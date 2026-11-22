#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
version=$(cargo metadata --locked --offline --no-deps --format-version 1 | python3 -c 'import json,sys; d=json.load(sys.stdin); print(next(p["version"] for p in d["packages"] if p["name"] == "czi"))')
target=x86_64-unknown-linux-gnu
host=$(rustc -vV | sed -n 's/^host: //p')
if [[ $host != "$target" ]]; then
  echo "Expected native host $target, got $host" >&2
  exit 1
fi
cargo build --release --locked --target "$target"
binary="target/$target/release/czi"
[[ -x $binary && -f README.md && -f LICENSE.txt ]] || { echo 'Missing release input' >&2; exit 1; }
[[ $("$binary" --version) == "czi $version" ]] || { echo 'Binary version mismatch' >&2; exit 1; }
mkdir -p dist
archive="czi-v${version}-${target}.tar.gz"
if [[ -e dist/$archive || -e dist/SHA256SUMS ]]; then
  echo 'Release output already exists; move it aside before packaging' >&2
  exit 1
fi
tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT
mkdir -p "$tmp/czi"
install -m 755 "$binary" "$tmp/czi/czi"
install -m 644 README.md LICENSE.txt "$tmp/czi/"
tar -czf "dist/$archive" -C "$tmp" czi
(cd dist && sha256sum "$archive" > SHA256SUMS)
echo "Created dist/$archive and dist/SHA256SUMS"
