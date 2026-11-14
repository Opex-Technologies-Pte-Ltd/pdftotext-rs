#!/usr/bin/env bash
# Build the npm package `pdftotext-rs` into npm/ (publish with `npm publish npm/`).
#
# One package for every JavaScript runtime, with the same API everywhere:
#   import init, { extract_pdf, page_images } from 'pdftotext-rs';
#   await init();
# Node gets wasm-pack's `nodejs` build (the module loads from disk; init() is a
# no-op), everything else (browsers, bundlers, Deno, Bun, Workers) gets the
# `web` build, whose init() fetches or takes the .wasm.
#
# Needs: wasm-pack, the wasm32-unknown-unknown target.
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
OUT=npm
rm -rf "$OUT" build-npm
wasm-pack build --target nodejs --release --out-dir build-npm/node --out-name pdftotext -- --locked
wasm-pack build --target web --release --out-dir build-npm/web --out-name pdftotext -- --locked

mkdir -p "$OUT/node" "$OUT/web"
cp build-npm/node/pdftotext.js build-npm/node/pdftotext_bg.wasm build-npm/node/pdftotext.d.ts "$OUT/node/"
cp build-npm/web/pdftotext.js build-npm/web/pdftotext_bg.wasm build-npm/web/pdftotext.d.ts "$OUT/web/"
# wasm-pack's nodejs glue is CommonJS; mark that folder so it loads as such
# inside this "type": "module" package.
echo '{ "type": "commonjs" }' > "$OUT/node/package.json"
rm -rf build-npm

cat > "$OUT/node.js" <<'EOF'
import { createRequire } from 'node:module';
const m = createRequire(import.meta.url)('./node/pdftotext.js');
export const extract_pdf = m.extract_pdf;
export const page_images = m.page_images;
/** No-op in Node: the module is loaded from disk on import. */
export default async function init() {}
EOF

cat > "$OUT/index.js" <<'EOF'
export { default, extract_pdf, page_images } from './web/pdftotext.js';
EOF

cat > "$OUT/index.d.ts" <<'EOF'
export { extract_pdf, page_images } from './web/pdftotext.js';
/**
 * Load the WebAssembly module. Call once before the other functions. In Node
 * it does nothing; elsewhere it fetches `pdftotext_bg.wasm` next to this file,
 * or takes the module/bytes/URL you pass (e.g. a Cloudflare Worker's
 * `import wasm from 'pdftotext-rs/pdftotext_bg.wasm'`).
 */
export default function init(
  module_or_path?:
    | { module_or_path: WebAssembly.Module | BufferSource | URL | string | Response | Promise<Response> }
    | WebAssembly.Module
    | BufferSource
    | URL
    | string
    | Response
    | Promise<Response>,
): Promise<unknown>;
EOF
cp "$OUT/index.d.ts" "$OUT/node.d.ts"

cp README.md LICENSE NOTICE THIRD_PARTY.md "$OUT/"

cat > "$OUT/package.json" <<EOF
{
  "name": "pdftotext-rs",
  "version": "$VERSION",
  "description": "PDF text extraction that reads equipment manuals correctly (CJK, columns, scrambled fonts). Clean-room Rust compiled to WebAssembly; no native dependencies.",
  "license": "Apache-2.0",
  "repository": { "type": "git", "url": "git+https://github.com/Opex-Technologies-Pte-Ltd/pdftotext-rs.git" },
  "homepage": "https://github.com/Opex-Technologies-Pte-Ltd/pdftotext-rs#readme",
  "bugs": "https://github.com/Opex-Technologies-Pte-Ltd/pdftotext-rs/issues",
  "keywords": ["pdf", "pdftotext", "text-extraction", "wasm", "webassembly", "cjk", "ocr", "manuals"],
  "type": "module",
  "sideEffects": false,
  "main": "./index.js",
  "types": "./index.d.ts",
  "exports": {
    ".": {
      "node": { "types": "./node.d.ts", "default": "./node.js" },
      "types": "./index.d.ts",
      "default": "./index.js"
    },
    "./pdftotext_bg.wasm": "./web/pdftotext_bg.wasm",
    "./package.json": "./package.json"
  },
  "files": ["index.js", "index.d.ts", "node.js", "node.d.ts", "node/", "web/", "README.md", "LICENSE", "NOTICE", "THIRD_PARTY.md"]
}
EOF
echo "npm package ready in $OUT/ (version $VERSION)"
