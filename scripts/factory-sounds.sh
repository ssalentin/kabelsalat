#!/usr/bin/env bash
# Rebuild assets/factory/ from the FreePats sound banks (all CC0 1.0).
#
#   scripts/factory-sounds.sh [workdir]
#
# The factory sounds (#221) are a reduced copy of three FreePats banks: one
# velocity layer, every second root note, mono, 16 bit, a lower sample rate
# and a fade where the long decay is cut. That keeps what the repo and the
# binary carry near 2 MB; src/factory.rs holds the key maps. The full banks
# stay at their source. Needs git and ffmpeg. The work directory must not be
# on tmpfs (rule 79); default ~/.cache/ksalat-factory-sounds.
set -euo pipefail

here=$(cd "$(dirname "$0")/.." && pwd)
work=$(realpath -m "${1:-$HOME/.cache/ksalat-factory-sounds}")
out="$here/assets/factory"
mkdir -p "$work" "$out/piano" "$out/epiano" "$out/drums"

# Repository and the commit the shipped files were made from.
fetch() {
  local name=$1 commit=$2
  if [ ! -d "$work/$name/.git" ]; then
    git clone -q "https://github.com/freepats/$name.git" "$work/$name"
  fi
  git -C "$work/$name" checkout -q "$commit"
}
fetch upright-piano-KW 570f6c60ed2eff67accad3b85d5b452e57a3ad28
fetch fm-piano2 2e1241f5db3ff6b938b1645a2f2f82f7e510c708
fetch synthesizer-percussion 39bbabce8e0e10aa259d5d94fca329f668733185

# conv <in> <out> <rate> <seconds> <fade seconds>
# Mono, 16 bit, at most <seconds> long with a fade over the last <fade>.
conv() {
  local in=$1 dst=$2 rate=$3 len=$4 fade=$5
  local dur start
  dur=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$in")
  dur=$(awk -v d="$dur" -v l="$len" 'BEGIN { print (d < l) ? d : l }')
  start=$(awk -v d="$dur" -v f="$fade" 'BEGIN { s = d - f; print (s > 0) ? s : 0 }')
  ffmpeg -v error -y -i "$in" -ac 1 -ar "$rate" -t "$dur" \
    -af "afade=t=out:st=$start:d=$fade:curve=qsin" \
    -sample_fmt s16 -compression_level 12 -map_metadata -1 -bitexact "$dst"
}

# Piano: Upright Piano KW, the loud layer, a root every six semitones.
p="$work/upright-piano-KW/samples"
conv "$p/A0vH.flac"  "$out/piano/a0.flac"  24000 5.0 2.0
conv "$p/D#1vH.flac" "$out/piano/ds1.flac" 24000 5.0 2.0
conv "$p/A1vH.flac"  "$out/piano/a1.flac"  24000 5.0 2.0
conv "$p/D#2vH.flac" "$out/piano/ds2.flac" 32000 5.0 2.0
conv "$p/A2vL.flac"  "$out/piano/a2.flac"  32000 4.5 2.0
conv "$p/D#3vH.flac" "$out/piano/ds3.flac" 32000 4.5 2.0
conv "$p/A3vH.flac"  "$out/piano/a3.flac"  32000 4.0 1.5
conv "$p/D#4vH.flac" "$out/piano/ds4.flac" 32000 4.0 1.5
conv "$p/A4vH.flac"  "$out/piano/a4.flac"  32000 3.0 1.0
conv "$p/D#5vH.flac" "$out/piano/ds5.flac" 32000 3.0 1.0
conv "$p/A5vH.flac"  "$out/piano/a5.flac"  32000 2.5 0.8
conv "$p/D#6vH.flac" "$out/piano/ds6.flac" 32000 2.0 0.6
conv "$p/A6vH.flac"  "$out/piano/a6.flac"  32000 1.5 0.4
conv "$p/D#7vH.flac" "$out/piano/ds7.flac" 32000 1.2 0.3
conv "$p/A7vH.flac"  "$out/piano/a7.flac"  32000 1.0 0.2

# Electric piano: FM Piano 2, a root every twelve semitones from F#1.
e="$work/fm-piano2/samples"
conv "$e/F#1v80.flac" "$out/epiano/fs1.flac" 32000 4.0 2.0
conv "$e/F#2v80.flac" "$out/epiano/fs2.flac" 32000 4.0 2.0
conv "$e/C3v80.flac"  "$out/epiano/c3.flac"  32000 4.0 2.0
conv "$e/F#3v80.flac" "$out/epiano/fs3.flac" 32000 3.5 1.5
conv "$e/C4v80.flac"  "$out/epiano/c4.flac"  32000 3.5 1.5
conv "$e/F#4v80.flac" "$out/epiano/fs4.flac" 32000 3.0 1.5
conv "$e/C5v80.flac"  "$out/epiano/c5.flac"  32000 3.0 1.5
conv "$e/F#5v80.flac" "$out/epiano/fs5.flac" 32000 2.5 1.0
conv "$e/F#6v80.flac" "$out/epiano/fs6.flac" 32000 2.0 1.0

# Drum kit: Synthesizer percussion.
d="$work/synthesizer-percussion/samples"
conv "$d/Kick04.flac"           "$out/drums/kick.flac"       32000 1.0 0.05
conv "$d/Kick06.flac"           "$out/drums/kick2.flac"      32000 1.0 0.05
conv "$d/Snare09.flac"          "$out/drums/snare.flac"      44100 1.0 0.05
conv "$d/Snare14.flac"          "$out/drums/snare2.flac"     44100 1.0 0.05
conv "$d/Clap01.flac"           "$out/drums/clap.flac"       44100 1.0 0.05
conv "$d/Claves01.flac"         "$out/drums/claves.flac"     44100 1.0 0.05
conv "$d/ClosedHiHat01-01.flac" "$out/drums/hat_closed.flac" 44100 1.0 0.05
conv "$d/ClosedHiHat02-01.flac" "$out/drums/hat_pedal.flac"  44100 1.0 0.05
conv "$d/OpenHiHat02-01.flac"   "$out/drums/hat_open.flac"   44100 1.0 0.1
conv "$d/Cymbal01-01.flac"      "$out/drums/crash.flac"      44100 1.8 0.8
conv "$d/Cymbal02.flac"         "$out/drums/ride.flac"       44100 1.5 0.7
conv "$d/LowTom02-01.flac"      "$out/drums/tom_low.flac"    32000 1.0 0.05
conv "$d/MidTom02-01.flac"      "$out/drums/tom_mid.flac"    32000 1.0 0.05
conv "$d/HighTom02-01.flac"     "$out/drums/tom_high.flac"   32000 1.0 0.05
conv "$d/Shaker04.flac"         "$out/drums/shaker.flac"     44100 1.0 0.05

# The licence of every bank, as its repository ships it.
cp "$work/upright-piano-KW/LICENSE" "$out/piano/LICENSE"
cp "$work/fm-piano2/LICENSE.txt" "$out/epiano/LICENSE"
cp "$work/synthesizer-percussion/LICENSE" "$out/drums/LICENSE"

du -sh "$out"/* "$out"
