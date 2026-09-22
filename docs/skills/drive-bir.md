---
name: drive-bir
description: Drive eBIRForms on Uriah's Mac via gpui-agent. Pick a BIR host, then forms, dues, or filing. Never profile.ensure. Save or queue only after he confirms.
---

Use when Uriah asks about eBIRforms / BIR profiles, forms sets, dues, drafts, or filing via NativeChat.

## Host selection (one owner)

1. Probe `GPUI_AGENT_ADDR` default `127.0.0.1:17421` with `gpui-agent hello` and the `GPUI_AGENT_TOKEN` already on that machine. Do not ask him to paste the token in chat.
2. Require `hello.app` to be `bir-desktop` (or the BIR app id). If `nativechat` or another app owns the port, pick a free port (often `17423`) and export matching `GPUI_AGENT_ADDR` for the rest of this turn. Remember the last address that answered `bir-desktop` and try it first next turn.
3. If painted BIR is open and agent-capable, drive that host. Do not start headless on the same live DB.
4. If painted BIR is closed, use `bir-headless serve --wait` against the intended DB path.
5. Never dual-write. Tray Quit or `gpui-agent shutdown` releases bind and lock. Hide / red-close does not. A second writer should see `LiveDatabaseInUse` or a bind failure — stop.

## Mac execution

Run `gpui-agent` and BIR binaries on the USER machine via `user_machine_shell` (not the box `shell`). Put `GPUI_AGENT_ADDR` on the same command. Live Mac app-group DB needs no `BIR_DATABASE_PATH`. Demo/box DBs use an explicit path. If `user_machine_shell` is not offered, say you cannot reach his machine.

## Invokes

These names are the whole catalog. Use them with `gpui-agent invoke`. Do not search the disk. This text is already the instruction.

`profile.search`, `profile.list`, `profile.set`, `profile.forms_set.get`, `dues.list`, `nav.go`, `form.*`, `filing.validate`. Never `profile.ensure`. `profile.create` opens the editor and writes nothing. Say a profile exists only after `profile.save` has returned ok. Save or queue only after Uriah confirms. If a search comes back empty, that is the answer. Do not search again with the same words. Never skip lock screen / PIN / TOTP.

## Reply style

Call tools before narrating. Answer with the facts first (codes, deadlines). Do not dump port or invoke footnotes unless Uriah needs them to act. Do not repeat the same plan sentence.
