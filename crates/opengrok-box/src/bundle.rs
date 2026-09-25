//! A set of files placed onto a computer as one unit, under one directory in its home.
//!
//! What a skill's bundle needs (#192): the files a body tells the coworker to read or run, on the
//! machine it runs them on. Provider-neutral — it is `run` and `write_file` on any `Computer`.
//!
//! PLAIN PATHS ONLY, CHECKED HERE AS WELL AS WHERE THEY WERE STORED. The paths reach a shell
//! (`chmod`, the Docker write), and a bundle is often somebody else's: a colleague's `a';curl …`
//! would run on the box of whoever invoked the skill. Every component must be `[A-Za-z0-9._-]`,
//! not `.`/`..`, not starting with `-`; anything else is skipped with the reason, never quoted.
//!
//! ONCE PER CONTENT, CHECKED AGAINST THE DISK. A `.bundle` file in the directory is a
//! `sha256sum` manifest of what was written, and a later turn writes nothing only when that
//! manifest is the one it would write AND `sha256sum -c` still passes on the box. It used to hold
//! one digest of what had been written, so a file a turn edited or deleted stayed that way for
//! every later turn of the same version, and the model was told it held the author's words. A
//! box with no `sha256sum` fails the check and is rewritten each turn: slower, never wrong.
//! Anything else (a new set, an edited file) clears the directory first, so no file of the
//! previous set is left beside the new one.

use sha2::{Digest, Sha256};

use crate::{BoxError, BoxResult, Computer};

/// What was placed, and what was not and why. Paths are relative to `dir`. `not_executable` are
/// written scripts whose `chmod +x` failed: they are there, but only run through an interpreter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    pub dir: String,
    pub written: Vec<String>,
    pub skipped: Vec<(String, String)>,
    pub not_executable: Vec<String>,
}

const MARKER: &str = ".bundle";

/// A path a shell can be handed inside single quotes without it meaning anything but itself.
pub fn plain_path(path: &str) -> bool {
    !path.is_empty()
        && path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.starts_with('-')
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        })
}

/// The `.bundle` manifest for `texts`: one `sha256sum` line per file, in the order given.
fn manifest(texts: &[(&str, &str)]) -> String {
    texts
        .iter()
        .map(|(path, text)| {
            let hex: String = Sha256::digest(text.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            format!("{hex}  {path}\n")
        })
        .collect()
}

fn refused(why: impl Into<String>) -> BoxError {
    BoxError::Refused {
        status: 400,
        body: why.into(),
    }
}

/// `$HOME` on the box, absolute. `~` is expanded by nothing on the way to a box — the Docker
/// write single-quotes it and box.ascii.dev takes the path literally — so it is asked for.
async fn home(computer: &dyn Computer, box_id: &str) -> BoxResult<String> {
    let said = computer.run(box_id, "printf %s \"$HOME\"", 30).await?;
    let home = said.stdout.trim();
    match home
        .strip_prefix('/')
        .map(|rest| rest.trim_end_matches('/'))
    {
        Some("") => Ok(String::new()),
        Some(rest) if plain_path(rest) => Ok(format!("/{rest}")),
        _ => Err(refused(
            "the computer did not say where its home directory is",
        )),
    }
}

/// Place `files` under `$HOME/<under_home>/`. `under_home` must be a plain path; so must every
/// file's, or it is skipped. Non-UTF-8 files are skipped too: `write_file` carries text, and a
/// lossy write would hand the coworker a file that is not the one in the bundle.
pub async fn place(
    computer: &dyn Computer,
    box_id: &str,
    under_home: &str,
    files: &[(String, Vec<u8>)],
) -> BoxResult<Placed> {
    if !plain_path(under_home) {
        return Err(refused(format!("{under_home:?} is not a plain directory")));
    }
    let dir = format!("{}/{under_home}", home(computer, box_id).await?);
    let mut placed = Placed {
        dir: dir.clone(),
        written: Vec::new(),
        skipped: Vec::new(),
        not_executable: Vec::new(),
    };
    let mut texts = Vec::new();
    for (path, bytes) in files {
        if !plain_path(path) || path == MARKER {
            placed
                .skipped
                .push((path.clone(), "not a plain path".into()));
        } else if let Ok(text) = std::str::from_utf8(bytes) {
            texts.push((path.as_str(), text));
        } else {
            placed
                .skipped
                .push((path.clone(), "not a text file".into()));
        }
    }
    let want = manifest(&texts);
    let have = computer
        .run(
            box_id,
            &format!("cd '{dir}' 2>/dev/null && sha256sum -c --status {MARKER} && cat {MARKER}"),
            30,
        )
        .await?;
    if have.exit_code == 0 && !want.is_empty() && have.stdout.trim_end() == want.trim_end() {
        placed.written = texts.iter().map(|(path, _)| path.to_string()).collect();
        return Ok(placed);
    }
    let cleared = computer
        .run(box_id, &format!("rm -rf '{dir}' && mkdir -p '{dir}'"), 30)
        .await?;
    if cleared.exit_code != 0 {
        return Err(refused(format!("could not make {dir}: {}", cleared.stderr)));
    }
    let mut runnable = Vec::new();
    let mut complete = true;
    for (path, text) in texts {
        match computer
            .write_file(box_id, &format!("{dir}/{path}"), text)
            .await
        {
            Ok(()) => {
                if text.starts_with("#!") {
                    runnable.push(path);
                }
                placed.written.push(path.to_string());
            }
            Err(error) => {
                complete = false;
                placed.skipped.push((path.to_string(), error.to_string()));
            }
        }
    }
    // A script arrives without its mode bit; a shebang is the author saying it is meant to run.
    // A chmod that fails is said, not swallowed: the model would otherwise be told the script is
    // there to run and meet "permission denied" — and the next turn tries again.
    if !runnable.is_empty() {
        let quoted: Vec<String> = runnable
            .iter()
            .map(|path| format!("'{dir}/{path}'"))
            .collect();
        let marked = computer
            .run(box_id, &format!("chmod +x {}", quoted.join(" ")), 30)
            .await;
        if !matches!(marked, Ok(ref output) if output.exit_code == 0) {
            complete = false;
            placed.not_executable = runnable.iter().map(|path| path.to_string()).collect();
        }
    }
    // A write that failed is tried again next turn; a file that can never be written is not.
    if complete {
        computer
            .write_file(box_id, &format!("{dir}/{MARKER}"), &want)
            .await?;
    }
    Ok(placed)
}
