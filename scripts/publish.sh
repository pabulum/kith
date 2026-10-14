#!/usr/bin/env bash
# Publishes a release CI drafted (.github/workflows/release.yml). It checks
# the draft's files against its SHA256SUMS, signs SHA256SUMS with the key
# Kith's updater trusts (KEY in src/update.rs), uploads that as
# SHA256SUMS.sig, and publishes. Installed Kiths update only to releases
# signed this way, and the key never leaves this PC, so the GitHub repository
# alone can't push an update to anyone. Given a release published already,
# it adds the signature.
#
# The key is $KITH_SIGNING_KEY, or ~/.config/kith-release/signing-key.pem.
# Keep a copy somewhere safe: without it, installed Kiths stop updating, and
# friends have to download the next Kith by hand. To give it a passphrase,
# which signing then asks for:
#   openssl pkey -in <key> -aes256 -out <key>.new && mv <key>.new <key>
#
# Needs gh (logged in as someone who can push here), openssl 3 and sha256sum.
# Usage: scripts/publish.sh vX.Y.Z
set -euo pipefail

cd "$(dirname "$0")/.."
tag=${1:?usage: scripts/publish.sh vX.Y.Z}
key=${KITH_SIGNING_KEY:-$HOME/.config/kith-release/signing-key.pem}
[[ -f $key ]] || { echo "error: no signing key at $key" >&2; exit 1; }

# The key the tagged Kith trusts, in the form openssl reads an Ed25519 public
# key: a fixed 12-byte header, then the key's 32 bytes.
trusted=$(git show "$tag:src/update.rs" | sed -n 's/^const KEY: &str = "\([0-9a-f]\{64\}\)";$/\1/p')
[[ -n $trusted ]] || { echo "error: $tag's src/update.rs has no KEY" >&2; exit 1; }
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
printf '%b' "$(sed 's/../\\x&/g' <<<"302a300506032b6570032100$trusted")" >"$tmp/trusted.der"

draft=$(gh release view "$tag" --json isDraft --jq .isDraft)
gh release download "$tag" --dir "$tmp/release"
(cd "$tmp/release" && sha256sum --check --strict --quiet SHA256SUMS) || {
    echo "error: $tag's files don't match its SHA256SUMS" >&2
    exit 1
}

echo "Signing $tag's SHA256SUMS with $key"
openssl pkeyutl -sign -inkey "$key" -rawin -in "$tmp/release/SHA256SUMS" -out "$tmp/SHA256SUMS.sig"
openssl pkeyutl -verify -pubin -keyform DER -inkey "$tmp/trusted.der" -rawin \
    -in "$tmp/release/SHA256SUMS" -sigfile "$tmp/SHA256SUMS.sig" >/dev/null 2>&1 || {
    echo "error: $key isn't the key $tag's Kith trusts (KEY in src/update.rs)" >&2
    exit 1
}
gh release upload "$tag" "$tmp/SHA256SUMS.sig" --clobber
if [[ $draft == true ]]; then
    gh release edit "$tag" --draft=false >/dev/null
    echo "Published $tag. Installed Kiths find it within six hours, and install it when they next start."
else
    echo "Signed $tag, which was published already."
fi
