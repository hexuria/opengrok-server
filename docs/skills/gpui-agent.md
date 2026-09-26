---
name: gpui-agent
description: Run the gpui-agent CLI. If it is missing, stop. Install it only when the message says install.
---

Use this skill when the command is `gpui-agent`.

## Missing CLI

Skip this section when their message asks to install. Otherwise, before any other `gpui-agent` command, run `command -v gpui-agent` on the sandbox with `shell`. If that fails, run the same check on the person's computer with `user_machine_shell`.

If both fail, stop. Say the CLI is not installed. Tell them to send `install` with this skill. Do not clone a repository. Do not run `cargo`. Do not run `gpui-agent hello`.

## Install

Do this section only when their message asks to install. Install the CLI and stop. Do not invoke an app.

Install on the computer they named. If they named none, install on the sandbox with `shell`.

If `command -v gpui-agent` already succeeds on that computer, say so and stop.

If `command -v cargo` fails there, say Rust is not installed and stop.

If that computer has no `gpui-agent` checkout, clone `https://github.com/hexuria/gpui-agent` into `~/src/gpui-agent`.

From that checkout, run `cargo install --path crates/gpui-agent-cli --locked=false`.

Then run `command -v gpui-agent`. If it fails, say the install failed and stop.

Do not install `todo-headless`. Do not set a host address. Do not pass a token. Do not print a token.

## Which computer

Run on the computer the person named. If they named none, use the first computer where `command -v gpui-agent` succeeded.

1. The coworker's sandbox, with `shell`.
2. The person's computer, with `user_machine_shell`.

## How to run it

Pass arguments as repeated `--arg key=value`. If a value parses as JSON, send that JSON value.

Do not set a host address. Do not pass a token. Do not print a token. Do not run `gpui-agent hello` to find a host. If a call is refused, do not try another address. The CLI picks the host.

On the person's computer, `~/.cargo/bin` is already on PATH.
