#!/usr/bin/env sh
# Build the dashboard: Rust → wasm32 → wasm-bindgen glue → ui/dist.
# Requires the wasm32-unknown-unknown target and wasm-bindgen-cli 0.2.129
# (must equal the `wasm-bindgen` crate version pinned in Cargo.toml).
set -eu
cd "$(dirname "$0")"
PROFILE="${PROFILE:-release}"
cargo build --locked --target wasm32-unknown-unknown --profile "$PROFILE"
# Cargo names the output directory of the `dev` profile "debug".
OUT_DIR="$PROFILE"
[ "$PROFILE" = "dev" ] && OUT_DIR="debug"
rm -rf dist
mkdir -p dist
wasm-bindgen --target web --no-typescript --out-dir dist --out-name wm_ui \
  "target/wasm32-unknown-unknown/${OUT_DIR}/wm-ui.wasm"
if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt -Oz --all-features dist/wm_ui_bg.wasm -o dist/wm_ui_bg.wasm
fi
cp static/index.html static/style.css static/boot.js dist/
ls -l dist
