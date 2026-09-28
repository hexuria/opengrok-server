//! The text of a PDF a person attached (#229), read in a process of its own.
//!
//! THE SERVER NEVER PARSES A PDF ITSELF. The parser walks an untrusted file: a compressed stream
//! can inflate without limit, and nested objects recurse with no depth check. A stack overflow
//! or an allocation that fails is not a panic anything can catch, so in the server it would take
//! every conversation down with one file (review of #261). Here it takes down only this process,
//! which the server kills after its time limit anyway, and on Linux this process cannot grow past
//! `MEMORY_LIMIT` at all.
//!
//! Protocol: the PDF on stdin; on success, `<pages kept> <pages in the file>` on the first line of
//! stdout and the kept pages' text after it, exit 0. Any failure exits non-zero, and the server
//! tells the model the file could not be read.

use std::io::{Read, Write};
use std::process::ExitCode;

/// The most a PDF may weigh coming in: the artifact cap, and a little over.
const MAX_INPUT_BYTES: u64 = 26 * 1024 * 1024;

/// Pages whose text is kept.
const MAX_PAGES: usize = 100;

/// Characters written back; the server cuts again at what the model reads.
const MAX_OUTPUT_CHARS: usize = 100_000;

/// What this process may allocate, on Linux (`RLIMIT_AS`). An inflation bomb meets it and fails
/// here, rather than in the host's memory.
#[cfg(target_os = "linux")]
const MEMORY_LIMIT: u64 = 1024 * 1024 * 1024;

fn main() -> ExitCode {
    #[cfg(target_os = "linux")]
    {
        if rlimit::setrlimit(rlimit::Resource::AS, MEMORY_LIMIT, MEMORY_LIMIT).is_err() {
            return ExitCode::from(3);
        }
    }
    let mut bytes = Vec::new();
    if std::io::stdin()
        .take(MAX_INPUT_BYTES)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return ExitCode::from(2);
    }
    let Ok(pages) = pdf_extract::extract_text_from_mem_by_pages(&bytes) else {
        return ExitCode::from(1);
    };
    let total = pages.len();
    let kept = total.min(MAX_PAGES);
    let text: String = pages
        .into_iter()
        .take(MAX_PAGES)
        .collect::<Vec<_>>()
        .join("\n\n")
        .chars()
        .take(MAX_OUTPUT_CHARS)
        .collect();
    let mut out = std::io::stdout().lock();
    if writeln!(out, "{kept} {total}")
        .and_then(|()| out.write_all(text.as_bytes()))
        .is_err()
    {
        return ExitCode::from(2);
    }
    ExitCode::SUCCESS
}
