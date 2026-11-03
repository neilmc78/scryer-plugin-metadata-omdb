#!/usr/bin/env bash
# Build the release artifact: dist/scryer-plugin-metadata-omdb.wasm, the
# component with its descriptor embedded, ready for Settings > Plugins >
# upload in Scryer.
set -euo pipefail
cd "$(dirname "$0")"

cargo build --release --target wasm32-wasip2 -p scryer-plugin-metadata-omdb
mkdir -p dist
cargo run --quiet -p embed-descriptor -- \
  target/wasm32-wasip2/release/scryer_plugin_metadata_omdb.wasm \
  dist/scryer-plugin-metadata-omdb.wasm
sha256sum dist/scryer-plugin-metadata-omdb.wasm
