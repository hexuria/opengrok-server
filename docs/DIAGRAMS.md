# Diagrams and picture-explainers

Visual explanations of why OpenGrok exists and what it is. Each is a published Claude artifact
(private to the owner's account unless shared from the page's share menu). **Sources for the three
authored here are vendored in `docs/artifacts/`**, so they survive independently of the hosting.

Read them in this order for the fastest possible orientation.

---

## 1. The Coworkers Move Out — *the pivot, in seven pictures*

**https://claude.ai/code/artifact/1c526721-d19b-406c-b4f9-feef43a507dd**
Source: [`artifacts/coworkers-move-out.html`](artifacts/coworkers-move-out.html)

Why the harness, tools, computers and delivery belong on one server rather than inside a browser
tab. Walks through: where the brain lives today → close the tab, kill the work → one building with
harness/tools/computers/workflows and the gateway as its front door → clients become windows → each
coworker gets an office → configure from the client, the guard decides → `@All` becomes "tell the
building once", written to a ledger and fanned out in parallel.

**This is the founding document of OpenGrok.** If a reader has time for one thing, this is it.

---

## 2. Two Doors to the Server — *how the desktop client is reused*

**https://claude.ai/code/artifact/a6abb218-83a2-4225-93e7-a66f0532be88**
Source: [`artifacts/two-doors.html`](artifacts/two-doors.html)

The workflow for putting a backend behind the Grok Bot desktop app, drawn as the fork it actually
is. Door A — reimplement the vendor's private gRPC/proto server — is marked blocked, because the
repo's own `NOTICE.md` and disparity inventory forbid exactly that. Door B — keep the shell, speak
our own contract — is the chosen path.

See [`LEGAL.md`](LEGAL.md) for the position this diagram encodes.

---

## 3. The Desk and the Door — *why the agent endpoint and the gateway are different layers*

**https://claude.ai/code/artifact/c801de42-69c4-487b-88f6-3131c4f1b569**
Source: [`artifacts/desk-and-door.html`](artifacts/desk-and-door.html)

Explains an AG-UI agent endpoint (a *desk* — where a coworker sits and works) versus
open-ai-gateway (a *door* — where model calls leave the building), why they are not alternatives,
and why implementing the gateway inside the agent would be a mistake. The layering argument behind
`opengrok-harness` (the desk) and OAG (the door) being separate crates.

---

## 4. OpenSesame — build plan & specification *(prior product; shared by the operator)*

**https://claude.ai/code/artifact/71c9828e-4713-4b06-9625-7125404ddf4a**

The spec for the previous attempt. Valuable to OpenGrok for three things, all carried into
[`PLAN.md`](PLAN.md):

- **the five-question permission model** — may this principal talk to this agent / what may the
  agent ever do / what may this principal make it do / whose records may a call touch / which calls
  need a human yes — enforced in five different places, combined by intersection, never union;
- **the "overwrite, don't validate" rule** — a capability profile's `bind` clause replaces every
  identity argument from the session before a tool runs, so the model never gets a say in whose data
  it fetches;
- **the ADRs**, especially: own the agent loop (no framework offers suspension that survives the
  process), Postgres-only (enqueue is transactional with its cause), and Rust core / TypeScript
  edges (the tenancy fence becomes a compiler fence).

Its audit of four candidate repos is also the reason this project is a *build*, not an assembly job.

---

## 5. The Cowork Picture Book *(reference point; shared by the operator)*

**https://claude.ai/code/artifact/9b07e7f0-d235-4534-a7fe-3b19fb73f0e2**

Anthropic's Claude Cowork, explained in six pictures — hand it a job and walk away; the work runs on
their machines, not yours; it reaches your files/apps/web; it hands back real artifacts; it shows
every step and asks before anything big; put it on a schedule.

Included because it is the **product shape OpenGrok is aiming at**: give it a job, close the lid,
come back to finished work. Useful as a north star when a design decision could go either way.

---

## Adding to this list

Publish with the Artifact tool, vendor the HTML into `docs/artifacts/`, and add an entry here with
its URL, its source path, and one paragraph on what it explains. A link with no vendored source
rots the moment the host changes.
