#!/usr/bin/env bash
# Builds the release in dist/: a Linux tarball, a Windows zip, the source of
# the ffmpeg.exe in that zip, and SHA256SUMS. GitHub Actions runs this for
# every version tag (.github/workflows/release.yml).
#
# The Linux program is linked against glibc 2.31 (zig does that), so it runs
# on distributions from 2020 on, not just on the one it was built on. The
# Windows zip holds what the app installs: kith.exe, the trimmed ffmpeg.exe
# that streaming needs (scripts/ffmpeg-windows.sh), and their licenses.
#
# Compilers bake source paths into binaries (panic messages, C asserts), and
# those paths start with the builder's home directory, which usually names
# them. The build remaps it, then fails if the home directory (or, off CI,
# the user name) still shows up in a binary.
#
# Needs the x86_64-pc-windows-gnu rustup target, mingw-w64-gcc, zig,
# cargo-zigbuild and cargo-about (`cargo install --locked cargo-zigbuild
# cargo-about --features cargo-about/cli`), and Docker unless ffmpeg.exe is
# built already.
# Usage: scripts/release.sh
set -euo pipefail

cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
for tool in about zigbuild; do
    cargo $tool --help >/dev/null 2>&1 || {
        echo "cargo-$tool is missing: cargo install --locked cargo-$tool" >&2
        exit 1
    }
done

ffmpeg=target/ffmpeg-windows/out
[[ -f $ffmpeg/ffmpeg.exe ]] || scripts/ffmpeg-windows.sh

export RUSTFLAGS="--remap-path-prefix=$HOME=~"
export CFLAGS="-ffile-prefix-map=$HOME=~"
export CXXFLAGS="$CFLAGS"
# Its own target dir, so these flags don't force a full rebuild of dev builds.
target=target/release-dist

rm -rf dist
mkdir -p dist/stage

notices=dist/stage/THIRD-PARTY-LICENSES.txt
cargo about generate --fail -c scripts/about/about.toml scripts/about/about.hbs -o "$notices"
{
    printf '\n%s\n' "------------------------------------------------------------------------"
    echo "mpegts.js 1.8.2 (embedded; plays streams in the browser)"
    echo
    cat assets/mpegts.js/mpegts.js.LICENSE.txt
    echo
    cat assets/mpegts.js/LICENSE
} >>"$notices"

check_leaks() {
    local leaks=("$HOME")
    # A CI runner's user name ("runner") is a common word in binaries.
    [[ -n ${CI:-} ]] || leaks+=("$USER")
    for leak in "${leaks[@]}"; do
        if grep -qaF -- "$leak" "$1"; then
            echo "error: $1 still contains \"$leak\"" >&2
            exit 1
        fi
    done
}

# Linux
cargo zigbuild --release --locked --target x86_64-unknown-linux-gnu.2.31 --target-dir "$target"
bin=$target/x86_64-unknown-linux-gnu/release/kith
check_leaks "$bin"
name=kith-$version-linux-x86_64
mkdir -p "dist/stage/$name"
cp "$bin" README.md LICENSE "$notices" "dist/stage/$name/"
tar -C dist/stage -czf "dist/$name.tar.gz" "$name"

# Windows: laid out as the app's installer expects (install/windows.rs).
cargo build --release --locked --target x86_64-pc-windows-gnu --target-dir "$target"
exe=$target/x86_64-pc-windows-gnu/release/kith.exe
check_leaks "$exe"
name=kith-$version-windows-x86_64
mkdir -p "dist/stage/$name"
cp "$exe" README.md "$notices" "$ffmpeg/ffmpeg.exe" "$ffmpeg/FFMPEG-LICENSE.txt" \
    "$ffmpeg/FFMPEG-SOURCE.txt" "dist/stage/$name/"
cp LICENSE "dist/stage/$name/LICENSE.txt"
(cd dist/stage && bsdtar -a -cf "../$name.zip" "$name")

cp "$ffmpeg"/kith-ffmpeg-*-source.tar dist/
rm -rf dist/stage
(cd dist && sha256sum -- * >SHA256SUMS)
ls -l dist
