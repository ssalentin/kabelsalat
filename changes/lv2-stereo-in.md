### Fixed
- **Stereo LV2 effects keep the right channel:** every input of an LV2
  effect was fed the left channel, so a stereo effect played L in both
  sides. Inputs now take left and right in order; mono effects still take
  the left channel.
