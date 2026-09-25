//! How the gate reads a shell line (#203, #204). The daemon runs the whole line under `/bin/sh`,
//! so a rule judged on the raw string by prefix allowed anything chained after an allowed word
//! (`ls; rm -rf ~`) and missed a denied one written any other way (`/bin/rm`, `true; rm`).
//!
//! Two readers, one per side, because the two sides fail in opposite directions:
//! - `read` is for ALLOW. It follows quoting the way `sh` does and yields words only for ONE
//!   simple command with nothing it cannot follow. Anything else (an operator, a substitution, a
//!   redirection to a path, a subshell, a variable or assignment in front, a program that runs
//!   another) has no words, so no allow rule can vouch for it and a person is asked.
//! - `programs` is for DENY. It ignores quoting and splits on every operator and bracket, so text
//!   a quote would hide (`sh -c 'rm -rf ~'`) is read as well. That over-reads on purpose: a deny
//!   that fires on a quoted `; rm` refuses a command, which is the safe direction.
//!
//! No shell parser crate: `shlex` does not split on operators (`a&&b` is one token), and a full
//! bash grammar is far more than a gate that only ever says "Ask" when unsure. A deny rule is a
//! safety net, not a sandbox: `git -c alias.x='!rm' x`, `$(echo rm) x`, a glob in program
//! position (`/bin/r?`) or an interpreter's own string (`osascript -e …`) still pass a deny of
//! `rm`. Only `Never` closes the channel.

/// Words that run a command taken from their arguments, a string or stdin — shell keywords
/// included (`then rm`, `! rm`). An allow of one is an allow of anything, so a line whose program
/// is one of these never auto-runs; on the deny side every later word of such a line may be the
/// program.
const RUNS_ANOTHER: &str = "eval exec source . command builtin env nice nohup time timeout xargs \
    sudo doas sh bash zsh dash ksh fish csh tcsh trap caffeinate arch watch \
    ! if then else elif while until do coproc";

/// `find` arguments after which the words are a command.
const FIND_RUNS: &[&str] = &["-exec", "-execdir", "-ok", "-okdir"];

/// A line as the allow side reads it.
pub(super) struct Line {
    /// The simple commands between top-level control operators, as written. The whole line when
    /// it nests, since a split through `$( … )` or `{ …; }` would cut a command in half.
    pub simple_commands: Vec<String>,
    /// The words after quote removal, only when the line is one simple command the gate can
    /// follow. `None` means no allow rule may match it.
    pub plain: Option<Vec<String>>,
}

#[derive(Default)]
struct Reader {
    words: Vec<String>,
    word: String,
    in_word: bool,
    /// The current word had a quote or an escape, so digits in it are not a file descriptor.
    quoted: bool,
    /// The current word can expand (`$x`, a glob) into something the pattern cannot see.
    expands: bool,
    program_expands: bool,
    segments: Vec<String>,
    segment: String,
    opaque: bool,
    nested: bool,
}

impl Reader {
    fn end_word(&mut self) {
        if self.in_word {
            if self.words.is_empty() && self.expands {
                self.program_expands = true;
            }
            self.words.push(std::mem::take(&mut self.word));
        }
        self.in_word = false;
        self.quoted = false;
        self.expands = false;
    }

    fn end_segment(&mut self) {
        let text = self.segment.trim();
        if !text.is_empty() {
            self.segments.push(text.to_string());
        }
        self.segment.clear();
    }

    fn push(&mut self, c: char) {
        self.word.push(c);
        self.in_word = true;
    }
}

fn is_operator(c: char) -> bool {
    matches!(c, ';' | '&' | '|' | '\n' | '\r')
}

fn is_assignment(word: &str) -> bool {
    match word.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && !name.starts_with(|c: char| c.is_ascii_digit())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn runs_another(program: &str) -> bool {
    let name = basename(program).to_ascii_lowercase();
    RUNS_ANOTHER.split_whitespace().any(|word| word == name)
}

/// Read `line` the way `sh` would, as far as the allow side needs.
pub(super) fn read(line: &str) -> Line {
    let chars: Vec<char> = line.chars().collect();
    let mut r = Reader::default();
    let mut quote: Option<char> = None;
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        let next = chars.get(i + 1).copied();
        match quote {
            Some('\'') => {
                r.segment.push(c);
                if c == '\'' {
                    quote = None;
                } else {
                    r.push(c);
                }
            }
            Some(_) => {
                r.segment.push(c);
                match (c, next) {
                    ('"', _) => quote = None,
                    // Inside double quotes a backslash escapes only these; otherwise it is kept.
                    ('\\', Some(n @ ('$' | '`' | '"' | '\\' | '\n'))) => {
                        r.segment.push(n);
                        if n != '\n' {
                            r.push(n);
                        }
                        i += 1;
                    }
                    ('`', _) | ('$', Some('(')) => r.nested = true,
                    ('$', _) => {
                        r.expands = true;
                        r.push(c);
                    }
                    _ => r.push(c),
                }
            }
            None => match c {
                ' ' | '\t' => {
                    r.segment.push(c);
                    r.end_word();
                }
                '\\' => {
                    r.segment.push(c);
                    match next {
                        // A continuation joins the two halves into one word.
                        Some('\n') => r.segment.push('\n'),
                        Some(n) => {
                            r.segment.push(n);
                            r.quoted = true;
                            r.push(n);
                        }
                        None => r.opaque = true,
                    }
                    i += 1;
                }
                '\'' | '"' => {
                    r.segment.push(c);
                    r.in_word = true;
                    r.quoted = true;
                    quote = Some(c);
                }
                // A comment runs to the newline, and a backslash in it continues nothing: joining
                // `ls #\⏎rm -rf ~` into one word would allow the `rm` the shell runs next.
                '#' if !r.in_word => {
                    r.opaque = true;
                    while let Some(&k) = chars.get(i).filter(|&&k| k != '\n') {
                        r.segment.push(k);
                        i += 1;
                    }
                    continue;
                }
                '`' | '(' | ')' | '{' | '}' => {
                    r.segment.push(c);
                    r.nested = true;
                }
                '$' => {
                    r.segment.push(c);
                    match next {
                        Some('(') => r.nested = true,
                        // `$'…'` lets a backslash escape the closing quote; not followed here.
                        Some('\'') => r.opaque = true,
                        _ => r.expands = true,
                    }
                    r.push(c);
                }
                '*' | '?' | '[' => {
                    r.segment.push(c);
                    r.expands = true;
                    r.push(c);
                }
                '<' | '>' => {
                    // `2>&1`: one stream into another touches no path, so it is not a redirection
                    // the gate has to judge. Anything else aimed at a path, or a heredoc, is.
                    let digits = chars
                        .get(i + 2..)
                        .unwrap_or_default()
                        .iter()
                        .take_while(|d| d.is_ascii_digit())
                        .count();
                    let ends = chars
                        .get(i + 2 + digits)
                        .is_none_or(|&after| matches!(after, ' ' | '\t') || is_operator(after));
                    if next == Some('&') && digits > 0 && ends {
                        let is_fd = !r.quoted && r.word.chars().all(|d| d.is_ascii_digit());
                        if is_fd {
                            r.word.clear();
                            r.in_word = false;
                        }
                        r.end_word();
                        r.segment
                            .extend(chars.get(i..i + 2 + digits).unwrap_or_default());
                        i += 1 + digits;
                    } else {
                        r.segment.push(c);
                        r.opaque = true;
                    }
                }
                c if is_operator(c) => {
                    r.end_word();
                    r.end_segment();
                    r.opaque = true;
                }
                _ => {
                    r.segment.push(c);
                    r.push(c);
                }
            },
        }
        i += 1;
    }
    r.end_word();
    r.end_segment();
    let unjudgeable = r.opaque
        || r.nested
        || quote.is_some()
        || r.program_expands
        || r.words
            .first()
            .is_none_or(|program| is_assignment(program) || runs_another(program));
    let simple_commands = if r.nested {
        let whole = line.trim();
        if whole.is_empty() {
            Vec::new()
        } else {
            vec![whole.to_string()]
        }
    } else {
        r.segments
    };
    Line {
        simple_commands,
        plain: (!unjudgeable).then_some(r.words),
    }
}

/// Does the allow `pattern` cover the plain `words` of a command? Word-boundary prefix on the
/// words after quote removal, so `git status` covers `git  status --short` but not `git statusx`.
/// The program is compared as written: `/tmp/x/ls` is not `ls`. A pattern that is not itself one
/// plain command (`cd src && cargo test`) matches nothing.
pub(super) fn allows(pattern: &str, words: &[String]) -> bool {
    match read(pattern).plain {
        Some(pattern) => !pattern.is_empty() && words.starts_with(&pattern),
        None => false,
    }
}

/// Quotes, backslashes and the `$` of `$'…'` dropped rather than followed.
fn flatten(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' | '\\' => {}
            '$' if matches!(chars.peek(), Some('\'' | '"')) => {}
            _ => out.push(c),
        }
    }
    out
}

/// Every word run the shell could treat as a program, for the deny side: each piece between
/// operators and brackets, leading `NAME=value` assignments skipped, and after a program that
/// runs another (`env`, `sudo`, `sh -c`, `then`, `find -exec`…) every later word as well.
///
/// The line is read twice, with backslash-newline joined (`r\⏎m` is `rm`) and not joined (in a
/// comment it joins nothing, so `ls #\⏎rm` runs `rm`); whichever reading names a denied program
/// wins.
pub(super) fn programs(line: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let readings = [flatten(&line.replace("\\\n", "")), flatten(line)];
    for piece in readings.iter().flat_map(|reading| {
        reading
            .split(|c: char| is_operator(c) || matches!(c, '(' | ')' | '{' | '}' | '`' | '<' | '>'))
    }) {
        let words: Vec<&str> = piece
            .split_whitespace()
            .skip_while(|word| is_assignment(word))
            .collect();
        let starts = match words.first() {
            Some(program) if runs_another(program) => words.len(),
            Some(_) if words.iter().any(|word| FIND_RUNS.contains(word)) => words.len(),
            Some(_) => 1,
            None => 0,
        };
        for start in 0..starts {
            let run = words.get(start..).unwrap_or_default();
            out.push(run.iter().map(|word| word.to_string()).collect());
        }
    }
    out
}

/// Does the deny `pattern` name any of `programs`? The program by basename, any case (APFS runs
/// `/bin/rm` for `RM`); the rest as words, after any leading options, so `git push` still meets
/// `git -C . push` and `git --no-pager push`.
pub(super) fn denies(pattern: &str, programs: &[Vec<String>]) -> bool {
    let pattern: Vec<String> = flatten(&pattern.replace("\\\n", ""))
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let Some((program, args)) = pattern.split_first() else {
        return false;
    };
    programs.iter().any(|words| match words.split_first() {
        Some((first, rest)) => {
            basename(first).eq_ignore_ascii_case(basename(program)) && after_options(rest, args)
        }
        None => false,
    })
}

fn after_options(rest: &[String], args: &[String]) -> bool {
    let mut at = 0;
    loop {
        if rest.get(at..).unwrap_or_default().starts_with(args) {
            return true;
        }
        match rest.get(at) {
            Some(option) if option.starts_with('-') => {
                at += 1;
                // `-C .`: an option may take the next word as its value.
                let takes_value = !option.contains('=')
                    && rest.get(at).is_some_and(|value| !value.starts_with('-'));
                if takes_value {
                    if rest.get(at..).unwrap_or_default().starts_with(args) {
                        return true;
                    }
                    at += 1;
                }
            }
            _ => return false,
        }
    }
}
