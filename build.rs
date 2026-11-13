//! Hard memory ceiling for the wasm build.
//!
//! The Cloudflare Worker that runs the wasm has 128 MB for everything: the JS
//! heap, the PDF bytes it downloaded, and this module's linear memory. Without
//! a declared maximum, wasm memory grows until the isolate is OOM-killed, which
//! is not catchable and leaves the manual stuck. With `--max-memory`,
//! `memory.grow` fails at the ceiling, the Rust allocator reports the failure
//! and the module aborts: a `WebAssembly.RuntimeError` the Worker catches
//! (OpexMX's loader: `pdf-wasm-loader.ts`), after which it routes the PDF to
//! the extraction container.
//!
//! 80 MiB: the largest of 22 real equipment manuals measured 74 MiB peak
//! (skf-bearing, 23 MB, 1152 pages; most need 2-48 MiB). Together with the JS
//! copy of a PDF at the Worker's wasm input cap (24 MiB,
//! `PDF_MAX_BYTES_FOR_WASM`) that is 104 MiB (109 MB), leaving ~19 MB of the
//! 128 MB for the isolate's own heap. Keep `WASM_MAX_MEMORY_BYTES` in
//! the host's loader (OpexMX: `pdf-wasm-loader.ts`) equal to this value.
//! Must be a multiple of the 64 KiB wasm page.

const WASM_MAX_MEMORY: u64 = 80 * 1024 * 1024;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    const _: () = assert!(WASM_MAX_MEMORY.is_multiple_of(65536));
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") {
        println!("cargo:rustc-link-arg-cdylib=--max-memory={WASM_MAX_MEMORY}");
    }
}
