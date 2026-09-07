# Diagrams and picture-explainers

Visual explanations of why OpenGrok exists and what it is. Each is a published Claude artifact
(private to the owner's account unless shared from the page's share menu). **Sources for the three
authored here are vendored in `docs/artifacts/`**, so they survive independently of the hosting.

Read them in this order for the fastest possible orientation.

**№6–№18 are one series**, drawn 7 Sep 2026 against the from-scratch architecture. Read **№7 first**
— it is the building every later book is drawn against — then №6 and №8, then any hard topic in any
order. Together they supersede the seven `Grok-bot-architecture` PDF guides: Guide 01 → №6 + №7,
Guide 02 → №11, Guide 03 → №10, Guide 04 → №15, Guide 05 → №7, Guide 06 → №12, Guide 07 → №16 —
with №8, №9, №13, №14, №17 and №18 covering layers the PDFs had no chapter for. House style for
new books in the series: dark-first, Fraunces + Public Sans, gold accent, the shared SVG class
vocabulary — copy the `<style>` block from any vendored source.

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

## 9. The One Card — *how consent works*

**https://claude.ai/code/artifact/0f4c7429-6371-44b1-8bec-9dcf97406fdf**
Source: [`artifacts/the-one-card.html`](artifacts/the-one-card.html)

Four controls answering four different questions — the machine's own switch, the remote-control
gate, the card, auto-review — drawn as a funnel where nothing to the right can widen what the left
already closed. The card's four answers and which two write standing rules. Why the card never
expires, drawn as the failure of the alternative: a card asked on the machine must time out, and a
timeout is a silent no. The two-tier judge with per-field inheritance. "At most one card per tool
call" as an invariant, not an optimisation. Spend as a brake rather than a question, and why points
rather than currency.

---

## 10. A Desk Is Made — *how a computer is born, sleeps and dies*

**https://claude.ai/code/artifact/432479b9-f5e3-480b-9a7a-0deb56a8a894**
Source: [`artifacts/a-desk-is-made.html`](artifacts/a-desk-is-made.html)

Six states and no more; the difference between "stopped" and "gone" is the volumes, and it is the
only difference that matters. What create actually does, in the order it must happen: mint the
token, pick the org's desk host, prepare the volumes, start the image, publish three ports
privately, record the row. Why *running* is not *ready* and what breaks when you trust the wrong
one. A table of what survives a stop versus a destroy — including the one that surprises people,
that installed packages do not. Where desks run (per-org choice) and who they belong to (scope, not
coworker). Ends with a seven-step stand-up order.

---

## 11. The Run That Won't Die — *durability, suspension and recovery*

**https://claude.ai/code/artifact/fef70901-1756-4072-8f46-10f91358314c**
Source: [`artifacts/the-run-that-wont-die.html`](artifacts/the-run-that-wont-die.html)

Opens by disambiguating "harness" — the runtime loop versus the field on a coworker — which is
Guide 02's subject settled in one picture. A run as a journaled graph rather than a function call.
The load-bearing rule drawn as two timelines: journal *before* the call, because the alternative
loses the fact that a call happened and repeats it. Three reasons a run pauses and why only the
human one is a saved state. Recovery as a sweep plus an atomic claim rather than a crash handler.
Exactly-once answers when a phone and a laptop both answer the same card.

---

## 12. Five Keys, Five Locks — *where every secret lives*

**https://claude.ai/code/artifact/1be42f46-a3e4-4081-b209-1a553a46aa36**
Source: [`artifacts/five-keys-five-locks.html`](artifacts/five-keys-five-locks.html)

Guide 06 redrawn against this building. Five kinds of secret placed in three zones, plus the sixth
that has no resting place at all, with a blast-radius column for each. The login handoff drawn
end to end — move the human, not the secret — and what never happened as a result. The one-off
secret's five steps and where each one deliberately does nothing. Why connector tokens are lent
rather than copied, so "who still has a copy?" is never a question. Closes with the five lines
nothing crosses, and why provider credentials are the ones to guard hardest.

---

## 13. Which Model, Whose Key — *routing, credentials and metering*

**https://claude.ai/code/artifact/536abaac-917b-4f2e-bc97-b3d4b8be6dd9**
Source: [`artifacts/which-model-whose-key.html`](artifacts/which-model-whose-key.html)

The Inference floor. Six things that happen to every call, with the two that save money and the two
that keep it alive marked. Why a coworker names a route rather than a model, and why that is what
makes coworkers safe to share. Failover versus escalation as one pure decision, with the cost of
getting it backwards in both directions. The counterfactual column that turns "we save money" from
an argument into a sum. The two doors — in-process for coworkers, the model door for outside tools
— sharing one pool and one meter, which is the piece that disappears when the gateway moves inside.

---

## 14. Nobody Typed This — *runs that start on their own*

**https://claude.ai/code/artifact/3ae22ec9-193b-46bd-bb26-b1ced1b7c763**
Source: [`artifacts/nobody-typed-this.html`](artifacts/nobody-typed-this.html)

The clock, the ear and the megaphone — and the fact that all three end by *asking* for a run
exactly as a person would, which is why autonomy adds no new policy surface. The atomic claim that
stops three replicas firing one schedule three times, drawn as a race with two cheap losers. The
loop guard as a before/after: a monitor that reacts to its own runs, versus one stamped with its
cause. Dispatch written to a ledger first so delivery becomes retryable and "did everyone get it?"
is answerable.

---

## 15. The Window and the Machine — *the client's two jobs*

**https://claude.ai/code/artifact/78e65783-e7c9-4a9f-aa7b-fcfcd6ea12d8**
Source: [`artifacts/the-window-and-the-machine.html`](artifacts/the-window-and-the-machine.html)

Guide 04 redrawn. Job one: a window renders and owns nothing — drawn as the close-the-lid test
against the alternative, which is the bug that created this project. A renders/owns table for every
surface. Job two: the only path to the person's real computer, with the arrow direction as the
security property (nothing outside can dial in; requests travel down a line the window opened).
The three gates, one of which the building deliberately cannot open, with the trade-off stated
out loud. What "connected" decides, and the much longer list of what it does not.

---

## 16. Eyes and Hands — *Computer Use*

**https://claude.ai/code/artifact/e91af54a-efa1-46f3-b8f9-e3d81d505382**
Source: [`artifacts/eyes-and-hands.html`](artifacts/eyes-and-hands.html)

Guide 07 redrawn. Look, decide, act, look again — every arrow a full tool call, which is what buys
auto-review on every click. The screen with no monitor attached, and why the human viewer and the
coworker see literally the same pixels. The three things people mean by "the bot used my computer",
only one of which is Computer Use and none of which is your screen. The coordinate rule drawn as
the bug it prevents. The password rule: stop and hand off. Ends with a judgement table — reach for
this last, with the cost of a screenshot versus a connector call stated plainly.

---

## 17. Many Voices, One Room — *groups and shared rooms*

**https://claude.ai/code/artifact/fc9688dc-c7b5-4a41-8adc-196b623f374a**
Source: [`artifacts/many-voices-one-room.html`](artifacts/many-voices-one-room.html)

The two features called "rooms" separated on the first page: a group (one person's coworkers, one
tenancy) versus a shared room (several accounts, crossing the tenancy wall). A group as a coworker
whose members are coworkers, and the list of things that work free on day one because of it. The
round — resolve, order, rotate the first speaker, each speaks once or passes — and why caps are not
optional. What a member's prompt is built from. Then the six tenancy questions shared rooms force,
each with its trap and its safe default, and the build order that follows: groups first, rooms
behind a switch that fails closed.

---

## 18. Moving Out — *how a floor leaves the monolith*

**https://claude.ai/code/artifact/d6fbdb5f-971b-4216-91ec-ca4139a067ef**
Source: [`artifacts/moving-out.html`](artifacts/moving-out.html)

The extraction test made concrete: three substitutions and nothing else may change. Four symptoms
of a fake seam, three of which one habit prevents — never write a query that names two floors'
tables. Inference extracted in seven steps, with step 3 (boot it alone, still call it in-process)
as the honest test of undeclared dependencies. The honest section: extract for hardware, scale,
blast radius or ownership — never for tidiness — and a scoreboard of what each floor would cost
today, read as a design review rather than a plan.

---

---

## Client-facing: The OpenGrok Platform — *target-state architecture*

**https://claude.ai/code/artifact/d129eef2-f3c0-425d-b88e-1ce14cbda4ae**
Source: [`artifacts/opengrok-platform.html`](artifacts/opengrok-platform.html)

**Not part of the picture-book series** — different audience, different house style (light-first,
Bricolage Grotesque + Source Serif 4, teal accent), and industry vocabulary rather than the
building metaphor. Written to hand to a prospective customer of the platform. Four parts: what it
is and what it guarantees (eight architectural properties stated as claims); the system map and the
ten subsystems in standard terms; where the customer sits — data residency by class, the
organisational isolation boundary, the three execution destinations, and the administrator control
surface; then operations, failure behaviour, and an explicit in-scope / not-in-scope section.

It describes the **target state**, not what is built today, and says so in its closing note.
`ROADMAP.md` remains the truth about what exists — check any capability claim against it before
sending this to anyone.

---

## Adding to this list

Publish with the Artifact tool, vendor the HTML into `docs/artifacts/`, and add an entry here with
its URL, its source path, and one paragraph on what it explains. A link with no vendored source
rots the moment the host changes.
