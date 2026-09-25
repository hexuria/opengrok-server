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
//!   a quote would hide (`sh -c 'rm -rf ~'`) is read as well, and it reads PAST any word the
//!   shell may leave out of argv: a redirection and its target (`2>&1 git push`,
//!   `>/dev/null rm`) and an expansion that may be empty (`git $x push`, `git "$@" push`). The
//!   allow side's own words are checked too, so every line an allow can auto-run is judged by
//!   deny exactly as allow read it. It over-reads on purpose: a deny that fires on a quoted
//!   `; rm` refuses a command, which is the safe direction.
//!
//! Both are linear in the line's length, and `decide` refuses a line past `MAX_JUDGED_BYTES`
//! unread: the deny reader once copied every suffix of a line and took gigabytes on 16 KB.
//!
//! No shell parser crate: `shlex` does not split on operators (`a&&b` is one token), and a full
//! bash grammar is far more than a gate that only ever says "Ask" when unsure. A deny rule is a
//! safety net, not a sandbox: `git -c alias.x='!rm' x`, `$(echo rm) x`, `git $x` with `x=push`,
//! or an interpreter's own string (`osascript -e …`) still pass a deny of `rm`. Only `Never`
//! closes the channel.

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
    /// it nests or redirects to a path: a split through `$( … )`, `{ …; }`, `>&out`, `&> out` or
    /// a heredoc's body cuts a command in half (`ls >&out` read as `ls >` and `out`), and the
    /// machine's own approval would be shown commands that are not in the line.
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
    redirects: bool,
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
                        r.redirects = true;
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
    // Bash evaluates an array subscript in `printf -v`, `read`, `declare`, `test -v`… with its
    // substitutions, even when the text arrived quoted: `printf -v 'a[$(id)]' x` runs `id`.
    let unjudgeable = r.opaque
        || r.nested
        || quote.is_some()
        || r.program_expands
        || r.words
            .iter()
            .any(|word| word.contains("$(") || word.contains('`'))
        || r.words
            .first()
            .is_none_or(|program| is_assignment(program) || runs_another(program));
    let simple_commands = if r.nested || r.redirects {
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

/// Quotes, backslashes and the `$` of `$'…'` dropped rather than followed. Deny patterns only.
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

/// One word as the deny side reads it.
pub(super) struct Word {
    /// Quotes and backslashes dropped, expansions kept as written.
    text: String,
    /// `text` with its expansions taken out, when it has any: what is left if each is empty, so
    /// `$x"push"` is `push`. The default of `${x:-rm}` stays in, since the shell may use it.
    bare: Option<String>,
    /// The shell may leave it out of argv, so a deny reads past it: it holds an expansion or a
    /// substitution (`git $x push` runs `git push`), or it is a redirection's descriptor or
    /// target (`2>&1 git push`, `>/dev/null rm`).
    may_vanish: bool,
    /// `*`, `?` or `[` beside a literal character: a file in the working directory may turn
    /// it into another name (`/bin/r?`, `git pus?`). A word of wildcards alone (`sudo ls *`)
    /// names no program in particular and would match every deny, so it is read as written.
    glob: bool,
}

impl Word {
    fn plain(word: &str) -> Self {
        Self {
            text: word.to_string(),
            bare: None,
            may_vanish: word.contains('$'),
            glob: is_glob(word),
        }
    }

    /// Could this word run `program`? By basename, in any case: APFS runs `/bin/rm` for `RM`.
    fn could_be(&self, program: &str) -> bool {
        let program = basename(program);
        let same = |text: &str| basename(text).eq_ignore_ascii_case(program);
        same(&self.text)
            || self.bare.as_deref().is_some_and(same)
            || (self.glob && glob(basename(&self.text), program, true))
    }

    fn is(&self, arg: &str) -> bool {
        self.text == arg
            || self.bare.as_deref() == Some(arg)
            || (self.glob && glob(&self.text, arg, false))
    }
}

fn is_glob(word: &str) -> bool {
    word.contains(['*', '?', '[']) && word.contains(|c: char| !matches!(c, '*' | '?' | '[' | ']'))
}

/// Could the glob `pattern` expand to `name`, in any case when `fold`? A bracket expression
/// counts as any one character, which can only widen a deny.
fn glob(pattern: &str, name: &str, fold: bool) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while let Some(&c) = name.get(n) {
        let step = match pattern.get(p) {
            Some('*') => {
                star = Some((p, n));
                p += 1;
                continue;
            }
            Some('?') => Some(p + 1),
            Some('[') => pattern
                .get(p + 1..)
                .and_then(|rest| rest.iter().skip(1).position(|&k| k == ']'))
                .map(|close| p + close + 3)
                .or((c == '[').then_some(p + 1)),
            Some(&k) if k == c || (fold && k.eq_ignore_ascii_case(&c)) => Some(p + 1),
            _ => None,
        };
        match (step, star) {
            (Some(next), _) => {
                p = next;
                n += 1;
            }
            (None, Some((at, from))) => {
                star = Some((at, from + 1));
                p = at + 1;
                n = from + 1;
            }
            (None, None) => return false,
        }
    }
    pattern
        .get(p..)
        .unwrap_or_default()
        .iter()
        .all(|&k| k == '*')
}

/// The words of one command between operators and brackets.
pub(super) struct Run {
    words: Vec<Word>,
    /// Its program runs another (`env`, `sh -c`, `then`, `find -exec`…), or a substitution
    /// closed inside it: any word may be the program.
    every: bool,
}

impl Run {
    fn new(words: Vec<Word>, every: bool) -> Option<Self> {
        if words.is_empty() {
            return None;
        }
        let mut every = every || words.iter().any(|w| FIND_RUNS.contains(&w.text.as_str()));
        for word in words.iter().filter(|w| !is_assignment(&w.text)) {
            every |= RUNS_ANOTHER
                .split_whitespace()
                .any(|name| word.could_be(name));
            if !word.may_vanish {
                break;
            }
        }
        Some(Self { words, every })
    }

    /// Does `program args…` run here? One backward pass, so the cost is the number of words
    /// times the pattern's: comparing every suffix instead cost gigabytes on a 16 KB line.
    ///
    /// The program is the first word after leading assignments and words that may vanish, or
    /// any word when `every`. The arguments may follow leading options (`git -C . push`), and a
    /// word that may vanish may be read past anywhere.
    fn names(&self, program: &str, args: &[String]) -> bool {
        // Most runs name no denied program at all; say so before allocating for the pass.
        if !self.words.iter().any(|word| word.could_be(program)) {
            return false;
        }
        let m = args.len();
        // `row[k]`: the words from the next one on start with `args[k..]`.
        let mut row: Vec<bool> = (0..=m).map(|k| k == m).collect();
        let mut spare = row.clone();
        // The same three facts for the next word and the one after it.
        let (mut starts_next, mut follows_next, mut follows_after) = (m == 0, m == 0, false);
        let mut program_next = false;
        for (j, word) in self.words.iter().enumerate().rev() {
            let here = word.could_be(program) && follows_next;
            if here && self.every {
                return true;
            }
            let skip_before_program = word.may_vanish || is_assignment(&word.text);
            let program_here = here || (skip_before_program && program_next);
            for (k, slot) in spare.iter_mut().enumerate() {
                *slot = k == m
                    || (args.get(k).is_some_and(|arg| word.is(arg))
                        && row.get(k + 1).copied().unwrap_or(false))
                    || (word.may_vanish && row.get(k).copied().unwrap_or(false));
            }
            std::mem::swap(&mut row, &mut spare);
            let starts = row.first().copied().unwrap_or(false);
            // `-C .`: an option may take the next word as its value.
            let option = word.text.starts_with('-');
            let takes_value = option
                && !word.text.contains('=')
                && self
                    .words
                    .get(j + 1)
                    .is_some_and(|value| !value.text.starts_with('-'));
            let follows = starts
                || (word.may_vanish && follows_next)
                || (takes_value && (starts_next || follows_after))
                || (option && !takes_value && follows_next);
            (starts_next, follows_after, follows_next) = (starts, follows_next, follows);
            program_next = program_here;
        }
        program_next
    }
}

#[derive(Default)]
struct Cut {
    words: Vec<Word>,
    text: String,
    bare: String,
    in_word: bool,
    expands: bool,
    /// The word being read is a redirection's descriptor or target.
    target: bool,
    every: bool,
}

impl Cut {
    fn push(&mut self, c: char, literal: bool) {
        self.text.push(c);
        if literal {
            self.bare.push(c);
        } else {
            self.expands = true;
        }
        self.in_word = true;
    }

    fn end_word(&mut self) {
        if self.in_word {
            let text = std::mem::take(&mut self.text);
            let bare = std::mem::take(&mut self.bare);
            self.words.push(Word {
                glob: is_glob(&text),
                bare: self.expands.then_some(bare),
                may_vanish: self.expands || self.target,
                text,
            });
            self.target = false;
        }
        self.in_word = false;
        self.expands = false;
    }

    /// Just past a `$`. The name goes into `text` only.
    fn expansion(&mut self, chars: &[char], mut i: usize) -> usize {
        match chars.get(i).copied() {
            Some('{') => {
                self.push('$', false);
                self.push('{', false);
                i += 1;
                let mut default = false;
                while let Some(&c) = chars.get(i) {
                    match c {
                        '}' => {
                            self.push(c, false);
                            return i + 1;
                        }
                        '\'' | '"' | '\\' => {}
                        '-' | '=' | '+' if !default => {
                            default = true;
                            self.push(c, false);
                        }
                        c if c.is_whitespace()
                            || is_operator(c)
                            || matches!(c, '(' | ')' | '`' | '<' | '>' | '{' | '$') =>
                        {
                            return i;
                        }
                        c => self.push(c, default),
                    }
                    i += 1;
                }
                i
            }
            Some(c) if c.is_ascii_digit() || "@*#?-$!".contains(c) => {
                self.push('$', false);
                self.push(c, false);
                i + 1
            }
            Some(c) if c == '_' || c.is_ascii_alphabetic() => {
                self.push('$', false);
                while let Some(&c) = chars
                    .get(i)
                    .filter(|c| **c == '_' || c.is_ascii_alphanumeric())
                {
                    self.push(c, false);
                    i += 1;
                }
                i
            }
            _ => {
                self.push('$', true);
                i
            }
        }
    }

    /// Just past the first character of a redirection operator. The descriptor in front of it
    /// (`2>`) and the word after it are not arguments. `<(…)` is a command; its `(` is read next.
    fn redirect(&mut self, chars: &[char], mut i: usize) -> usize {
        if self.in_word && !self.expands && self.text.chars().all(|d| d.is_ascii_digit()) {
            self.target = true;
        }
        self.end_word();
        let mut taken = 0;
        while taken < 2
            && chars
                .get(i)
                .is_some_and(|&c| matches!(c, '<' | '>' | '&' | '|' | '-'))
        {
            i += 1;
            taken += 1;
        }
        self.target = chars.get(i) != Some(&'(');
        i
    }
}

/// An open `$(` or backtick, with the command it sits in.
struct Frame {
    close: char,
    depth: usize,
    outer: Cut,
}

#[derive(Default)]
struct Splitter {
    runs: Vec<Run>,
    cut: Cut,
    frames: Vec<Frame>,
}

impl Splitter {
    fn end_run(&mut self) {
        self.cut.end_word();
        let cut = std::mem::take(&mut self.cut);
        self.runs.extend(Run::new(cut.words, cut.every));
    }

    fn open(&mut self, close: char) {
        self.cut.in_word = true;
        self.cut.expands = true;
        let outer = std::mem::take(&mut self.cut);
        self.frames.push(Frame {
            close,
            depth: 0,
            outer,
        });
    }

    /// The substitution's own commands are runs; the word it sat in may vanish. A quote the
    /// reader dropped can close it early (`$(env -u')' rm x)`), so the rest of the outer
    /// command is read as if each word could be a program, as splitting there used to.
    fn close(&mut self) {
        if let Some(frame) = self.frames.pop() {
            self.end_run();
            self.cut = frame.outer;
            self.cut.every = true;
        }
    }

    fn finish(mut self) -> Vec<Run> {
        self.end_run();
        while let Some(frame) = self.frames.pop() {
            self.cut = frame.outer;
            self.end_run();
        }
        self.runs
    }
}

/// Split `line` into runs, ignoring quotes so the text of `sh -c '…'` is read too.
fn split(line: &str) -> Vec<Run> {
    let chars: Vec<char> = line.chars().collect();
    let mut s = Splitter::default();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        i += 1;
        let next = chars.get(i).copied();
        // Backticks do not nest unescaped, so one always closes an open backtick.
        let closes = |close: char, s: &Splitter| {
            s.frames
                .last()
                .is_some_and(|f| f.close == close && (close == '`' || f.depth == 0))
        };
        match c {
            '\'' | '"' | '\\' => {}
            '$' if matches!(next, Some('\'' | '"')) => {}
            '$' if next == Some('(') => {
                i += 1;
                s.open(')');
            }
            '$' => i = s.cut.expansion(&chars, i),
            '`' if closes('`', &s) => s.close(),
            '`' => s.open('`'),
            ')' if closes(')', &s) => s.close(),
            '(' | ')' => {
                if let Some(frame) = s.frames.last_mut() {
                    frame.depth = if c == '(' {
                        frame.depth + 1
                    } else {
                        frame.depth.saturating_sub(1)
                    };
                }
                s.end_run();
            }
            ' ' | '\t' => s.cut.end_word(),
            '<' | '>' => i = s.cut.redirect(&chars, i),
            '&' if next == Some('>') => i = s.cut.redirect(&chars, i),
            c if is_operator(c) || matches!(c, '{' | '}') => s.end_run(),
            c => s.cut.push(c, true),
        }
    }
    s.finish()
}

/// Every command the shell could run in `line`, for the deny side: each piece between operators
/// and brackets, the inside of each substitution, and after a program that runs another every
/// later word as well. `plain` is the allow side's reading, when it has one: the line a rule can
/// auto-run is always checked the way the allow side read it.
///
/// The line is read with backslash-newline joined (`r\⏎m` is `rm`) and, when it has one, not
/// joined (in a comment it joins nothing, so `ls #\⏎rm` runs `rm`).
pub(super) fn programs(line: &str, plain: Option<&[String]>) -> Vec<Run> {
    let mut runs = split(&line.replace("\\\n", ""));
    if line.contains("\\\n") {
        runs.extend(split(line));
    }
    if let Some(words) = plain {
        runs.extend(Run::new(
            words.iter().map(|w| Word::plain(w)).collect(),
            false,
        ));
    }
    runs
}

/// Does the deny `pattern` name any of `runs`? The program by basename, any case; the rest as
/// words, after any leading options, so `git push` still meets `git -C . push`.
pub(super) fn denies(pattern: &str, runs: &[Run]) -> bool {
    let pattern: Vec<String> = flatten(&pattern.replace("\\\n", ""))
        .split_whitespace()
        .map(str::to_string)
        .collect();
    match pattern.split_first() {
        Some((program, args)) => runs.iter().any(|run| run.names(program, args)),
        None => false,
    }
}
