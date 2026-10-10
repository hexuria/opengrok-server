#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
//! The process a PDF is read in, played by stand-in scripts: each way it can go wrong must end
//! as a sentence for the model, never in this process (review of #261).

use super::*;

/// A reader that is this shell script, with this time limit.
fn reader(script: &str, timeout_ms: u64) -> PdfReader {
    let dir = std::env::temp_dir().join(format!("og-pdf-{}", uuid::Uuid::now_v7().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("reader.sh");
    // A script written here can be exec'd while a sibling test's fork still holds its write fd;
    // writing from a child keeps that fd out of this process and avoids ETXTBSY.
    let status = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            "printf '%s' \"$2\" > \"$1\" && chmod 755 \"$1\"",
            "sh",
        ])
        .arg(&bin)
        .arg(format!("#!/bin/sh\n{script}\n"))
        .status()
        .unwrap();
    assert!(
        status.success(),
        "the test reader script could not be written"
    );
    PdfReader {
        bin: Some(bin),
        timeout: std::time::Duration::from_millis(timeout_ms),
    }
}

fn why(read: PdfText) -> &'static str {
    match read {
        PdfText::Unreadable(why) => why,
        PdfText::Read { .. } => panic!("expected the file to be unreadable"),
    }
}

#[tokio::test]
async fn a_reader_that_does_not_finish_is_killed_at_its_limit() {
    let started = std::time::Instant::now();
    let read = read_pdf(&reader("exec sleep 30", 300), b"%PDF".to_vec()).await;
    assert_eq!(why(read), "reading it took too long");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "the turn does not wait on it"
    );
}

#[tokio::test]
async fn a_reader_that_crashes_is_a_file_that_could_not_be_read() {
    let read = read_pdf(&reader("kill -SEGV $$", 5_000), b"%PDF".to_vec()).await;
    assert_eq!(why(read), "it could not be read");
}

#[tokio::test]
async fn a_reader_that_says_nothing_sensible_is_a_file_that_could_not_be_read() {
    let read = read_pdf(
        &reader("cat >/dev/null; printf nonsense", 5_000),
        b"%PDF".to_vec(),
    )
    .await;
    assert_eq!(why(read), "it could not be read");
}

#[tokio::test]
async fn a_scan_is_read_as_no_text() {
    let read = read_pdf(
        &reader("cat >/dev/null; printf '1 1\\n'", 5_000),
        b"%PDF".to_vec(),
    )
    .await;
    match read {
        PdfText::Read { text, kept, total } => {
            assert!(text.is_empty());
            assert_eq!((kept, total), (1, 1));
        }
        PdfText::Unreadable(why) => panic!("{why}"),
    }
}

#[tokio::test]
async fn what_a_reader_writes_back_is_capped() {
    let read = read_pdf(
        &reader(
            "cat >/dev/null; printf '1 1\\n'; head -c 2000000 /dev/zero | tr '\\\\000' a",
            10_000,
        ),
        b"%PDF".to_vec(),
    )
    .await;
    match read {
        PdfText::Read { text, .. } => {
            assert!(text.len() as u64 <= MAX_PDF_OUTPUT_BYTES, "{}", text.len());
        }
        PdfText::Unreadable(why) => panic!("{why}"),
    }
}

#[tokio::test]
async fn with_no_reader_installed_the_model_is_told_so() {
    let read = read_pdf(
        &PdfReader {
            bin: None,
            timeout: std::time::Duration::from_secs(1),
        },
        b"%PDF".to_vec(),
    )
    .await;
    assert_eq!(why(read), "this server has no PDF reader installed");
}

/// A staged file's name is a path component, so a name is kept to characters that can never
/// leave `~/office/inbox`: `..` goes to underscores, a slash too, and only a name that is
/// nothing at all earns a default.
#[test]
fn a_staged_name_stays_inside_the_inbox() {
    assert_eq!(staged_name("report.docx"), "report.docx");
    // Every `/` is flattened to `_`, so a `..` can only ever sit inside one safe component —
    // and leading dots are trimmed so the name cannot hide as a dotfile either.
    assert_eq!(staged_name("../escape.docx"), "_escape.docx");
    assert_eq!(staged_name("a/b/../../c.pptx"), "a_b_.._.._c.pptx");
    assert_eq!(staged_name("q3 plan (final).xlsx"), "q3_plan__final_.xlsx");
    assert_eq!(staged_name("..."), "document");
    assert_eq!(staged_name(""), "document");
    assert!(staged_name(&"é".repeat(200)).len() <= 80);
}
