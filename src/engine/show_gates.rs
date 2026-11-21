//! Show-level gates (#273): which aux buses take signal and which bindings
//! work in the active part, as the setlist's levels decide
//! (`Setlist::gates`). Fixed tables: nothing here allocates.

use super::MAX_BUSES;
use crate::control::NodeId;
use crate::matrix::SourceId;

/// Pinned bindings the engine tracks; one past it keeps its default.
pub const MAX_BINDING_GATES: usize = 64;

const NO_BUS: (NodeId, f32) = (NodeId::MAX, 1.0);

pub(super) struct ShowGates {
    closed_buses: [NodeId; MAX_BUSES],
    /// Input gain of each bus that is closed or still fading (others: 1).
    bus_gain: [(NodeId, f32); MAX_BUSES],
    bindings: [(SourceId, NodeId, u8, bool); MAX_BINDING_GATES],
    n_bindings: usize,
}

impl ShowGates {
    pub fn new() -> Self {
        Self {
            closed_buses: [NodeId::MAX; MAX_BUSES],
            bus_gain: [NO_BUS; MAX_BUSES],
            bindings: [(SourceId::Keys, NodeId::MAX, 0, true); MAX_BINDING_GATES],
            n_bindings: 0,
        }
    }

    /// Every bus open and every binding at its default; a bus that was
    /// closed fades back in.
    pub fn clear(&mut self) {
        self.closed_buses = [NodeId::MAX; MAX_BUSES];
        self.n_bindings = 0;
    }

    /// A graph load: as `clear`, with no fade (the buses are new).
    pub fn reset(&mut self) {
        self.clear();
        self.bus_gain = [NO_BUS; MAX_BUSES];
    }

    /// A removed bus: its slots are free again.
    pub fn forget_bus(&mut self, bus: NodeId) {
        for b in self.closed_buses.iter_mut().filter(|b| **b == bus) {
            *b = NodeId::MAX;
        }
        for g in self.bus_gain.iter_mut().filter(|g| g.0 == bus) {
            *g = NO_BUS;
        }
    }

    pub fn set_bus(&mut self, bus: NodeId, live: bool) {
        let at = self.closed_buses.iter().position(|&b| b == bus);
        match (at, live) {
            (Some(i), true) => self.closed_buses[i] = NodeId::MAX,
            (None, false) => {
                if let Some(free) = self.closed_buses.iter_mut().find(|b| **b == NodeId::MAX) {
                    *free = bus;
                }
            }
            _ => {}
        }
    }

    pub fn set_binding(&mut self, source: SourceId, dest: NodeId, cc: u8, live: bool) {
        let n = self.n_bindings;
        match self.bindings[..n].iter_mut().find(|b| b.0 == source && b.1 == dest && b.2 == cc) {
            Some(b) => b.3 = live,
            None if n < MAX_BINDING_GATES => {
                self.bindings[n] = (source, dest, cc, live);
                self.n_bindings += 1;
            }
            None => {}
        }
    }

    /// `Some(live)` for a binding the show pinned, `None` for the default.
    pub fn binding(&self, source: SourceId, dest: NodeId, cc: u8) -> Option<bool> {
        self.bindings[..self.n_bindings]
            .iter()
            .find(|b| b.0 == source && b.1 == dest && b.2 == cc)
            .map(|b| b.3)
    }

    /// Gate one block of `bus`'s input (stereo interleaved): it fades from
    /// the last block's gain to this block's over the block, so a bus that
    /// closes takes no new signal and makes no click.
    pub fn gate_bus_input(&mut self, bus: NodeId, buf: &mut [f32]) {
        let target = if self.closed_buses.contains(&bus) { 0.0 } else { 1.0 };
        let slot = self.bus_gain.iter().position(|g| g.0 == bus);
        let from = slot.map_or(1.0, |i| self.bus_gain[i].1);
        if from == 1.0 && target == 1.0 {
            return;
        }
        let frames = (buf.len() / 2).max(1);
        if from == target {
            buf.fill(0.0);
        } else {
            for (i, f) in buf.chunks_exact_mut(2).enumerate() {
                let g = from + (target - from) * (i + 1) as f32 / frames as f32;
                f[0] *= g;
                f[1] *= g;
            }
        }
        match (slot, target == 1.0) {
            (Some(i), true) => self.bus_gain[i] = NO_BUS,
            (Some(i), false) => self.bus_gain[i].1 = target,
            (None, _) => {
                if let Some(free) = self.bus_gain.iter_mut().find(|g| g.0 == NodeId::MAX) {
                    *free = (bus, target);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closing_bus_fades_out_over_one_block_and_back_in_over_the_next() {
        let mut g = ShowGates::new();
        let mut block = vec![1.0f32; 8];
        g.gate_bus_input(7, &mut block);
        assert_eq!(block, vec![1.0; 8], "an open bus passes its input untouched");
        g.set_bus(7, false);
        let mut block = vec![1.0f32; 8];
        g.gate_bus_input(7, &mut block);
        assert_eq!(block, vec![0.75, 0.75, 0.5, 0.5, 0.25, 0.25, 0.0, 0.0], "the input fades out over the block");
        let mut block = vec![1.0f32; 8];
        g.gate_bus_input(7, &mut block);
        assert_eq!(block, vec![0.0; 8], "a closed bus takes nothing");
        g.clear();
        let mut block = vec![1.0f32; 8];
        g.gate_bus_input(7, &mut block);
        assert_eq!(block, vec![0.25, 0.25, 0.5, 0.5, 0.75, 0.75, 1.0, 1.0], "reopened, it fades back in");
        let mut block = vec![1.0f32; 8];
        g.gate_bus_input(7, &mut block);
        assert_eq!(block, vec![1.0; 8]);
    }

    #[test]
    fn a_removed_bus_frees_its_slots() {
        let mut g = ShowGates::new();
        for bus in 0..MAX_BUSES as NodeId {
            g.set_bus(bus, false);
            g.gate_bus_input(bus, &mut [1.0; 2]);
        }
        g.forget_bus(3);
        g.set_bus(100, false);
        let mut block = vec![1.0f32; 2];
        g.gate_bus_input(100, &mut block);
        g.gate_bus_input(100, &mut block);
        assert_eq!(block, vec![0.0; 2], "the freed slot closes a new bus");
    }

    #[test]
    fn binding_gates_hold_one_entry_per_binding_until_cleared() {
        let mut g = ShowGates::new();
        assert_eq!(g.binding(SourceId::Hardware(0), 3, 11), None, "no entry: the default");
        g.set_binding(SourceId::Hardware(0), 3, 11, false);
        g.set_binding(SourceId::Hardware(0), 3, 11, true);
        assert_eq!(g.binding(SourceId::Hardware(0), 3, 11), Some(true));
        assert_eq!(g.binding(SourceId::Hardware(1), 3, 11), None, "another device is another binding");
        g.clear();
        assert_eq!(g.binding(SourceId::Hardware(0), 3, 11), None);
    }
}
