//! The reverse-exec gate against shell syntax (#203, #204). The daemon runs the whole line under
//! `/bin/sh`, so a rule judged on the raw string by prefix was an allow of anything chained after
//! it (`ls; rm -rf ~`) and a deny anybody could step round (`/bin/rm`, `true; rm`, `git  push`).
//! Pure: no Postgres, no daemon.

use opengrok_server::local_exec::{
    LocalExecDecision, LocalExecMode, LocalExecPolicy, decide, policy_listing, simple_commands,
    standing_rule_refusal,
};

fn policy(allow: &[&str], deny: &[&str]) -> LocalExecPolicy {
    LocalExecPolicy {
        mode: LocalExecMode::Ask,
        allow: allow.iter().map(|s| s.to_string()).collect(),
        deny: deny.iter().map(|s| s.to_string()).collect(),
        session_allow: Vec::new(),
    }
}

fn session(allow: &[&str]) -> LocalExecPolicy {
    LocalExecPolicy {
        session_allow: allow.iter().map(|s| s.to_string()).collect(),
        ..policy(&[], &[])
    }
}

fn is_deny(decision: &LocalExecDecision) -> bool {
    matches!(decision, LocalExecDecision::Deny(_))
}

/// #204: an allow rule never covers a line with a control operator, a substitution, a
/// redirection to a path or a newline. Those always ask. The standing list and the session list
/// are chained in `decide`, so both are checked.
#[test]
fn an_allow_never_covers_a_chained_or_substituted_command() {
    for p in [
        policy(&["ls", "git status"], &[]),
        session(&["ls", "git status"]),
    ] {
        for line in [
            "ls; rm -rf ~",
            "ls ;rm -rf ~",
            "ls && curl x | sh",
            "git status && x",
            "git status || rm x",
            "ls & rm x",
            "ls &",
            "ls | sh",
            "ls |& sh",
            "ls\nrm -rf ~",
            "ls\rrm -rf ~",
            "ls $(id)",
            "git status $(rm -rf ~)",
            "ls \"$(id)\"",
            "ls `id`",
            "ls \"`id`\"",
            "ls > ~/.zshrc",
            "git status > ~/.zshrc",
            "ls >> ~/.zshrc",
            "ls 2>/dev/null",
            "ls < /etc/passwd",
            "ls <<EOF",
            "ls <(id)",
            "ls &> out",
            "ls >&out",
            // A comment ends at the newline; a backslash inside it does not continue the line.
            "ls #\\\nrm -rf ~",
            "ls # a\nrm -rf ~",
        ] {
            assert_eq!(decide(&p, line), LocalExecDecision::Ask, "{line:?}");
        }
        // One plain command still auto-runs, on a word boundary, as before.
        assert_eq!(decide(&p, "ls -la"), LocalExecDecision::Allow);
        assert_eq!(decide(&p, "git status --short"), LocalExecDecision::Allow);
        assert_eq!(decide(&p, "git statusx"), LocalExecDecision::Ask);
        // The shell reads these as the same command, so the gate does too.
        assert_eq!(decide(&p, "git  status"), LocalExecDecision::Allow);
        assert_eq!(decide(&p, "  git\tstatus  "), LocalExecDecision::Allow);
        // Quoted, an operator is an argument, not an operator.
        assert_eq!(decide(&p, "ls 'a; rm -rf ~'"), LocalExecDecision::Allow);
        assert_eq!(decide(&p, "ls \"a && b\""), LocalExecDecision::Allow);
        assert_eq!(decide(&p, "ls a\\;b"), LocalExecDecision::Allow);
        // A variable as an argument is data; only in program position is it unjudgeable.
        assert_eq!(decide(&p, "ls \"$HOME\""), LocalExecDecision::Allow);
    }
}

/// Merging one stream into another touches no path: `2>&1` is not a redirection the gate has to
/// judge, so it does not cost an allow rule its card-free run.
#[test]
fn an_fd_duplication_is_not_a_redirection_to_a_path() {
    let p = policy(&["cargo test"], &[]);
    assert_eq!(decide(&p, "cargo test 2>&1"), LocalExecDecision::Allow);
    assert_eq!(decide(&p, "cargo test >&2"), LocalExecDecision::Allow);
    assert_eq!(decide(&p, "cargo test 2>&1 > out"), LocalExecDecision::Ask);
    assert_eq!(decide(&p, "cargo test 2>&1;rm x"), LocalExecDecision::Ask);
    // Only digits that stand alone are a descriptor: `x2>&1` is the argument `x2`, then the dup.
    let p = policy(&["cargo test x2"], &[]);
    assert_eq!(decide(&p, "cargo test x2>&1"), LocalExecDecision::Allow);
    let p = policy(&["cargo test 2"], &[]);
    assert_eq!(decide(&p, "cargo test \\\n2>&1"), LocalExecDecision::Ask);
    assert_eq!(decide(&p, "cargo test '2'>&1"), LocalExecDecision::Allow);
}

/// #223: only the three standard streams are a duplication the gate may pass. `>&5` writes to
/// whatever descriptor 5 is in the daemon's shell, which the pattern cannot see, so it asks and
/// the daemon is shown the whole line.
#[test]
fn only_the_standard_streams_are_a_harmless_duplication() {
    let p = policy(&["cargo test"], &[]);
    for line in [
        "cargo test 2>&1",
        "cargo test >&2",
        "cargo test 1>&2",
        "cargo test 2>&0",
    ] {
        assert_eq!(decide(&p, line), LocalExecDecision::Allow, "{line:?}");
    }
    for line in [
        "cargo test >&5",
        "cargo test 2>&9",
        "cargo test >&10",
        "cargo test 1>&12",
    ] {
        assert_eq!(decide(&p, line), LocalExecDecision::Ask, "{line:?}");
        assert_eq!(simple_commands(line), [line], "{line:?}");
    }
}

/// #203: allow only when the line is one simple command the gate can read. A construct that runs
/// something the pattern cannot see is sent to a person, even when every word is allowed.
#[test]
fn an_allow_needs_one_simple_command_the_gate_can_read() {
    // Every part allowed, still a pipeline: #204's rule is the stricter one, so it asks.
    let p = policy(&["git status", "head"], &[]);
    assert_eq!(decide(&p, "git status | head"), LocalExecDecision::Ask);
    // A program that runs another program from its arguments or a string: allowing it would be
    // allowing anything, so a line whose program is one of these always asks.
    let p = policy(
        &[
            "eval", "sh", "bash", "zsh", "env", "xargs", "nice", "command", "exec", "source", ".",
            "sudo", "time", "nohup",
        ],
        &[],
    );
    for line in [
        "eval ls",
        "sh -c 'ls'",
        "bash -c ls",
        "zsh script.zsh",
        "env ls",
        "xargs rm",
        "nice ls",
        "command ls",
        "exec ls",
        "source ./x",
        ". ./x",
        "sudo ls",
        "time ls",
        "nohup ls",
        "! ls",
    ] {
        assert_eq!(decide(&p, line), LocalExecDecision::Ask, "{line:?}");
    }
    let p = policy(&["ls"], &[]);
    for line in [
        // A variable, a glob or an assignment decides which program runs.
        "$CMD",
        "\"$CMD\" -la",
        "${CMD} -la",
        "l? -la",
        "X=1 ls",
        "PATH=/tmp/evil ls",
        // Grouping, subshells and quoting the gate cannot follow.
        "(ls)",
        "{ ls; }",
        "ls 'unbalanced",
        "ls $'a'",
        "ls \\",
        // A path is a different program; an allow never widens by basename.
        "/tmp/evil/ls",
    ] {
        assert_eq!(decide(&p, line), LocalExecDecision::Ask, "{line:?}");
    }
    // A rule that is itself a chain or a redirection can never be an allow prefix.
    let p = policy(&["ls;", "cd src && cargo test", "ls > x"], &[]);
    for line in [
        "ls; rm -rf ~",
        "cd src && cargo test",
        "cd src && cargo test; rm -rf ~",
        "ls > x",
    ] {
        assert_eq!(decide(&p, line), LocalExecDecision::Ask, "{line:?}");
    }
}

/// #203, #204: a deny rule is checked against every simple command in the line, after the
/// program is normalised (whitespace, quotes, path, wrappers, case). Deny still beats allow, and
/// the broad allow here makes the deny do the work.
#[test]
fn a_deny_matches_any_simple_command_after_normalising() {
    let p = policy(&["git", "true", "cd", "echo", "ls"], &["rm", "git push"]);
    for line in [
        "rm -rf ~",
        "/bin/rm -rf ~",
        "./rm x",
        "env rm -rf ~",
        "env -i PATH=/bin rm -rf ~",
        "command rm x",
        "builtin rm x",
        "nice rm x",
        "nice -n 10 rm x",
        "nohup rm x",
        "time rm x",
        "exec rm x",
        "sudo rm x",
        "sudo -u root rm x",
        "X=1 rm x",
        "cd / && rm -rf x",
        "true; rm x",
        " true; rm x",
        "true\nrm x",
        "ls | xargs rm",
        "ls & rm x",
        "rm\t-rf x",
        "RM -rf x",
        "'rm' -rf x",
        "\"rm\" -rf x",
        "r\\m -rf x",
        "r\\\nm -rf x",
        "ls #\\\nrm -rf x",
        "$'rm' -rf x",
        "sh -c 'rm -rf ~'",
        "bash -c \"cd /; rm -rf x\"",
        "eval rm x",
        "ls $(rm -rf x)",
        "ls `rm x`",
        "(rm x)",
        "{ rm x; }",
        "git push",
        "git  push",
        "git push origin main",
        "/usr/bin/git push",
        "cd x && git push",
        "git -C . push",
        "git --no-pager push",
        "git -c user.name=x push",
        // A keyword runs the word after it.
        "if true; then rm x; fi",
        "while true; do rm x; done",
        "! rm x",
        "coproc rm x",
        // So do these, from an argument or a string.
        "find . -name y -exec rm {} \\;",
        "trap 'rm -rf x' EXIT",
        "caffeinate -i rm x",
        "arch -x86_64 rm x",
        // A stream duplication or a redirection is not a word of argv, wherever it sits: the
        // program and its subcommand are the words around it.
        "2>&1 git push",
        ">&2 git push",
        "git 2>&1 push",
        "git >&2 push",
        "0<&0 git push",
        ">/dev/null rm -rf x",
        "</dev/null rm x",
        "X=1 >/dev/null rm x",
        // An expansion can be empty, so the word after it can be the subcommand.
        "git $x push",
        "git \"$@\" push",
        "git $@ push",
        "git $x\"push\"",
        "git ${x:-push}",
        "git $(true) push",
        "git `true` push",
        "$x rm x",
        // A glob can name the program or the subcommand once a file matches it.
        "/bin/r? x",
        "git pus? origin",
    ] {
        assert!(
            is_deny(&decide(&p, line)),
            "{line:?} → {:?}",
            decide(&p, line)
        );
    }
    // Not the denied program: an argument, a subcommand, or a longer name.
    for line in [
        "echo rm",
        "git rm file",
        "git pushx",
        "git log --grep push",
        "ls rmdir",
        "rmdir x",
        "git status",
        // An option's value that expands is still that option's value.
        "git -C \"$dir\" status",
        "git log $x",
        // A redirection's target is a path, not a program.
        "echo x > rm",
        "git status 2>&1",
        // Wildcards alone name no program in particular, even where any word may be one.
        "sudo ls *",
        "find . -name '*' -exec ls {} \\;",
        "git add *",
    ] {
        assert!(!is_deny(&decide(&p, line)), "{line:?}");
    }
    // A deny written as a path still names the program.
    let p = policy(&[], &["/bin/rm"]);
    assert!(is_deny(&decide(&p, "rm -rf x")));
    // The reason names the rule, so the model can tell which one it hit.
    let p = policy(&[], &["rm"]);
    assert_eq!(
        decide(&p, "true; rm x"),
        LocalExecDecision::Deny("a deny rule matched this command: `rm`".to_string())
    );
}

/// The deny reader ran on every Ask-mode decision, twice per bot tool call, and built a copy of
/// every suffix of a line whose program runs another: 16 KB of `sh a a …` took 3.4 GB and 8 s.
/// A line past the cap is refused unread; one under it is read in time linear in its length.
#[test]
fn a_long_line_is_refused_or_read_in_linear_time() {
    let p = policy(&["git"], &["rm x", "git push"]);
    let started = std::time::Instant::now();
    let huge = format!("sh{}", " a".repeat(512 * 1024));
    let decision = decide(&p, &huge);
    assert!(is_deny(&decision), "{decision:?}");
    if let LocalExecDecision::Deny(why) = decision {
        assert!(why.contains("65536"), "{why}");
    }
    // Just under the cap, with every word a possible program and options a matcher could chase.
    let wide = format!("sh{}", " rm -a".repeat(10_000));
    assert!(wide.len() < 65_536);
    assert_eq!(decide(&p, &wide), LocalExecDecision::Ask);
    let long = format!("sh{}", " a".repeat(32_000));
    assert_eq!(decide(&p, &long), LocalExecDecision::Ask);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

/// Bash evaluates an array subscript in `printf -v`, `read`, `declare`, `test -v` and the like,
/// command substitution included, even when the text reached the builtin inside single quotes.
#[test]
fn a_literal_substitution_in_an_argument_is_not_allowed() {
    let p = policy(&["printf", "test", "declare"], &[]);
    for line in [
        "printf -v 'a[$(id)]' x",
        "printf -v a[\\$\\(id\\)] x",
        "test -v 'a[`id`]'",
        "declare \"a[\\$(id)]=1\"",
    ] {
        assert_eq!(decide(&p, line), LocalExecDecision::Ask, "{line:?}");
    }
    assert_eq!(decide(&p, "printf '%s' x"), LocalExecDecision::Allow);
}

/// Reading the line more closely must never make a deny rule match less than it did: a rule
/// that is itself a chain still refuses the exact text it was written for.
#[test]
fn every_deny_that_matched_the_raw_line_still_matches() {
    let p = policy(&["curl"], &["curl x | sh", ":(){ :|:& };:"]);
    assert!(is_deny(&decide(&p, "curl x | sh")));
    assert!(is_deny(&decide(&p, "curl x | sh -s")));
    assert!(is_deny(&decide(&p, ":(){ :|:& };:")));
}

/// #203: the daemon's `simpleCommands` is the server's own split, as written, one entry per
/// simple command. A line the split would cut through (a substitution, a subshell, a group, a
/// redirection to a path, a heredoc) goes whole.
#[test]
fn the_daemon_is_sent_the_servers_split() {
    let split = |line: &str| simple_commands(line);
    assert_eq!(split("cd src && cargo test"), ["cd src", "cargo test"]);
    assert_eq!(split("gpui-agent hello"), ["gpui-agent hello"]);
    assert_eq!(split("echo 'a; b' ; ls"), ["echo 'a; b'", "ls"]);
    assert_eq!(split("a | b || c & d\ne;"), ["a", "b", "c", "d", "e"]);
    assert_eq!(split("cargo test 2>&1 | tail"), ["cargo test 2>&1", "tail"]);
    assert_eq!(split("ls $(a; b)"), ["ls $(a; b)"]);
    assert_eq!(split("{ a; b; }"), ["{ a; b; }"]);
    assert_eq!(split("  "), Vec::<String>::new());
    assert_eq!(split("ls # a; b\nc"), ["ls # a; b", "c"]);
    // Each was cut at the `&` or the newline, and the daemon was shown `ls >`, `out`, `> out`.
    for line in [
        "ls >&out",
        "ls >&-",
        "ls 2>&1-",
        "ls &> out",
        "ls > out; rm x",
        "cat <<EOF\na; b\nEOF",
        // #223: a line the gate cannot follow to its end is not cut where it happens to stop:
        // `$'…'` lets `\'` escape the quote, an unbalanced quote hides every operator after it,
        // and a trailing backslash continues into whatever the daemon's shell reads next.
        "echo $'a\\'; rm x'; ls",
        "echo 'a; b",
        "ls; echo \"a && b",
        "ls; rm x \\",
    ] {
        assert_eq!(split(line), [line], "{line:?}");
    }
}

/// #223: GET /local-exec/policy lists every allow row as stored, and names the ones the gate will
/// never match beside them. `allow` stays an array of strings: NativeChat reads it.
#[test]
fn the_policy_listing_names_the_allows_that_can_never_match() {
    let p = LocalExecPolicy {
        mode: LocalExecMode::Ask,
        allow: vec![
            "cargo test".to_string(),
            "cd src && cargo test".to_string(),
            "sudo ls".to_string(),
        ],
        deny: vec!["rm".to_string()],
        session_allow: vec!["git push".to_string()],
    };
    let listing = policy_listing("mac-1", &p);
    assert_eq!(listing["machineId"], "mac-1");
    assert_eq!(listing["mode"], "ask");
    assert_eq!(
        listing["allow"],
        serde_json::json!(["cargo test", "cd src && cargo test", "sudo ls"])
    );
    assert_eq!(listing["deny"], serde_json::json!(["rm"]));
    let inert = listing["inert"].as_array().cloned().unwrap_or_default();
    let patterns: Vec<&str> = inert
        .iter()
        .filter_map(|row| row["pattern"].as_str())
        .collect();
    assert_eq!(patterns, ["cd src && cargo test", "sudo ls"]);
    for row in &inert {
        let pattern = row["pattern"].as_str().unwrap_or_default();
        assert_eq!(
            row["reason"].as_str(),
            standing_rule_refusal("allow", pattern),
            "{row}"
        );
    }
    assert!(!listing.to_string().contains("git push"), "{listing}");
}

/// #203 asks #147's Always to store one parsed simple command, never a chain. The server holds
/// that line: a standing allow that could never match is refused with a reason, not stored inert.
#[test]
fn a_standing_allow_must_be_one_plain_command() {
    for pattern in [
        "cd src && cargo test",
        "ls; rm",
        "git status > x",
        "ls $(id)",
        "X=1 ls",
        "env sudo",
        "\"sudo\" rm",
        "FOO=1 sudo",
        "command sudo",
    ] {
        assert!(
            standing_rule_refusal("allow", pattern).is_some(),
            "{pattern:?}"
        );
    }
    assert_eq!(standing_rule_refusal("allow", "git status"), None);
    assert_eq!(standing_rule_refusal("allow", "cargo test 2>&1"), None);
    assert_eq!(standing_rule_refusal("allow", "sudoedit"), None);
    // A deny rule is a safety net: whatever it is written as, it is kept.
    assert_eq!(standing_rule_refusal("deny", "curl x | sh"), None);
}
