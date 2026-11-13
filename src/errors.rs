//! Error type. std-only (no thiserror) — keeps `cargo build` dependency-light.

use std::fmt;

/// All extraction failures map to a non-zero process exit (caller falls back to
/// unpdf). The variant carries enough for `main.rs` to pick an exit code and a
/// stderr line, and for the wasm bridge to surface a stable machine-readable tag
/// (see `Display`).
#[derive(Debug)]
pub enum Error {
    /// `%PDF-` header missing or unreadable — not a PDF.
    NotPdf,
    /// File could not be read (missing, permission, …).
    Read(String),
    /// PDF structure we don't handle (broken xref, no pages, …).
    Unsupported(String),
    /// Document needs a password (or uses an encryption scheme we cannot open).
    /// Documents encrypted with only an owner password open transparently with
    /// the empty user password and never reach this variant.
    Encrypted(String),
    /// A hard work/memory ceiling was hit (decompression bomb, runaway XObject
    /// nesting, …). The input is treated as hostile or pathological.
    Limit(String),
}

/// `Display` always begins with a stable lowercase tag (`not_pdf:`, `read:`,
/// `unsupported:`, `encrypted:`, `limit:`) so the wasm bridge — which throws the
/// message as a JS string — lets the TS caller branch without matching prose.
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotPdf => write!(f, "not_pdf: missing %PDF- header"),
            Error::Read(m) => write!(f, "read: {m}"),
            Error::Unsupported(m) => write!(f, "unsupported: {m}"),
            Error::Encrypted(m) => write!(f, "encrypted: {m}"),
            Error::Limit(m) => write!(f, "limit: {m}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T, E = Error> = std::result::Result<T, E>;
