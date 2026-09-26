# tmp2 CDP proofs — `@plugin` pills like MCP

Packaged from `opengrok-tmp2-v2` (`feat/tmp2-on-v2`). Installed into `/Applications/Open Grok.app`. Driven over CDP 9223 with `scripts/verify-tmp2.mjs`. Driver **2026-09-13T08:11:24Z**, exit **0**.

**Composer law:** `@` activates bots / MCP servers / TMP plugins as pills. `#` is args for **activated** TMP plugins only. `/` is skills. The `person` pill is a `tmpPlugin` atom (`data-type=tmp-plugin`), not payload. `#user` chips stay `tmpToken`.

Session: **uriah@goldcoders.dev** / **Quill**. Candidates **Uriah Galang** and **Ada Lovelace**.

Live stream proof: `is_running=true` in the rust serve log (OAG `routed` log was unlinked this run). Preflight `GET /tmp/complete` catalog `user,years,role,pin`.

Inspected pixels (this run, 16:11 local):

| # | Flow | Screenshot | Server proof |
|---|---|---|---|
| 01 | Idle composer, no People badge | `01-tmp-mode-on.jpg` | `uriah@goldcoders.dev` |
| 12 | Bare `#` with no pill | `12-hash-without-pill.jpg` | “Nothing to reference yet”. No `#user`. complete door 0 |
| 10 | `@per` → `person` pill with prefix glyph | `10-at-per-enter-pill.jpg` | Sidebar draft `person`. No `@per` bubble. routed unchanged |
| 11 | Backspace deletes the pill | `11-backspace-deletes-pill.jpg` | `#` no longer lists `#user` |
| 14 | Pill-only Enter, required `#user` missing | `14-required-blocks-send.jpg` | Red **Person is required**. Inference stayed 12 |
| 02/13 | Pill then `#` lists tokens above composer | `02-at-opens-user-picker.jpg` / `13-hash-after-pill.jpg` | `#user #years #role #pin`. Draft `person #` |
| 03 | `#user` → Uriah chip | `03-chip-bound-user.jpg` | Chip `Uriah Galang`, no `acct_` |
| 04 | Unique send | `04-send-unique-grounded.jpg` | Bubble **Uriah Galang say hi**, no `@person`. Inference **12→13** |
| 05 | Empty `#user` pick-list, no send | `05-ambiguous-pick-no-send.jpg` | Ada + Uriah listed |
| 06 | Pick Ada, send | `06-pick-then-send.jpg` | Bubble **Ada Lovelace thanks**. Inference **14→15** |
| 08 | `/` is skills | `08-tmp-mode-off-skills-on-slash.jpg` | Not Chat Settings |
| 09 | `#years` number UI (after pill) | `09-bang-enables-tmp.jpg` | Placeholder Years |
| 07 | Implicit `email Uriah the invoice` **with** pill | `07-implicit-uriah.jpg` | Original text, no `@person`, no `acct_`. Inference **16→17** |
| 17 | Same text **without** pill | `17-implicit-without-pill.jpg` | Normal chat. No `acct_` in the bubble |

Kernel/server: `against_tmp2_preflight` **10/10** including `tmp_mode_missing_required_user_does_not_open_door` and `implicit_uriah_without_tmp_mode_does_not_ground`. Desktop `tmp-mode.test.mjs` **5/5**.
