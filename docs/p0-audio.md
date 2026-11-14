# P0 — Audio + MIDI I/O verification report

**Workstream:** audio-io (task-1) · **Date:** 2026-11-14 · **Status: PASS**

Machine: MacBook Pro M1 (Asahi Linux, aarch64) · PipeWire 1.6.9 · Rust 1.100.0
Stack: cpal 0.18.2 (**ALSA host → pipewire-alsa bridge**; cpal's `default = []`
features mean the native PipeWire host is *not* compiled in), midir 0.11.1
(ALSA-seq), rtrb 0.4.1. A browser was playing audio during all tests —
real-world desktop load, not an idle machine.

## 1. Devices found

- **Default output:** `Default Audio Device` (ALSA `default` → PipeWire) →
  **Kiwi Ears AD1 Pro** USB DAC, 48 000 Hz, 2 ch, F32.
  Supported buffer range: 16..=262144 frames.
- **Default input:** `Default Audio Device` → PipeWire source
  `omarchy_asahi_mic` (Asahi mic filter-chain), 48 000 Hz, 2 ch, F32.
- ALSA cards: `AppleJ293HPAI` (aop_audio, capture-only), `AppleJ293`
  (macbook speakers/mic), `Pro` (Kiwi Ears AD1 Pro USB).
- cpal enumerates 19 output / 15 input PCMs (ALSA hints + `hw:`/`plughw:`).
- PipeWire graph: quantum 1024 at idle (browser), min 32 / max 2048;
  adapts down when low-latency clients appear (observed 128 during soak,
  driven by another app; our stream kept its requested 512).

## 2. Buffer-size sweep (output sine, 5 s each, 48 kHz)

| Buffer | Effective frames/cb | Period | cb avg | cb max | cpal latency est. | Errors / xruns |
|-------:|--------------------:|-------:|-------:|-------:|------------------:|:--------------:|
| 128    | 128.0               | 2.67 ms  | 26.0 µs  | 467.9 µs | 13.15 ms | 0 / 0 |
| 256    | 256.0               | 5.33 ms  | 47.3 µs  | 747.1 µs | 15.94 ms | 0 / 0 |
| 512    | 512.0               | 10.67 ms | 96.9 µs  | 492.0 µs | 21.15 ms | 0 / 0 |
| 1024   | 1024.0              | 21.33 ms | 146.8 µs | 748.4 µs | 31.94 ms | 0 / 0 |

Requested `BufferSize::Fixed(n)` is honored exactly (avg frames/cb == n).
**Latency notes:** *buffer latency* = n / 48 kHz (2.67–21.33 ms). The *cpal
estimate* (playback − callback timestamp from `snd_pcm_status`) includes the
pipewire-alsa bridge buffering; at 128 it was inflated (13.15 ms) because the
graph quantum was still 1024 from the browser at that moment. These are
buffer-derived estimates, **not** a measured round-trip; true RTL measurement
(impulse loopback) is a P1 item.

## 3. 60 s soak @ 512 frames — acceptance run

```
callbacks: 5631 · frames: 2 883 072 · avg frames/cb: 512.0
callback duration: avg 96.7 µs, max 2955.8 µs
deadline misses (> 10.67 ms period): 0
stream errors: 0 · xruns (cpal ErrorKind::Xrun): 0
```

Duration histogram (µs): 16–32: 1.9% · 32–64: 5.6% · **64–128: 80.9%** ·
128–256: 9.6% · 256–512: 1.2% · 512–1024: 0.5% · 1024–2048: 0.3% ·
2048–4096: 2 callbacks (0.04%) · >4096: 0. Worst callback used 28% of the
period; the two >2 ms outliers are consistent with desktop scheduling noise.

**PipeWire-side cross-check** (`pw-top -b` sampled 14× during the soak):
node `alsa_playback.kabelsalat` ran at QUANT 512 / 48 000 Hz with **ERR = 0 in
every sample**; the DAC sink node also showed ERR = 0 in all 14 samples.
cpal and PipeWire agree: **zero xruns**.

## 4. Duplex (input + output simultaneously, 512 frames, 10 s)

- Output: 944 callbacks, avg 85.2 µs, max 2401.6 µs — 0 errors / 0 xruns.
- Input: 935 callbacks, avg 14.6 µs, max 127.8 µs — 0 errors / 0 xruns.
- **Input peak 36.4 (+31 dBFS):** the default source is the Asahi mic
  *filter-chain*, whose float output legitimately exceeds 1.0 (mic gain).
  Anything consuming this source must clamp/normalize — the loopback path
  clamps to [-1, 1] at gain 0.25 for that reason.
- **Loopback variant** (`KSALAT_LOOPBACK=1`, 3 s): input → preallocated rtrb
  ring (4096 samples) → output mix. 0 samples dropped (ring full), 6144
  output slots starved — all in the initial fill before the first input
  callback, none afterwards. 0 xruns. Per-sample `push`/`pop` raised the
  output callback to avg 303.5 µs (max 4552 µs), still ≪ 10.67 ms period;
  chunked ring copies are the P1 optimization.

## 5. MIDI (midir / ALSA-seq)

- Ports found: `Midi Through 14:0`, `vibez-midi-in:vibez-midi-listener 128:0`
  (another app). **No hardware controller attached** (PLAN §11 open item).
- Virtual input port **`kabelsalat-in`** created (client `kabelsalat`, port 0).
- **Self-test (second midir client in-process):** 8/8 messages received
  (C-major arpeggio note on/off ×3, CC7, pitch bend), 0 dropped; ALSA-seq
  timestamps monotonic and matching the send gaps (120/30 ms).
- **External test (`aplaymidi -p 129:0 test.mid`):** 6/6 events received,
  note offs exactly 500 ms after note ons per the file tempo — sample-accurate
  ALSA-seq delivery, 0 dropped.
- Events cross the thread boundary as fixed-size records in a preallocated
  rtrb ring (512 slots); decoding/logging happens off the callback thread.

## 6. Real-time discipline (PLAN §7)

Callbacks are allocation-free, lock-free, log-free and syscall-free:
sine phase + atomics only; timing via `Instant::now()` (CLOCK_MONOTONIC through
vDSO on aarch64 — no syscall); statistics in a fixed 12-bucket atomic
histogram; xruns counted from cpal's error callback (`ErrorKind::Xrun`) without
logging; duplex handoff via preallocated rtrb rings. All reporting is on the
main thread.

## 7. Reproduce

```sh
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR=/tmp/ksalat-audio-target   # avoid shared-build lock
cargo run -- audio-test                # quick: 512 frames, 3 s
cargo run -- audio-test 128 5          # buffer sweep: 128/256/512/1024
cargo run -- audio-test 512 60         # the acceptance soak
KSALAT_DUPLEX=1 cargo run -- audio-test 512 10
KSALAT_LOOPBACK=1 cargo run -- audio-test 512 3   # opt-in, clamped low gain
cargo run -- midi-test                 # self-send verification
KSALAT_MIDI_NO_SELFSEND=1 KSALAT_MIDI_LISTEN_SECS=8 cargo run -- midi-test
#   then from another shell: aplaymidi -p <client:port> song.mid
```

Env overrides: `KSALAT_BUFFER`, `KSALAT_SECS`, `KSALAT_FREQ`,
`KSALAT_MIDI_LISTEN_SECS`. Raw logs: `/tmp/ksalat-p0/*.log`.

## 8. Notes for integration

- The original spike targeted the pre-0.18 cpal API; updated to 0.18.2
  (`Device::description()`, `StreamConfig` by value, `SampleRate = u32`,
  `ErrorKind::Xrun`). No Cargo.toml changes were needed.
- cpal 0.18 ships a native PipeWire host behind the `pipewire` feature
  (not enabled here). Worth evaluating in P1 for lower bridge latency and
  `pw_stream_get_time_n` timestamps; the ALSA path is verified and sufficient
  for the P0 gate.
- During final verification the shared build was temporarily broken by
  in-progress edits in `src/host.rs` (clap-host workstream, outside this
  task's scope); audio.rs/midi.rs were additionally compile- and run-verified
  in an isolated scratch crate with identical dependency versions.