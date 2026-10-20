#!/bin/sh
# Regenerate the MP3 fixtures for crates/audio-io's lossless gain tests. Run from the repo root.
# Each is 2 s of a 440 Hz sine at -6 dBFS plus a 3 kHz sine at -12 dBFS, with no metadata.
set -eu
OUT=crates/audio-io/tests/fixtures/mp3
mkdir -p "$OUT"

make() {
  file=$1
  shift
  ffmpeg -hide_banner -loglevel error -y -f lavfi -i "sine=f=440:d=2,volume=-6dB" -f lavfi -i "sine=f=3000:d=2,volume=-12dB" \
    -filter_complex amix=inputs=2:normalize=0 -map_metadata -1 -id3v2_version 3 "$@" "$OUT/$file"
}

make cbr128_44k_stereo.mp3 -ar 44100 -ac 2 -c:a libmp3lame -b:a 128k -write_xing 0
make vbr_48k_stereo.mp3 -ar 48000 -ac 2 -c:a libmp3lame -q:a 2
make cbr64_44k_mono.mp3 -ar 44100 -ac 1 -c:a libmp3lame -b:a 64k -write_xing 0
make mpeg2_22k_mono.mp3 -ar 22050 -ac 1 -c:a libmp3lame -b:a 32k -write_xing 0
make mpeg2_24k_stereo.mp3 -ar 24000 -ac 2 -c:a libmp3lame -b:a 64k
