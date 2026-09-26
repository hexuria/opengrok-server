---
name: bir
description: Work BIR tax profiles, forms, dues, and filing with gpui-agent. If headless BIR and eBIRForms are missing, stop. Install only when the message says install.
---

Requires skill: gpui-agent

Use this skill when the person asks about BIR profiles, forms sets, dues, drafts, or filing.

Follow the `gpui-agent` skill for how to run the CLI. Do not set a host address in this skill. Do not install the CLI from this skill. If `gpui-agent` is missing and their message does not ask to install, stop and tell them to send `install` with the `gpui-agent` skill.

## Missing program

Skip this section when their message asks to install. Otherwise, before any invoke, check these and stop at the first one that exists. Do not search the whole disk.

1. On the sandbox, with `shell`, run `command -v bir-headless`.
2. On the person's computer, with `user_machine_shell`, test for `/Applications/eBIRForms.app` on macOS, or `eBIRForms.exe` on Windows. Then run `command -v bir-headless`.

If none of those exist, stop. Say headless BIR and eBIRForms are not installed. Tell them to send `install` with this skill. Do not clone a repository. Do not run `cargo`. Do not run `gpui-agent`.

## Install

Do this section only when their message asks to install. Install headless BIR and stop. Do not file a return.

Install on the computer they named. If they named none, install on the sandbox with `shell`.

If `command -v bir-headless` already succeeds there, say so and stop.

If `command -v cargo` fails there, say Rust is not installed and stop.

If that computer has no buwiz-forms checkout, clone `https://github.com/hexuria/buwiz-forms` into `~/src/buwiz-forms`.

From that checkout, run `cargo install --path crates/bir-desktop --bin bir-headless --features agent --locked`.

Then run `command -v bir-headless`. If it fails, say the install failed and stop.

Build the Mac app only when they ask for the app, and only on their Mac. From that checkout, run `just _package-mac --agent`. Do not replace an existing `eBIRForms.app` until they confirm.

Do not set a host address. Do not pass a token. Do not print a token.

## Which app

Use the program the missing-program check found. The sandbox hit is headless BIR. A hit on the person's computer is `eBIRForms.app`, `eBIRForms.exe`, or `bir-headless`, in that order.

If the BIR window is open, use that program. Do not start headless BIR on the same live database. If the BIR window is closed, run `bir-headless serve --wait` against the database path for this work. Do not let two programs write that database.

## First call

The first call is `gpui-agent invoke profile.search` or `gpui-agent invoke profile.list` on the computer you selected above. Do not run `find`, `mdfind`, or `which`. Do not run `gpui-agent hello` before `profile.search` or `profile.list`. A live app-group database needs no `BIR_DATABASE_PATH`. A demo database or a box database needs an explicit path.

## Arguments

Pass arguments as repeated `--arg key=value`. If a value parses as JSON, send that JSON value. `year=2026` is a number. `confirm=true` is a boolean. Do not pass a JSON object as a positional argument.

`profile.search` and `profile.set` take `--arg q=Juan Dela Cruz` or `--arg tin=00000000000000`. The field name is `q`. Do not send `query`.

`profile.forms_set.get` requires `--arg year=2026`. The selected profile is enough. Add `--arg tin=` only when no profile is selected. If they did not name a year, use the current calendar year. Answer from the `codes`, `entries`, and `empty` fields.

`profile.forms_set` writes. It needs `--arg confirm=true`, and only after they confirm.

## Invokes

This is the full list. Call them with `gpui-agent invoke`. Do not search the disk for more names.

`profile.search`, `profile.list`, `profile.set`, `profile.forms_set.get`, `dues.list`, `nav.go`, `form.fill`, `form.fields`, `form.save_draft`, `form.pdf`, `filing.validate`.

Never call `profile.ensure`. Never call `form.set-value`. `set-value` is not an invoke. `form.fill` writes a tax return. It does not write a profile.

## New profile

`profile.create` opens a blank editor. It ignores `name` and `tin`. `profile.save` reads that editor and takes no arguments. Do not call `profile.save` until the fields are filled.

When they ask to create a profile, call `request_user_form` with `collect` true before any save. Title it "New tax profile". Every field is required. The fields are `name`, `tin`, `rdo`, `line_of_business`, `address`, `zip`, `phone`, and `email`. Put what they already said on `value` for the name, and put the TIN on `value` as digits. Leave `value` off any field they did not give. Wait for the card.

After they submit, write each shared field with one positional command. Do not use invoke.

`gpui-agent set-value profile-name 'Juana Jane'`

`gpui-agent set-value profile-tin 00000000000001`

The targets are `profile-name`, `profile-tin`, `profile-rdo`, `profile-lob`, `profile-address`, `profile-zip`, `profile-phone`, and `profile-email`. Then tell them the editor is filled, and ask before `gpui-agent invoke profile.save`. The profile exists only after that save returns ok.

If a search comes back empty, say that. Do not search again with the same words. Never skip a lock screen, PIN, or TOTP.

## Reply style

Run the tool before you describe what you will do. Put the codes and deadlines in the first sentence. Do not repeat the same plan sentence.
