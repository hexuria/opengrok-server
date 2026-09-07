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

## 6. The Computer Moves In — *the four-layer stack, and bringing the box home*

**https://claude.ai/code/artifact/aa457e3c-df09-49ae-8ac0-43e48b10ac71**
Source: [`artifacts/computer-moves-in.html`](artifacts/computer-moves-in.html)

Why the three-layer drawing (client / server / computer) is missing a layer — model calls leave
through the gateway, which is L4 and has never heard of a box — and what each layer is made of,
part by part. Names the three things called "gateway" (inference gateway, tool router, host
gateway) and why "the gateway is up" is not a sentence. Rented desk (box.ascii.dev) vs owned desk
(`hexuria/box`): the difference is one arrow. Shows that the `Computer` trait is already the right
seam — two implementations behind it today, the self-hosted box is a third — and that the real gap
the vendor was hiding is Computer Use, not the box. Ends with a build order in which every step is
provable with curl.

---

## 7. The Building, Floor by Floor — *the from-scratch architecture, as a modular monolith*

**https://claude.ai/code/artifact/35c75af9-9178-458d-b3fc-8451b66b2a20**
Source: [`artifacts/building-floor-by-floor.html`](artifacts/building-floor-by-floor.html)

Language-agnostic. Four things that run — window, building, desks, the person's own machine —
and the building as one deployable with ten floors on a spine: Doors, Identity, Conversation,
Agents, Tools, Policy, Computers, Connectors & Secrets, Autonomy, Inference. One illustration per
floor, drawn the same way on purpose (rooms across the middle, who may knock on the left, what it
announces on the right). States the modular-monolith rules and the extraction test that defines
them. Records the operator's decisions of 7 Sep 2026: the gateway becomes Floor 10; the person's
Mac stays a separate local-exec path, not a computer kind; desk host is chosen per org. Closes with
the correction table — what is misplaced today and where it goes — and the queue of hard-topic
books. **This is the drawing the hard-topic books are drawn against.**

---

## 8. The Model Only Suggests — *how a tool call gets executed*

**https://claude.ai/code/artifact/da6d8fff-ccb5-43b6-bfd7-8b0755158176**
Source: [`artifacts/model-only-suggests.html`](artifacts/model-only-suggests.html)

Hard topic 1, drawn against №7. One tool call through eight stations across five floors:
the model suggests, the registry checks, the router addresses, policy signs (grants ∩ machine gate
∩ auto-review — intersection, never union), the executor overwrites identity and wakes the desk,
the desk hands back data, the record is written, the harness continues. The three exits side by
side (desk / the person's machine / connector) and what differs at each. Suspension as a state,
not a failure. Computer Use as ordinary tool calls in a loop, so auto-review sees every click.
Ends with the six things the model never gets to do.

---

## Adding to this list

Publish with the Artifact tool, vendor the HTML into `docs/artifacts/`, and add an entry here with
its URL, its source path, and one paragraph on what it explains. A link with no vendored source
rots the moment the host changes.
