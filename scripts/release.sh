#!/usr/bin/env bash
# Builds release archives in dist/: Linux and Windows, x86_64.
#
# Compilers bake source paths into binaries (panic messages, C asserts), and
# those paths start with the builder's home directory, which usually names
# them. The build remaps it, then fails if the home directory or user name
# still shows up in either binary.
#
# Needs the x86_64-pc-windows-gnu rustup target, mingw-w64-gcc, and
# cargo-about (`cargo install --locked cargo-about --features cli`).
# Usage: scripts/release.sh
set -euo pipefail

cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
command -v cargo-about >/dev/null || {
    echo "cargo-about is missing: cargo install --locked cargo-about --features cli" >&2
    exit 1
}

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

for triple in x86_64-unknown-linux-gnu x86_64-pc-windows-gnu; do
    cargo build --release --locked --target "$triple" --target-dir "$target"
    case $triple in
        *windows*) exe=pstream.exe platform=windows-x86_64 ;;
        *) exe=pstream platform=linux-x86_64 ;;
    esac
    bin=$target/$triple/release/$exe
    for leak in "$HOME" "$USER"; do
        if grep -qaF -- "$leak" "$bin"; then
            echo "error: $bin still contains \"$leak\"" >&2
            exit 1
        fi
    done

    name=pstream-$version-$platform
    mkdir -p "dist/stage/$name"
    cp "$bin" README.md LICENSE "$notices" "dist/stage/$name/"
    case $platform in
        windows*) (cd dist/stage && bsdtar -a -cf "../$name.zip" "$name") ;;
        *) tar -C dist/stage -czf "dist/$name.tar.gz" "$name" ;;
    esac
done

rm -rf dist/stage
(cd dist && sha256sum -- * >SHA256SUMS)
ls -l dist
