### Added
- **Akai EWI 4000s and EWI 5000 instrument profiles:** built-in MIDI Guide
  profiles (`instruments/akai-ewi-4000s.csv`, `instruments/akai-ewi-5000.csv`)
  covering breath (CC 2, plus CC 34/43 Hi-Res LSB on EWI 5000), glide
  (CC 5 / CC 65, plus CC 104 legato time on EWI 5000), hold/sostenuto
  (CC 64 / CC 66), modulation, expression and filter CCs. Both profiles bind
  CC 2 breath to `CcTarget::ChannelPressure` on CLAP and LV2 synths so breath
  drives both CC 2 and aftertouch wind patches while leaving glide, sustain
  and other performance CCs passing through. The EWI 5000 auto-matches its
  USB MIDI port (`EWI5000`).
