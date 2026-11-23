### Changed
- **Leaner engine and smaller binary:** the audio engine reads each audio
  input's ring in one chunk per block instead of sample by sample, takes a
  source's cable level once per block instead of once per cable, and finds
  what is wired into each master and bus chain in one pass over the cables.
  The rendered audio is identical to the sample; the engine uses about 6 %
  less CPU on a 4-rig rack with 8 audio inputs. HTTPS now goes through rustls
  on ring instead of aws-lc, which drops a large C library from the build.
