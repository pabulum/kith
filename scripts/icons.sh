#!/usr/bin/env bash
# Renders assets/icon/kith.svg (and kith-small.svg, drawn for 16 to 24
# pixels) into the PNGs and the Windows .ico that builds embed. Run it after
# editing either SVG; the outputs are committed so builds don't need these tools.
#
# Needs rsvg-convert (librsvg), ImageMagick 7 (`magick`) and python3.
set -euo pipefail

cd "$(dirname "$0")/../assets/icon"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

render() { # size -> kith-<size>.png, from the drawing meant for that size
    local src=kith.svg
    (($1 <= 24)) && src=kith-small.svg
    rsvg-convert -w "$1" -h "$1" "$src" -o "$2"
}

# Linux icon theme sizes, installed into hicolor by the app.
for size in 16 22 24 32 48 64 128 256 512; do
    render "$size" "kith-$size.png"
done

# Windows: every size Explorer, the taskbar and the tray ask for, at 100% to 200% scaling.
ico=()
for size in 16 20 24 32 40 48 64 256; do
    render "$size" "$tmp/$size.png"
    ico+=("$tmp/$size.png")
done
magick "${ico[@]}" "$tmp/bmp.ico"
# ImageMagick stores every entry as a bitmap, 270 KB for 256 pixels alone.
# Windows has read PNG entries since Vista, so the big one goes in as PNG.
python3 - "$tmp/bmp.ico" "$tmp/256.png" kith.ico <<'PY'
import struct, sys
ico, png, out = open(sys.argv[1], "rb").read(), open(sys.argv[2], "rb").read(), sys.argv[3]
count = struct.unpack_from("<H", ico, 4)[0]
entries, images = [], []
for i in range(count):
    entry = list(struct.unpack_from("<BBBBHHII", ico, 6 + 16 * i))
    data = ico[entry[7]:entry[7] + entry[6]]
    if entry[0] == 0:  # 0 means 256
        data = png
    entries.append(entry)
    images.append(data)
offset = 6 + 16 * count
head = struct.pack("<HHH", 0, 1, count)
for entry, data in zip(entries, images):
    entry[6], entry[7] = len(data), offset
    head += struct.pack("<BBBBHHII", *entry)
    offset += len(data)
open(out, "wb").write(head + b"".join(images))
PY
ls -l kith.ico kith-*.png
