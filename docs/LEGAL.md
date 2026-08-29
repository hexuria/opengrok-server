# The line this project does not cross

Read this before writing code that touches the client contract. It is short on purpose.

---

## The situation

`/Volumes/goldcoders/OSS/grok-bot` is an **evidence-based reconstruction of a shipped binary** — the
Grok Bot desktop app, originally distributed by Anysphere (bundle id `com.anysphere.sand`, DMG from
`downloads.cursor.com`). Its own files say so plainly:

- **`grok-bot/NOTICE.md`** — "an unofficial reconstruction derived from a publicly distributed
  binary application… No upstream source-code license is asserted or granted here… Anyone
  publishing or distributing this repository should independently review copyright, trademark,
  third-party dependency, and service-terms obligations."
- **`grok-bot/PROVENANCE.md`** — the readable `frontend/` tree is a *partial* reconstruction; the
  shipped renderer was optimised production bundles, not authored source. Reconstructed builds get a
  different bundle id and only ad-hoc signing.
- **`grok-bot/docs/grok-0.27-disparity-proto.md`** — header reads: **"inventory only. Do not
  implement from this file."**

The tree also contains **157** generated protobuf modules (`*_pb.ts` / `*_connect.ts`) under
`source/packages/proto/generated/{agent,aiserver,anyrun,internapi}/v1/`, plus redacted wrappers over
the same messages — Anysphere's own message definitions, recovered from the binary.

---

## What OpenGrok does

**We implement the client-facing contract for interoperability.** The desktop app emits a JSON
command surface (`SAND_GATEWAY_COMMANDS`) and renders a documented transcript format. OpenGrok
answers those calls. The brain behind them — the agent loop, the tools, the routing, the storage —
is entirely ours and shares no lineage with the vendor's server, which does not exist in any tree
here and therefore cannot be and is not copied.

## What OpenGrok does not do

1. **No vendored generated protobuf stubs.** `source/packages/proto/generated/**` is never copied
   into this repository, nor `@connectrpc`/`@bufbuild` runtime dependencies added to serve them.
   Where a message shape is genuinely needed, it is **transcribed** into `crates/og-wire` in Rust,
   with a provenance comment naming the file it was read from.
2. **No reimplementation "from the proto" as a goal in itself.** The target is the *client's
   behaviour*, not the vendor's backend. If a command is not needed for the client to work, we do
   not implement it to be faithful to something we cannot see.
3. **No third-party trademarks in product surfaces.** No "Cursor", "Anysphere", "Grok" or "xAI"
   marks in UI copy, product naming, or public materials.

   > **An honest tension, not an oversight.** This project's working name is *OpenGrok* — chosen by
   > the operator, inherited from the earlier OpenGrok project — and it contains "Grok". As a local
   > directory and an internal codename that is consistent with everything above, because neither is
   > a product surface. **A public-facing product name is a decision for the rights review, not for
   > a commit**, and this rule is the reason it cannot be settled by drift. If OpenGrok is ever
   > published under that name, that is a decision somebody made deliberately, with advice.
4. **No redistribution before review.** `NOTICE.md` requires an independent rights review before
   publishing or distributing. Therefore:

> **OpenGrok stays private until a rights review clears it.**
> Nothing in `docs/PLAN.md` depends on publishing. If publication becomes a goal, the review is a
> prerequisite task, not a formality — and the two candidates for review are the client
> reconstruction itself and this contract implementation.

---

## Why this is written down rather than assumed

Because the pressure to cross it is real and arrives phrased helpfully — *"just match the proto so
it's 100% compatible"*, *"the stubs are right there"*. The distinction that keeps this project on
solid ground is narrow but genuine:

| Fine | Not fine |
|---|---|
| Answering the calls a client already makes | Copying a vendor's server implementation |
| Transcribing a shape into our own types, with provenance | Vendoring their generated stubs |
| Reading an inventory to learn what the client expects | Treating that inventory as an implementation source |
| A private research build | Publishing without a rights review |

If a future task cannot be done without crossing the right-hand column, **stop and put the decision
to the operator.** Do not decide it inside a commit.
