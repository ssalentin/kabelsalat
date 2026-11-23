#![allow(non_snake_case)]
//! COM host objects implementing the Steinberg VST3 host interfaces.
//!
//! Includes `IHostApplication`, `IMessage`, `IAttributeList`, `IBStream`,
//! `IComponentHandler`, zero-alloc audio-thread `IEventList` + `IParameterChanges`,
//! and `IPlugFrame` + `Linux::IRunLoop`.

use std::cell::UnsafeCell;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::os::raw::c_void;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use vst3::Steinberg::Linux::{
    FileDescriptor, IEventHandler, IEventHandlerTrait, IRunLoop, IRunLoopTrait, ITimerHandler,
    ITimerHandlerTrait, TimerInterval,
};
use vst3::Steinberg::Vst::{
    Event, IAttributeList, IAttributeListTrait, IAttributeList_::AttrID, IComponentHandler,
    IComponentHandlerTrait, IEventList, IEventListTrait, IHostApplication, IHostApplicationTrait,
    IMessage, IMessageTrait, IParamValueQueue, IParamValueQueueTrait, IParameterChanges,
    IParameterChangesTrait, ParamID, ParamValue, String128, TChar,
};
use vst3::Steinberg::{
    FIDString, FUnknown, IBStream, IBStreamTrait, IBStream_::IStreamSeekMode_, IPlugFrame,
    IPlugFrameTrait, IPlugView, TUID, ViewRect, char16, int32, int64, kInvalidArgument,
    kNoInterface, kOutOfMemory, kResultFalse, kResultOk, tresult, uint32,
};
use vst3::{Class, ComPtr, ComRef, ComWrapper, Interface};

use super::scan::cstr_slice_to_string;

pub const EVENT_LIST_CAP: usize = 256;
pub const PARAM_QUEUE_CAP: usize = 16;
pub const PARAM_CHANGES_SLOTS: usize = 64;

/// Write a Rust `&str` as null-terminated UTF-16 into `out`.
pub fn str_to_tchar_slice(s: &str, out: &mut [char16]) {
    if out.is_empty() {
        return;
    }
    let mut i = 0;
    for unit in s.encode_utf16() {
        if i + 1 >= out.len() {
            break;
        }
        out[i] = unit as char16;
        i += 1;
    }
    out[i] = 0;
}

/// Write a Rust `&str` into a fixed VST3 `String128`.
pub fn str_to_string128(s: &str) -> String128 {
    let mut out = [0 as TChar; 128];
    str_to_tchar_slice(s, &mut out);
    out
}

/// Convert a null-terminated `String128` to `String`.
pub fn string128_to_string(s: &String128) -> String {
    let len = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    let units: Vec<u16> = s[..len].iter().map(|&c| c as u16).collect();
    String::from_utf16_lossy(&units)
}

/// Convert a VST3 `TChar*` UTF-16 C string to `String` (bounded to 4096 code units).
pub unsafe fn utf16_ptr_to_string(ptr: *const char16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut units = Vec::new();
    for i in 0..4096 {
        let u = *ptr.add(i);
        if u == 0 {
            break;
        }
        units.push(u as u16);
    }
    String::from_utf16_lossy(&units)
}

fn cstr_ptr_to_string(ptr: *const std::os::raw::c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    Some(unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------
// IAttributeList + IMessage + IHostApplication
// ---------------------------------------------------------------------------

enum AttrVal {
    Int(i64),
    Float(f64),
    Str(Box<[u16]>),
    Bin(Box<[u8]>),
}

pub struct HostAttributeList {
    map: Mutex<HashMap<String, AttrVal>>,
}

impl HostAttributeList {
    pub fn new() -> Self {
        Self { map: Mutex::new(HashMap::new()) }
    }
}

impl Class for HostAttributeList {
    type Interfaces = (IAttributeList,);
}

impl IAttributeListTrait for HostAttributeList {
    unsafe fn setInt(&self, id: AttrID, value: int64) -> tresult {
        let Some(k) = cstr_ptr_to_string(id) else { return kInvalidArgument };
        self.map.lock().unwrap().insert(k, AttrVal::Int(value));
        kResultOk
    }

    unsafe fn getInt(&self, id: AttrID, value: *mut int64) -> tresult {
        if value.is_null() {
            return kInvalidArgument;
        }
        let Some(k) = cstr_ptr_to_string(id) else { return kInvalidArgument };
        if let Some(AttrVal::Int(v)) = self.map.lock().unwrap().get(&k) {
            *value = *v;
            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn setFloat(&self, id: AttrID, value: f64) -> tresult {
        let Some(k) = cstr_ptr_to_string(id) else { return kInvalidArgument };
        self.map.lock().unwrap().insert(k, AttrVal::Float(value));
        kResultOk
    }

    unsafe fn getFloat(&self, id: AttrID, value: *mut f64) -> tresult {
        if value.is_null() {
            return kInvalidArgument;
        }
        let Some(k) = cstr_ptr_to_string(id) else { return kInvalidArgument };
        if let Some(AttrVal::Float(v)) = self.map.lock().unwrap().get(&k) {
            *value = *v;
            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn setString(&self, id: AttrID, string: *const TChar) -> tresult {
        if string.is_null() {
            return kInvalidArgument;
        }
        let Some(k) = cstr_ptr_to_string(id) else { return kInvalidArgument };
        let s = utf16_ptr_to_string(string);
        let mut units: Vec<u16> = s.encode_utf16().collect();
        units.push(0);
        self.map.lock().unwrap().insert(k, AttrVal::Str(units.into_boxed_slice()));
        kResultOk
    }

    unsafe fn getString(&self, id: AttrID, string: *mut TChar, sizeInBytes: uint32) -> tresult {
        if string.is_null() || sizeInBytes < 2 {
            return kInvalidArgument;
        }
        let Some(k) = cstr_ptr_to_string(id) else { return kInvalidArgument };
        let guard = self.map.lock().unwrap();
        let Some(AttrVal::Str(units)) = guard.get(&k) else {
            return kResultFalse;
        };
        let cap = (sizeInBytes as usize) / std::mem::size_of::<TChar>();
        let copy = units.len().min(cap);
        for (i, &u) in units[..copy].iter().enumerate() {
            *string.add(i) = u as TChar;
        }
        if copy > 0 && units[copy - 1] != 0 {
            *string.add(copy - 1) = 0;
        }
        kResultOk
    }

    unsafe fn setBinary(&self, id: AttrID, data: *const c_void, sizeInBytes: uint32) -> tresult {
        let Some(k) = cstr_ptr_to_string(id) else { return kInvalidArgument };
        let bytes = if data.is_null() || sizeInBytes == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(data as *const u8, sizeInBytes as usize).to_vec()
        };
        self.map.lock().unwrap().insert(k, AttrVal::Bin(bytes.into_boxed_slice()));
        kResultOk
    }

    unsafe fn getBinary(&self, id: AttrID, data: *mut *const c_void, sizeInBytes: *mut uint32) -> tresult {
        if data.is_null() || sizeInBytes.is_null() {
            return kInvalidArgument;
        }
        let Some(k) = cstr_ptr_to_string(id) else { return kInvalidArgument };
        let guard = self.map.lock().unwrap();
        let Some(AttrVal::Bin(bytes)) = guard.get(&k) else {
            return kResultFalse;
        };
        // Safety: `Box<[u8]>` address stays pinned even if other keys resize the HashMap.
        *data = bytes.as_ptr() as *const c_void;
        *sizeInBytes = bytes.len() as uint32;
        kResultOk
    }
}

pub struct HostMessage {
    id: Mutex<CString>,
    attrs: ComWrapper<HostAttributeList>,
}

impl HostMessage {
    pub fn new() -> Self {
        Self {
            id: Mutex::new(CString::default()),
            attrs: ComWrapper::new(HostAttributeList::new()),
        }
    }
}

impl Class for HostMessage {
    type Interfaces = (IMessage,);
}

impl IMessageTrait for HostMessage {
    unsafe fn getMessageID(&self) -> FIDString {
        // VST3 plugins read getMessageID synchronously while the message stays unchanged.
        let guard = self.id.lock().unwrap();
        guard.as_ptr()
    }

    unsafe fn setMessageID(&self, id: FIDString) {
        if !id.is_null() {
            let s = CStr::from_ptr(id).to_owned();
            *self.id.lock().unwrap() = s;
        }
    }

    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        self.attrs
            .as_com_ref::<IAttributeList>()
            .map(|r| r.as_ptr())
            .unwrap_or(std::ptr::null_mut())
    }
}

pub struct HostApplication;

impl HostApplication {
    pub fn new() -> Self {
        Self
    }
}

impl Class for HostApplication {
    type Interfaces = (IHostApplication,);
}

fn tuid_bytes(tuid: &TUID) -> [u8; 16] {
    let mut b = [0u8; 16];
    for i in 0..16 {
        b[i] = tuid[i] as u8;
    }
    b
}

impl IHostApplicationTrait for HostApplication {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        if name.is_null() {
            return kInvalidArgument;
        }
        *name = str_to_string128("Kabelsalat");
        kResultOk
    }

    unsafe fn createInstance(&self, cid: *mut TUID, iid: *mut TUID, obj: *mut *mut c_void) -> tresult {
        if cid.is_null() || iid.is_null() || obj.is_null() {
            return kInvalidArgument;
        }
        *obj = std::ptr::null_mut();
        let cid_g = tuid_bytes(&*cid);
        let iid_g = tuid_bytes(&*iid);
        if cid_g == IMessage::IID {
            if iid_g == IMessage::IID || iid_g == FUnknown::IID {
                if let Some(ptr) = ComWrapper::new(HostMessage::new()).to_com_ptr::<IMessage>() {
                    *obj = ptr.into_raw() as *mut c_void;
                    return kResultOk;
                }
            }
        } else if cid_g == IAttributeList::IID {
            if iid_g == IAttributeList::IID || iid_g == FUnknown::IID {
                if let Some(ptr) = ComWrapper::new(HostAttributeList::new()).to_com_ptr::<IAttributeList>() {
                    *obj = ptr.into_raw() as *mut c_void;
                    return kResultOk;
                }
            }
        }
        kNoInterface
    }
}

// ---------------------------------------------------------------------------
// IBStream
// ---------------------------------------------------------------------------

struct StreamInner {
    buf: Vec<u8>,
    pos: usize,
}

pub struct MemoryStream {
    inner: Mutex<StreamInner>,
}

impl MemoryStream {
    pub fn new() -> Self {
        Self { inner: Mutex::new(StreamInner { buf: Vec::new(), pos: 0 }) }
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self { inner: Mutex::new(StreamInner { buf: bytes, pos: 0 }) }
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.inner.lock().unwrap().buf.clone()
    }
}

impl Class for MemoryStream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(&self, buffer: *mut c_void, numBytes: int32, numBytesRead: *mut int32) -> tresult {
        if numBytes < 0 || (numBytes > 0 && buffer.is_null()) {
            return kInvalidArgument;
        }
        let mut st = self.inner.lock().unwrap();
        let avail = st.buf.len().saturating_sub(st.pos);
        let take = avail.min(numBytes as usize);
        if take > 0 {
            std::ptr::copy_nonoverlapping(st.buf[st.pos..].as_ptr(), buffer as *mut u8, take);
            st.pos += take;
        }
        if !numBytesRead.is_null() {
            *numBytesRead = take as int32;
        }
        kResultOk
    }

    unsafe fn write(&self, buffer: *mut c_void, numBytes: int32, numBytesWritten: *mut int32) -> tresult {
        if numBytes < 0 || (numBytes > 0 && buffer.is_null()) {
            return kInvalidArgument;
        }
        let mut st = self.inner.lock().unwrap();
        let n = numBytes as usize;
        if n > 0 {
            let end = st.pos.saturating_add(n);
            if end > st.buf.len() {
                st.buf.resize(end, 0);
            }
            let pos = st.pos;
            let src = std::slice::from_raw_parts(buffer as *const u8, n);
            st.buf[pos..pos + n].copy_from_slice(src);
            st.pos = end;
        }
        if !numBytesWritten.is_null() {
            *numBytesWritten = n as int32;
        }
        kResultOk
    }

    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        let mut st = self.inner.lock().unwrap();
        let base: i128 = match mode as u32 {
            IStreamSeekMode_::kIBSeekSet => 0,
            IStreamSeekMode_::kIBSeekCur => st.pos as i128,
            IStreamSeekMode_::kIBSeekEnd => st.buf.len() as i128,
            _ => return kInvalidArgument,
        };
        let target = base + pos as i128;
        if target < 0 || target > usize::MAX as i128 {
            return kInvalidArgument;
        }
        st.pos = target as usize;
        if !result.is_null() {
            *result = st.pos as int64;
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if pos.is_null() {
            return kInvalidArgument;
        }
        let st = self.inner.lock().unwrap();
        *pos = st.pos as int64;
        kResultOk
    }
}

// ---------------------------------------------------------------------------
// IComponentHandler
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ComponentHandlerState {
    edits: Vec<(u32, f64)>,
    restart_flags: i32,
}

pub struct HostComponentHandler {
    state: Mutex<ComponentHandlerState>,
}

impl HostComponentHandler {
    pub fn new() -> Self {
        Self { state: Mutex::new(ComponentHandlerState::default()) }
    }

    pub fn take(&self) -> (Vec<(u32, f64)>, i32) {
        let mut st = self.state.lock().unwrap();
        let edits = std::mem::take(&mut st.edits);
        let flags = std::mem::take(&mut st.restart_flags);
        (edits, flags)
    }
}

impl Class for HostComponentHandler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for HostComponentHandler {
    unsafe fn beginEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn performEdit(&self, id: ParamID, valueNormalized: ParamValue) -> tresult {
        let v = if valueNormalized.is_finite() {
            valueNormalized.clamp(0.0, 1.0)
        } else {
            return kInvalidArgument;
        };
        let mut st = self.state.lock().unwrap();
        if let Some(last) = st.edits.last_mut().filter(|(eid, _)| *eid == id) {
            last.1 = v;
        } else {
            st.edits.push((id, v));
        }
        kResultOk
    }

    unsafe fn endEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn restartComponent(&self, flags: int32) -> tresult {
        self.state.lock().unwrap().restart_flags |= flags;
        kResultOk
    }
}

// ---------------------------------------------------------------------------
// Audio-thread lock-free / zero-alloc IEventList & IParameterChanges
// ---------------------------------------------------------------------------

/// Preallocated event list used solely on the audio thread during one `process()` call.
pub struct HostEventList {
    events: UnsafeCell<Vec<Event>>,
}

// SAFETY: only mutated & read on the audio thread that exclusively owns `Vst3Voice`.
unsafe impl Send for HostEventList {}
unsafe impl Sync for HostEventList {}

impl HostEventList {
    pub fn new(capacity: usize) -> Self {
        Self { events: UnsafeCell::new(Vec::with_capacity(capacity)) }
    }

    /// Clear events without releasing capacity. Audio thread only.
    pub unsafe fn clear(&self) {
        (*self.events.get()).clear();
    }

    /// Append an event if within preallocated capacity (zero allocation). Audio thread only.
    pub unsafe fn push(&self, ev: Event) -> bool {
        let v = &mut *self.events.get();
        if v.len() >= v.capacity() {
            return false;
        }
        v.push(ev);
        true
    }
}

impl Class for HostEventList {
    type Interfaces = (IEventList,);
}

impl IEventListTrait for HostEventList {
    unsafe fn getEventCount(&self) -> int32 {
        (*self.events.get()).len() as int32
    }

    unsafe fn getEvent(&self, index: int32, e: *mut Event) -> tresult {
        let v = &*self.events.get();
        if e.is_null() || index < 0 || (index as usize) >= v.len() {
            return kInvalidArgument;
        }
        *e = v[index as usize];
        kResultOk
    }

    unsafe fn addEvent(&self, e: *mut Event) -> tresult {
        if e.is_null() {
            return kInvalidArgument;
        }
        if self.push(*e) {
            kResultOk
        } else {
            kOutOfMemory
        }
    }
}

struct ParamQueueInner {
    id: ParamID,
    points: Vec<(i32, f64)>,
}

/// Preallocated `IParamValueQueue` reused across blocks on the audio thread.
pub struct HostParamValueQueue {
    inner: UnsafeCell<ParamQueueInner>,
}

// SAFETY: only accessed by the single audio thread owning its parent `HostParamChanges`.
unsafe impl Send for HostParamValueQueue {}
unsafe impl Sync for HostParamValueQueue {}

impl HostParamValueQueue {
    fn new(capacity: usize) -> Self {
        Self {
            inner: UnsafeCell::new(ParamQueueInner {
                id: 0,
                points: Vec::with_capacity(capacity),
            }),
        }
    }

    unsafe fn reset(&self, id: ParamID) {
        let st = &mut *self.inner.get();
        st.id = id;
        st.points.clear();
    }

    pub unsafe fn push_point(&self, sample_offset: i32, value: f64) -> Option<i32> {
        let st = &mut *self.inner.get();
        if st.points.len() >= st.points.capacity() {
            return None;
        }
        let idx = st.points.len() as i32;
        st.points.push((sample_offset, value));
        Some(idx)
    }

    pub unsafe fn last_point(&self) -> Option<(ParamID, f64)> {
        let st = &*self.inner.get();
        st.points.last().map(|&(_, v)| (st.id, v))
    }
}

impl Class for HostParamValueQueue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for HostParamValueQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        (*self.inner.get()).id
    }

    unsafe fn getPointCount(&self) -> int32 {
        (*self.inner.get()).points.len() as int32
    }

    unsafe fn getPoint(&self, index: int32, sampleOffset: *mut int32, value: *mut ParamValue) -> tresult {
        if sampleOffset.is_null() || value.is_null() {
            return kInvalidArgument;
        }
        let st = &*self.inner.get();
        if index < 0 || (index as usize) >= st.points.len() {
            return kInvalidArgument;
        }
        let (off, val) = st.points[index as usize];
        *sampleOffset = off;
        *value = val;
        kResultOk
    }

    unsafe fn addPoint(&self, sampleOffset: int32, value: ParamValue, index: *mut int32) -> tresult {
        match self.push_point(sampleOffset, value) {
            Some(idx) => {
                if !index.is_null() {
                    *index = idx;
                }
                kResultOk
            }
            None => kOutOfMemory,
        }
    }
}

struct ParamChangesInner {
    queues: Vec<ComWrapper<HostParamValueQueue>>,
    active: usize,
}

/// Preallocated fixed-pool `IParameterChanges`: zero heap allocations per audio block.
pub struct HostParamChanges {
    inner: UnsafeCell<ParamChangesInner>,
}

// SAFETY: exclusively used by the single audio thread owning `Vst3Voice`.
unsafe impl Send for HostParamChanges {}
unsafe impl Sync for HostParamChanges {}

impl HostParamChanges {
    pub fn new(slots: usize, points_per_slot: usize) -> Self {
        let queues = (0..slots)
            .map(|_| ComWrapper::new(HostParamValueQueue::new(points_per_slot)))
            .collect();
        Self { inner: UnsafeCell::new(ParamChangesInner { queues, active: 0 }) }
    }

    pub unsafe fn clear(&self) {
        let st = &mut *self.inner.get();
        st.active = 0;
    }

    pub unsafe fn add_point(&self, id: ParamID, sample_offset: i32, value: f64) {
        let st = &mut *self.inner.get();
        for i in 0..st.active {
            if st.queues[i].getParameterId() == id {
                let _ = st.queues[i].push_point(sample_offset, value);
                return;
            }
        }
        if st.active < st.queues.len() {
            let idx = st.active;
            st.queues[idx].reset(id);
            let _ = st.queues[idx].push_point(sample_offset, value);
            st.active += 1;
        }
    }

    pub unsafe fn drain_last_points<F: FnMut(u32, f64)>(&self, mut f: F) {
        let st = &*self.inner.get();
        for i in 0..st.active {
            if let Some((id, val)) = st.queues[i].last_point() {
                f(id, val);
            }
        }
    }
}

impl Class for HostParamChanges {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for HostParamChanges {
    unsafe fn getParameterCount(&self) -> int32 {
        (*self.inner.get()).active as int32
    }

    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        let st = &*self.inner.get();
        if index < 0 || (index as usize) >= st.active {
            return std::ptr::null_mut();
        }
        st.queues[index as usize]
            .as_com_ref::<IParamValueQueue>()
            .map(|r| r.as_ptr())
            .unwrap_or(std::ptr::null_mut())
    }

    unsafe fn addParameterData(&self, id: *const ParamID, index: *mut int32) -> *mut IParamValueQueue {
        if id.is_null() {
            return std::ptr::null_mut();
        }
        let target = *id;
        let st = &mut *self.inner.get();
        for i in 0..st.active {
            if st.queues[i].getParameterId() == target {
                if !index.is_null() {
                    *index = i as int32;
                }
                return st.queues[i]
                    .as_com_ref::<IParamValueQueue>()
                    .map(|r| r.as_ptr())
                    .unwrap_or(std::ptr::null_mut());
            }
        }
        if st.active >= st.queues.len() {
            return std::ptr::null_mut();
        }
        let idx = st.active;
        st.queues[idx].reset(target);
        st.active += 1;
        if !index.is_null() {
            *index = idx as int32;
        }
        st.queues[idx]
            .as_com_ref::<IParamValueQueue>()
            .map(|r| r.as_ptr())
            .unwrap_or(std::ptr::null_mut())
    }
}

// ---------------------------------------------------------------------------
// IPlugFrame + Linux::IRunLoop
// ---------------------------------------------------------------------------

struct FdEntry {
    fd: FileDescriptor,
    raw_ptr: usize,
    handler: ComPtr<IEventHandler>,
}

struct TimerEntry {
    raw_ptr: usize,
    interval: Duration,
    next_fire: Instant,
    handler: ComPtr<ITimerHandler>,
}

struct PlugFrameState {
    pending_resize: Option<(u32, u32)>,
    fds: Vec<FdEntry>,
    timers: Vec<TimerEntry>,
}

pub struct HostPlugFrame {
    state: Mutex<PlugFrameState>,
}

impl HostPlugFrame {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(PlugFrameState {
                pending_resize: None,
                fds: Vec::new(),
                timers: Vec::new(),
            }),
        }
    }

    pub fn take_resize(&self) -> Option<(u32, u32)> {
        self.state.lock().unwrap().pending_resize.take()
    }

    /// Drive registered Linux FDs (non-blocking `poll`) and timers outside the
    /// mutex so callbacks may re-entrantly register/unregister handlers.
    pub fn pump_run_loop(&self) {
        let fd_snapshot: Vec<(FileDescriptor, ComPtr<IEventHandler>)>;
        let due_timers: Vec<ComPtr<ITimerHandler>>;
        {
            let now = Instant::now();
            let mut st = self.state.lock().unwrap();
            fd_snapshot = st
                .fds
                .iter()
                .map(|e| (e.fd, e.handler.clone()))
                .collect();
            let mut due = Vec::new();
            for t in &mut st.timers {
                if now >= t.next_fire {
                    due.push(t.handler.clone());
                    let step = t.interval.max(Duration::from_millis(1));
                    while t.next_fire <= now {
                        t.next_fire += step;
                    }
                }
            }
            due_timers = due;
        }

        if !fd_snapshot.is_empty() {
            let mut pollfds: Vec<libc::pollfd> = fd_snapshot
                .iter()
                .map(|(fd, _)| libc::pollfd { fd: *fd, events: libc::POLLIN, revents: 0 })
                .collect();
            let r = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, 0) };
            if r > 0 {
                let mut seen = HashSet::new();
                for (pfd, (fd, handler)) in pollfds.iter().zip(fd_snapshot.iter()) {
                    if pfd.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0
                        && seen.insert(*fd)
                    {
                        unsafe { handler.onFDIsSet(*fd) };
                    }
                }
            }
        }

        for timer in due_timers {
            unsafe { timer.onTimer() };
        }
    }

    pub fn clear_handlers(&self) {
        let mut st = self.state.lock().unwrap();
        st.fds.clear();
        st.timers.clear();
        st.pending_resize = None;
    }
}

impl Class for HostPlugFrame {
    type Interfaces = (IPlugFrame, IRunLoop);
}

impl IPlugFrameTrait for HostPlugFrame {
    unsafe fn resizeView(&self, view: *mut IPlugView, newSize: *mut ViewRect) -> tresult {
        if newSize.is_null() {
            return kInvalidArgument;
        }
        let w = ((*newSize).right - (*newSize).left).max(1) as u32;
        let h = ((*newSize).bottom - (*newSize).top).max(1) as u32;
        self.state.lock().unwrap().pending_resize = Some((w, h));
        if !view.is_null() {
            let _ = ((*(*view).vtbl).onSize)(view, newSize);
        }
        kResultOk
    }
}

impl IRunLoopTrait for HostPlugFrame {
    unsafe fn registerEventHandler(&self, handler: *mut IEventHandler, fd: FileDescriptor) -> tresult {
        let Some(href) = ComRef::from_raw(handler) else {
            return kInvalidArgument;
        };
        let ptr = href.to_com_ptr();
        let raw_ptr = handler as usize;
        let mut st = self.state.lock().unwrap();
        st.fds.retain(|e| e.raw_ptr != raw_ptr || e.fd != fd);
        st.fds.push(FdEntry { fd, raw_ptr, handler: ptr });
        kResultOk
    }

    unsafe fn unregisterEventHandler(&self, handler: *mut IEventHandler) -> tresult {
        let raw_ptr = handler as usize;
        let mut st = self.state.lock().unwrap();
        let before = st.fds.len();
        st.fds.retain(|e| e.raw_ptr != raw_ptr);
        if st.fds.len() < before { kResultOk } else { kResultFalse }
    }

    unsafe fn registerTimer(&self, handler: *mut ITimerHandler, milliseconds: TimerInterval) -> tresult {
        let Some(href) = ComRef::from_raw(handler) else {
            return kInvalidArgument;
        };
        let ptr = href.to_com_ptr();
        let raw_ptr = handler as usize;
        let interval = Duration::from_millis(milliseconds.max(1));
        let mut st = self.state.lock().unwrap();
        st.timers.retain(|t| t.raw_ptr != raw_ptr);
        st.timers.push(TimerEntry {
            raw_ptr,
            interval,
            next_fire: Instant::now() + interval,
            handler: ptr,
        });
        kResultOk
    }

    unsafe fn unregisterTimer(&self, handler: *mut ITimerHandler) -> tresult {
        let raw_ptr = handler as usize;
        let mut st = self.state.lock().unwrap();
        let before = st.timers.len();
        st.timers.retain(|t| t.raw_ptr != raw_ptr);
        if st.timers.len() < before { kResultOk } else { kResultFalse }
    }
}

/// Unused helper keepalive re-export for C-string parsing.
#[allow(dead_code)]
fn _keep_cstr_helper(bytes: &[std::os::raw::c_char]) -> String {
    cstr_slice_to_string(bytes)
}
