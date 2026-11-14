# P0 report — go/no-go

**Date:** 2026-11-14 · **Decision: GO for P1** · Commit baseline: `41c0bf5` (merged app)

## What was proven

One window on Asahi/ARM64 Linux: a Reason-style rack (Tab flip, physical animated
cables) playing **Surge XT through our own RT-safe CLAP host**, with laptop keys,
on-screen keys, auto-connected hardware MIDI, and a built-in DeepSeek assistant.

## Workstream results

### Audio/MIDI (task-1, audio-io) — details: p0-audio.md
- 60 s soak @ 512 frames / 48 kHz: **5631 callbacks, 2.88 M frames, 0 xruns, 0 errors**;
  callback avg 96.7 µs / max 2.96 ms (28% of period). pw-top cross-check: ERR=0.
- Buffer sweep 128/256/512/1024: all honored exactly, 0 xruns (2.67–21.33 ms buffer latency).
- Duplex in+out @512: 0 xruns; preallocated rtrb loopback: 0 drops.
- MIDI: virtual port 8/8 events; external aplaymidi 6/6 with exact 500 ms spacing.
- Callbacks alloc/lock/log/syscall-free (atomics + vDSO clock + rtrb).

### CLAP hosting (tasks 2+5, clap-host) — details: p0-host.md
- Surge XT 1.3.4 loads from ~/.clap; **775 params**; all extensions present.
- host-test through the ClapVoice API: audible arpeggio (peak 0.35), 930 callbacks,
  **0 underruns, 0 CLAP process errors**; state save/restore round-trip OK.
- **GUI embedded**: Surge creates its editor as a child of our x11rb window
  (CreateNotify + query_tree evidence — not a detached window).
- Plugin-reported latency: **0 samples**.
- ClapVoice API: RT-safe render (no alloc/locks, chunked at 256), lock-free note
  commands, main-thread state/GUI split documented (§3.1).

### Rack UI (task-3, ui-cables) — details: p0-ui.md
- Tab flip verified both directions; front/back pixel-checked at 2560×1600.
- Cable physics: damped springs (k=130, ζ≈0.3, 240 Hz substeps), distance-aware
  sag, grab-to-shake, tug-to-unplug, arc-length pulse travel; **zero per-frame
  heap** in the cable path.
- Native Wayland, native 1.6× scale (fractional-HiDPI rule: trust detection,
  fallback 1.6 only if ~1.0). Adaptive layout verified 1600×1000 and 781×950.

### Merge (task-4, lead)
- Single window verified (hyprctl: exactly 1 kabelsalat client); process ~8.6%
  CPU / 0.9% MEM with animation + audio + Surge running; no stream errors.
- Signal path: on-screen/laptop/hardware keys → `fire_keys` (LEDs + cable pulses)
  → NoteSink → ClapVoice controller → Surge → cpal (fixed 512-frame buffer).
- Assistant panel: BYOK (0600 local storage), tools get_status / play_note /
  set_octave / flip_rack; keyboard focus gating — typing never plays notes.
- Earlier preview dropouts fixed: fixed 512-frame buffer instead of device default.

## Latency picture (P0)

| Component | Value |
|---|---|
| Output buffer @48 kHz | 512 frames = 10.7 ms (configurable 128–1024) |
| Surge plugin latency | 0 samples |
| MIDI note quantization | ≤ 5.3 ms (256-frame blocks); sample-accurate via set_schedule |
| True round-trip | not yet measured — impulse measurement in P1 |

## Known limitations (P1 backlog)

- Rack is a fixed demo patch: visual rewiring does not yet change the audio graph.
- Knobs are visual; not yet mapped to Surge params (smart controls).
- No sessions/undo/templates yet; no embedded Surge editor in the rack window
  (mechanism proven in host-test); no system-audio capture; no MCP server yet;
  assistant toolset limited to preview-level actions.

## Decision

**GO.** All three feasibility risks (RT audio on PipeWire/ARM, CLAP hosting with
embedded GUI, physics-cable UI at 60fps) are resolved with evidence. P1 builds the
real graph behind the rack via the shared control plane.
