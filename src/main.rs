//! pdftotext CLI — a drop-in for poppler's `pdftotext`.
//!
//! Contract:
//!   pdftotext -v                              exit 0 (or 99); version → stderr
//!   pdftotext [-layout] [-raw] [-enc UTF-8] [-f N] [-l N] [-nopgbrk] [-q]
//!             <file.pdf>|- [outfile|-
//!   success → exit 0, UTF-8 text on stdout, pages joined by '\f'
//!   failure → exit != 0 (caller falls back to unpdf), diagnostics on stderr

use std::io::{self, Write};
use std::process::ExitCode;

use pdftotext::{extract, Error, Options};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => ExitCode::from(code),
    }
}

#[derive(Default)]
struct Cli {
    opts: Options,
    quiet: bool,
    nopgbrk: bool,
    positionals: Vec<String>,
}

fn run() -> Result<(), u8> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // `-v` / `-h` short-circuit before anything else (no file needed).
    if args.iter().any(|a| a == "-v" || a == "--version") {
        eprintln!("pdftotext-rs (clean-room, ISO 32000-2) {VERSION}");
        return Ok(());
    }
    if args.iter().any(|a| a == "-h" || a == "--help") {
        usage();
        return Ok(());
    }

    let mut cli = Cli::default();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "-layout" => cli.opts.layout = true,
            "-raw" => cli.opts.raw = true,
            "-q" => cli.quiet = true,
            "-nopgbrk" => cli.nopgbrk = true,
            "-enc" => {
                let enc = next_value(&args, &mut i, "-enc")?;
                // Only UTF-8 (case-insensitive). Anything else is an
                // honest non-zero exit rather than silent wrong-encoding output.
                if !enc.eq_ignore_ascii_case("UTF-8") && !enc.eq_ignore_ascii_case("UTF8") {
                    return Err(fail(
                        2,
                        &format!("unsupported encoding '{enc}' (only UTF-8)"),
                    ));
                }
            }
            "-f" => {
                let v = next_value(&args, &mut i, "-f")?;
                cli.opts.first_page = parse_page(&v, "-f")?;
            }
            "-l" => {
                let v = next_value(&args, &mut i, "-l")?;
                cli.opts.last_page = Some(parse_page(&v, "-l")?);
            }
            // Unknown -flag: ignore (poppler accepts options we don't implement,
            // e.g. -table which the container sidecar expects to be ABSENT).
            s if s.starts_with('-') && s != "-" => {
                if !cli.quiet {
                    eprintln!("warning: ignoring unknown option '{s}'");
                }
            }
            // Positional: first = input file, second = output (`-` or path).
            s => cli.positionals.push(s.to_string()),
        }
        i += 1;
    }

    let input = match cli.positionals.first() {
        Some(p) => p,
        None => {
            return Err(fail(
                1,
                "missing input file (usage: pdftotext [opts] <file.pdf>|- [outfile|-])",
            ))
        }
    };
    // Output target: second positional, default stdout. `-` = stdout.
    let out_is_stdout = match cli.positionals.get(1) {
        None => true,
        Some(p) if p == "-" => true,
        Some(_) => false,
    };

    if input == "-" {
        // Input must be a file path: reading a PDF from stdin is not supported.
        return Err(fail(1, "input must be a file path, not '-'"));
    }

    let bytes = match std::fs::read(input) {
        Ok(b) => b,
        Err(e) => return Err(fail(2, &format!("cannot read '{input}': {e}"))),
    };

    let pages = match extract(&bytes, &cli.opts) {
        Ok(p) => p,
        Err(e) => return Err(fail(exit_code(&e), &e.to_string())),
    };

    // 0x0C = form feed (the page separator the caller splits on).
    let sep = if cli.nopgbrk { "\n" } else { "\u{000C}" };
    let mut out: Box<dyn Write> = if out_is_stdout {
        Box::new(io::stdout().lock())
    } else {
        match std::fs::File::create(cli.positionals[1].as_str()) {
            Ok(f) => Box::new(f),
            Err(e) => return Err(fail(2, &format!("cannot create output: {e}"))),
        }
    };
    // Emit each page followed by the separator (trailing separator tolerated by
    // the caller, which splits on '\f' and drops an empty trailing element).
    // A failed write (closed pipe, full disk) must surface as a failure: the caller
    // would otherwise take truncated text as the whole document.
    let written = pages
        .iter()
        .try_for_each(|page| {
            out.write_all(page.text.as_bytes())?;
            out.write_all(sep.as_bytes())
        })
        .and_then(|()| out.flush());
    if let Err(e) = written {
        return Err(fail(2, &format!("write failed: {e}")));
    }
    Ok(())
}

fn next_value(args: &[String], i: &mut usize, flag: &str) -> Result<String, u8> {
    *i += 1;
    match args.get(*i) {
        Some(v) => Ok(v.clone()),
        None => Err(fail(1, &format!("{flag} requires a value"))),
    }
}

fn parse_page(v: &str, flag: &str) -> Result<u32, u8> {
    v.parse::<u32>()
        .map_err(|_| fail(1, &format!("{flag} expects a positive integer, got '{v}'")))
        .and_then(|n| {
            if n >= 1 {
                Ok(n)
            } else {
                Err(fail(1, &format!("{flag} must be >= 1")))
            }
        })
}

/// Exit status per failure class (callers only test `!= 0`; the distinct codes
/// are for humans and logs).
fn exit_code(e: &Error) -> u8 {
    match e {
        Error::Encrypted(_) => 3,
        Error::Limit(_) => 4,
        _ => 2,
    }
}

fn usage() {
    eprintln!(
        "pdftotext-rs (clean-room) {VERSION}\n\
         usage: pdftotext [-layout|-raw] [-enc UTF-8] [-f N] [-l N] [-nopgbrk] [-q] <file.pdf>|- [outfile|-]\n\
         pages on stdout are separated by form-feed (\\f)."
    );
}

/// Print a diagnostic to stderr and return the chosen exit code.
fn fail(code: u8, msg: &str) -> u8 {
    eprintln!("pdftotext: {msg}");
    code
}
