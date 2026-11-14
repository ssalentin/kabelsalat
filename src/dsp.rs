//! Built-in DSP node suite (task-7). RT-safe processors: no allocation,
//! locks, or syscalls inside `process`. Owned by the dsp workstream.

/// Common interface for built-in nodes; the engine wires these into the graph.
pub trait Processor: Send {
    /// Process `frames` stereo-interleaved frames in place.
    fn process(&mut self, io: &mut [f32], frames: usize);
    fn set_param(&mut self, id: u32, value: f32);
    fn param_count(&self) -> u32;
    fn param_name(&self, id: u32) -> &'static str;
}
