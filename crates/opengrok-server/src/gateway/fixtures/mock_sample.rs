//! Mock fixture: enough Rust to judge the highlighter.
use std::path::PathBuf;

const FIXTURE_DIR: &str = "/tmp/opengrok-mock-fixtures";

/// A fixture on disk, described. Lifetimes, generics, a macro and a
/// match all appear on purpose.
#[derive(Debug, Clone)]
pub struct Fixture<'a> {
    pub name: &'a str,
    pub size: usize,
}

pub fn describe<'a>(name: &'a str, bytes: &[u8]) -> Fixture<'a> {
    let kind = match name.rsplit_once('.') {
        Some((_, "png" | "mp4" | "pdf")) => "binary",
        Some(_) | None => "text",
    };
    println!("{name}: {} bytes ({kind})", bytes.len());
    Fixture { name, size: bytes.len() }
}

fn path_of(name: &str) -> PathBuf {
    PathBuf::from(FIXTURE_DIR).join(name)
}
