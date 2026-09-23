---
name: drive-bir
description: Drive eBIRForms on Uriah's Mac via gpui-agent. Pick a BIR host, then forms, dues, or filing. Never profile.ensure. Save or queue only after he confirms.
---

Use when Uriah asks about eBIRforms / BIR profiles, forms sets, dues, drafts, or filing via NativeChat.

## Host selection (one owner)

1. The host address is `GPUI_AGENT_ADDR=127.0.0.1:17423`. eBIRForms on this Mac listens there. Put it on the catalog invoke. Do not run `gpui-agent hello` before `profile.search` or `profile.list`. Do not pass `--token` and do not print `GPUI_AGENT_TOKEN`. The host already has it. Do not ask him to paste it.
2. The invoke answer must be from `bir-desktop` (or the BIR app id). If this port is a different app, try `127.0.0.1:17421`. Connection refused means nothing is listening. Remember the last address that answered `bir-desktop` and try it first next turn.
3. If painted BIR is open and agent-capable, drive that host. Do not start headless on the same live DB.
4. If painted BIR is closed, use `bir-headless serve --wait` against the intended DB path.
5. Never dual-write. Tray Quit or `gpui-agent shutdown` releases bind and lock. Hide / red-close does not. A second writer should see `LiveDatabaseInUse` or a bind failure — stop.

## Mac execution

The first call is `GPUI_AGENT_ADDR=127.0.0.1:17423 gpui-agent invoke profile.search` or `gpui-agent invoke profile.list` on `user_machine_shell` (not the box `shell`). Put `GPUI_AGENT_ADDR` on that same command. Do not run `find`, `mdfind`, or `which`. Do not run `gpui-agent hello` before a catalog invoke. This channel already includes `~/.cargo/bin` on PATH. Live Mac app-group DB needs no `BIR_DATABASE_PATH`. Demo/box DBs use an explicit path. If `user_machine_shell` is not offered, say you cannot reach his machine.

## Arguments

`gpui-agent invoke NAME` takes repeated `--arg key=value`. A value that parses as JSON is that JSON: `year=2026` is a number, `confirm=true` is a boolean. Do not pass a JSON object as a positional argument.

- `profile.search` and `profile.set`: `--arg q=Juan Dela Cruz` or `--arg tin=00000000000000`. The field is `q`, not `query`.
- `profile.forms_set.get`: `--arg year=2026` is required. The selected profile is enough. Add `--arg tin=` only when none is selected. If he did not name a year, use the current calendar year. Answer from `{codes, entries, empty}`.
- `profile.forms_set` writes. It needs `--arg confirm=true` and only after he confirms.

## Invokes

These names are the whole catalog. Use them with `gpui-agent invoke`. Do not search the disk. This text is already the instruction.

`profile.search`, `profile.list`, `profile.set`, `profile.forms_set.get`, `dues.list`, `nav.go`, `form.fill`, `form.fields`, `form.save_draft`, `form.pdf`, `filing.validate`. Never `profile.ensure`. Never `form.set-value`. `set-value` is not an invoke. `form.fill` writes a return, not a profile.

## New profile

`profile.create` opens a blank editor and ignores `name` and `tin`. `profile.save` reads that editor and takes no arguments. Do not call `profile.save` until the fields are filled.

When he asks to create a profile, call `request_user_form` with `collect` true before any save. Title it "New tax profile". Fields, all required: `name`, `tin`, `rdo`, `line_of_business`, `address`, `zip`, `phone`, `email`. Put what he already said on `value` (name, and the TIN as digits). Leave `value` off anything he did not say. Wait for the card.

After he submits, write each shared field with one positional command, not an invoke:

`gpui-agent set-value profile-name 'Juana Jane'`

`gpui-agent set-value profile-tin 00000000000001`

Targets: `profile-name`, `profile-tin`, `profile-rdo`, `profile-lob`, `profile-address`, `profile-zip`, `profile-phone`, `profile-email`. Then tell him the editor is filled and ask before `gpui-agent invoke profile.save`. A profile exists only after that save returns ok.

If a search comes back empty, that is the answer. Do not search again with the same words. Never skip lock screen / PIN / TOTP.

## Reply style

Call tools before narrating. Answer with the facts first (codes, deadlines). Do not dump port or invoke footnotes unless Uriah needs them to act. Do not repeat the same plan sentence.
