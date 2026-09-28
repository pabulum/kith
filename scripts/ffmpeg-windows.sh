#!/usr/bin/env bash
# Builds the ffmpeg.exe that Kith streams with on Windows, holding only what
# Kith's commands use: Windows.Graphics.Capture and Desktop Duplication,
# each graphics card maker's H.264 and HEVC encoders plus Windows' own, x264
# for the processor, AAC, and MPEG-TS through a pipe. It's statically linked:
# one small exe, where a full build is 160 MB.
#
# The build runs in an Arch Linux container (Docker) for a current
# mingw-w64, whose headers gfxcapture needs; `--here` uses this machine's
# tools instead (gcc, mingw-w64-gcc with headers 14+, nasm, cmake, make).
#
# Writes target/ffmpeg-windows/out/: ffmpeg.exe, FFMPEG-LICENSE.txt,
# FFMPEG-SOURCE.txt, and kith-ffmpeg-<version>-source.tar, everything it was
# built from. x264 makes the whole exe GPL, so a release that ships it
# offers that source too.
# Usage: scripts/ffmpeg-windows.sh [--here]
set -euo pipefail
cd "$(dirname "$0")/.."

FFMPEG=8.1.3
X264=b35605ace3ddf7c1a5d67a2eb553f034aef41d55 # x264's stable branch
# NVENC API 12.1, which ffmpeg refuses to use with drivers older than it:
# 531.61 (March 2023). Newer headers would shut out older drivers.
NVCODEC=12.1.14.0
# The oldest AMF headers ffmpeg builds with, for the same reason.
AMF=1.4.36
VPL=2.16.0

# file, url, sha256
SOURCES=(
    "ffmpeg-$FFMPEG.tar.xz https://ffmpeg.org/releases/ffmpeg-$FFMPEG.tar.xz 7138d28c96d9d3e3af4ee3d8cad72741f8ffb40da90c1112235dea3ecd3178a3"
    "x264-$X264.tar.bz2 https://code.videolan.org/videolan/x264/-/archive/$X264/x264-$X264.tar.bz2 6eeb82934e69fd51e043bd8c5b0d152839638d1ce7aa4eea65a3fedcf83ff224"
    "nv-codec-headers-n$NVCODEC.tar.gz https://github.com/FFmpeg/nv-codec-headers/releases/download/n$NVCODEC/nv-codec-headers-$NVCODEC.tar.gz 62b30ab37e4e9be0d0c5b37b8fee4b094e38e570984d56e1135a6b6c2c164c9f"
    "AMF-headers-v$AMF.tar.gz https://github.com/GPUOpen-LibrariesAndSDKs/AMF/releases/download/v$AMF/AMF-headers-v$AMF.tar.gz ec07ee21b820a73f5bae224e92fd3c492f52732d4885f0637f48189afdd87564"
    "libvpl-v$VPL.tar.gz https://github.com/intel/libvpl/archive/refs/tags/v$VPL.tar.gz d60931937426130ddad9f1975c010543f0da99e67edb1c6070656b7947f633b6"
)

if [[ ${1:-} != --here ]]; then
    image=kith-ffmpeg-windows
    docker build --quiet --tag "$image" - >/dev/null <<'EOF'
FROM archlinux:latest
RUN pacman -Syu --noconfirm --needed gcc mingw-w64-gcc nasm cmake make pkgconf diffutils \
 && pacman -Scc --noconfirm
EOF
    # As this user, so what it writes here stays theirs.
    exec docker run --rm --user "$(id -u):$(id -g)" --env HOME=/tmp \
        --volume "$PWD:/kith" --workdir /kith "$image" scripts/ffmpeg-windows.sh --here
fi

host=x86_64-w64-mingw32
work=$PWD/target/ffmpeg-windows
downloads=$work/downloads
src=$work/src
prefix=$work/prefix
out=$work/out
jobs=$(nproc)
mkdir -p "$downloads"
rm -rf "$src" "$prefix" "$out" "$work/build-vpl"
mkdir -p "$src" "$prefix/include" "$out"

for source in "${SOURCES[@]}"; do
    read -r file url sha <<<"$source"
    if [[ ! -f $downloads/$file ]]; then
        curl -fsSL --retry 3 -o "$downloads/$file.part" "$url"
        mv "$downloads/$file.part" "$downloads/$file"
    fi
    if ! echo "$sha  $downloads/$file" | sha256sum --check --quiet; then
        echo "error: $file doesn't match its checksum" >&2
        exit 1
    fi
    tar -xf "$downloads/$file" -C "$src"
done

make -C "$src/nv-codec-headers-$NVCODEC" PREFIX="$prefix" install >/dev/null
cp -r "$src/amf-headers-v$AMF/AMF" "$prefix/include/"

(
    cd "$src/x264-$X264"
    ./configure --host=$host --cross-prefix=$host- --prefix="$prefix" \
        --enable-static --disable-cli --disable-opencl --disable-lavf \
        --disable-swscale --disable-ffms --disable-gpac --disable-lsmash >/dev/null
    make -j"$jobs" >/dev/null
    make install >/dev/null
)

cmake -S "$src/libvpl-$VPL" -B "$work/build-vpl" -DCMAKE_SYSTEM_NAME=Windows \
    -DCMAKE_C_COMPILER=$host-gcc -DCMAKE_CXX_COMPILER=$host-g++ \
    -DCMAKE_RC_COMPILER=$host-windres -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX="$prefix" -DBUILD_SHARED_LIBS=OFF -DBUILD_TESTS=OFF \
    -DBUILD_EXAMPLES=OFF -DINSTALL_EXAMPLES=OFF -DBUILD_EXPERIMENTAL=OFF >/dev/null
cmake --build "$work/build-vpl" -j"$jobs" >/dev/null
cmake --install "$work/build-vpl" >/dev/null
# libvpl is C++, which its pkg-config file forgets to say.
sed -i 's/^Libs.private:.*/& -lstdc++/' "$prefix/lib/pkgconfig/vpl.pc"

# Everything else is off: `--disable-autodetect` keeps out whatever the
# build machine happens to have, and `--disable-everything` every codec,
# filter and format Kith doesn't name (capture.rs and capture/ffmpeg.rs).
CONFIGURE=(
    --target-os=mingw32 --arch=x86_64 --cross-prefix=$host- --pkg-config=pkg-config
    --pkg-config-flags=--static --extra-cflags="-I$prefix/include"
    --extra-ldflags="-L$prefix/lib -static"
    --enable-gpl --disable-debug --disable-doc --disable-ffplay --disable-ffprobe
    --disable-network --disable-autodetect --enable-w32threads
    --enable-d3d11va --enable-mediafoundation --enable-ffnvcodec --enable-nvenc
    --enable-amf --enable-libvpl --enable-libx264
    --disable-everything
    --enable-indev=lavfi
    --enable-filter=buffer,buffersink,abuffer,abuffersink,format,aformat,null,anull
    --enable-filter=scale,aresample,setpts,asetpts,trim,atrim
    --enable-filter=ddagrab,gfxcapture,scale_d3d11,hwmap,hwdownload,hwupload,vpp_qsv
    --enable-filter=testsrc2,sine
    --enable-encoder=h264_nvenc,hevc_nvenc,h264_amf,hevc_amf,h264_qsv,hevc_qsv
    --enable-encoder=h264_mf,hevc_mf,libx264,aac
    # lavfi hands over video as wrapped frames and sound as plain PCM; the
    # desktop sound Kith pipes in is 32-bit float PCM.
    --enable-decoder=wrapped_avframe,pcm_s16le,pcm_f32le --enable-demuxer=pcm_f32le
    --enable-muxer=mpegts,null --enable-protocol=pipe,file
    --enable-bsf=h264_mp4toannexb,hevc_mp4toannexb
)
(
    cd "$src/ffmpeg-$FFMPEG"
    PKG_CONFIG_PATH="$prefix/lib/pkgconfig" ./configure --prefix="$prefix" "${CONFIGURE[@]}" \
        >"$work/configure.log" || {
        tail -30 ffbuild/config.log >&2
        exit 1
    }
    make -j"$jobs" ffmpeg.exe >/dev/null
    $host-strip -o "$out/ffmpeg.exe" ffmpeg.exe
)

# A static build imports nothing but Windows' own DLLs: a stray
# libstdc++-6.dll would make it fail to start on every PC.
imports=$($host-objdump -p "$out/ffmpeg.exe" | sed -n 's/^\s*DLL Name: //p' | sort -fu)
for dll in $imports; do
    case ${dll,,} in
        kernel32.dll | user32.dll | gdi32.dll | advapi32.dll | ole32.dll | oleaut32.dll | \
            shell32.dll | shlwapi.dll | bcrypt.dll | msvcrt.dll | ucrtbase.dll | api-ms-win-*.dll | \
            d3d11.dll | dxgi.dll | mfplat.dll | mf.dll | mfuuid.dll | windowsapp.dll | \
            combase.dll | runtimeobject.dll | psapi.dll | ws2_32.dll | secur32.dll | \
            user-env.dll | userenv.dll | ntdll.dll | d3d9.dll | dxva2.dll) ;;
        *)
            echo "error: ffmpeg.exe imports $dll, which Windows doesn't have" >&2
            exit 1
            ;;
    esac
done

version_of() { sed -n "s/^#define __MINGW64_VERSION_$1 *\([0-9]*\).*/\1/p" "/usr/$host/include/_mingw_mac.h" 2>/dev/null || true; }
mingw="$(version_of MAJOR).$(version_of MINOR)"
toolchain="$($host-gcc --version | sed -n 1p), mingw-w64 ${mingw:-(unknown version)}"
{
    cat <<EOF
ffmpeg.exe is FFmpeg $FFMPEG, built for Kith by scripts/ffmpeg-windows.sh in
Kith's repository. It includes x264, so as a whole it's under the GNU General
Public License, version 2 or later (below). FFmpeg's own license terms follow
it, then those of the other code built in: NVIDIA's and AMD's encoder headers
and Intel's libvpl, all MIT.

Kith (kith.exe) only runs ffmpeg.exe as a separate program; Kith's own
license is in LICENSE.txt.

EOF
    for file in COPYING.GPLv2 LICENSE.md; do
        printf '%s\n' "------------------------------------------------------------------------" "FFmpeg: $file" ""
        cat "$src/ffmpeg-$FFMPEG/$file"
    done
    printf '%s\n' "------------------------------------------------------------------------" "x264 (GPL-2.0-or-later: the GPL text above)" ""
    sed -n '/Copyright/,/licensing@x264.com/p' "$src/x264-$X264/x264.h" | sed 's/^ \* \{0,1\}//'
    printf '%s\n' "------------------------------------------------------------------------" "nv-codec-headers $NVCODEC (NVIDIA)" ""
    sed -n '/This copyright notice/,/OTHER DEALINGS/p' "$src/nv-codec-headers-$NVCODEC/include/ffnvcodec/nvEncodeAPI.h" | sed 's/^ \* \{0,1\}//'
    printf '\n%s\n' "------------------------------------------------------------------------" "AMF headers $AMF (AMD)" ""
    sed -n '/^\/\/ Notice Regarding Standards/,/^\/\/ THE SOFTWARE\.$/p' "$src/amf-headers-v$AMF/AMF/core/Version.h" | sed 's|^// \{0,1\}||'
    printf '\n%s\n' "------------------------------------------------------------------------" "libvpl $VPL (Intel)" ""
    cat "$src/libvpl-$VPL/LICENSE"
} >"$out/FFMPEG-LICENSE.txt"

bundle=kith-ffmpeg-$FFMPEG-source.tar
{
    cat <<EOF
The source of ffmpeg.exe
========================

ffmpeg.exe is FFmpeg $FFMPEG with x264, NVIDIA's and AMD's encoder headers,
and Intel's libvpl, built by Kith's scripts/ffmpeg-windows.sh. Each Kith
release that ships it also carries $bundle, which holds
that script and these exact sources:

EOF
    for source in "${SOURCES[@]}"; do
        read -r file url sha <<<"$source"
        printf '  %s\n    from %s\n    sha256 %s\n' "$file" "$url" "$sha"
    done
    cat <<EOF

It was built with $toolchain, and FFmpeg was configured with:

  ./configure $(printf '%q ' "${CONFIGURE[@]}")

Whatever Kith release you got ffmpeg.exe from, you can have its source:
download the source file from the same release page, or ask through the
project's issue tracker, for three years after the release.
EOF
} >"$out/FFMPEG-SOURCE.txt"

tar -cf "$out/$bundle" -C "$downloads" $(for s in "${SOURCES[@]}"; do read -r f _ <<<"$s"; echo "$f"; done) \
    -C "$PWD/scripts" ffmpeg-windows.sh -C "$out" FFMPEG-SOURCE.txt
ls -l "$out"
