# What we built, and why it wasn't enough

The honest version, for anyone who asks why a working product is being rebuilt in another language.

**The short version.** We built OpenSesame, and it works — AI coworkers in rooms alongside people,
each pinned to its own model, answering through our own gateway on a real subscription. What it
cannot do is work *while you are not watching it*. The intelligence lives on the client's side of
the line, so the laptop has to stay open, the tab has to stay on that screen, and a person who wants
a job done has to sit and supervise it. That is the opposite of the thing worth building — and the
opposite of the client we already have, which was remote-first from the start.

You cannot fix that with a patch, because it isn't a bug. It is where the work lives.

---

## 1. What OpenSesame actually is

Not a prototype. Nineteen commits of our own on a vendored substrate, 29 migrations, ~300 source
files across the app and server, CI green on every job including the container build, and a test
suite in the low 2,000s. What works, today, in a browser:

| | |
|---|---|
| **Rooms** | several AI coworkers and several people as members of one channel, with add/remove, a last-member guard, and audit rows |
| **Per-coworker model pins** | each coworker routed to its own model (`xai/grok-4.6@sub`) through open-ai-gateway — a filterable picker, free-text route entry, and a Test probe that actually calls the model |
| **Correct thread binding** | one thread per (person, channel, coworker), because the hosted platform binds a thread to one agent permanently — this is what makes "who answers" a real control instead of a label |
| **A woven room transcript** | every coworker's exchanges interleaved into one timeline, ordered by an append-only index the room keeps itself (stored messages carry no timestamps) |
| **Attribution** | coworker replies captioned with avatar and name; a person's messages captioned with name and monogram; captions switch speaker mid-transcript |
| **@mention routing** | mention a coworker and the turn goes to them, with the draft carried across the rebind |
| **`@Bots` / `@All`** | group mentions that ask the whole room, with the fan-out's duplicate copies collapsed to one bubble |
| **People-picker** | add people to a room, with a findability policy enforced server-side (admins find anyone; everyone else only people they already share a room with) |
| **Mentions with notification** | @mention a person and they get a badge and the sentence that named them *(built, on a branch)* |
| **Animated avatars** | two original themes, three animation states, a builder with live preview and per-part colours; the recipe derives from a seed and manual picks persist |
| **A gateway status panel** | `/admin/brain` reading the gateway's live diagnostics envelope — which providers are serving, which are rate-limited and until when |
| **Inherited from the substrate** | skills, routines, per-agent computers, CEL policies, audit, handoffs between coworkers |

Along the way it found and got fixed three real bugs in the gateway — reasoning-stream openers,
duplicate response item ids, an honest model catalogue — each verified against live traffic.

**None of that is wasted.** Section 6 says what carries.

---

## 2. What it cannot do

Ask it to do a job and walk away. That's it. That's the whole limitation, and it is fatal to the
product this is supposed to be.

Concretely, when you close the tab:

- **A broadcast dies.** `@All` walks the room from React state in the page. Refresh, and four of five
  coworkers never hear the question — and nothing anywhere knows they were owed an answer. (The
  mechanism, in detail: [`research/lessons-opensesame.md`](research/lessons-opensesame.md) §4.)
- **The record of what happened stops being written.** This is the sharp one. The room's ordering
  index is written *by the browser* — the client reports each message id to the server after the
  fact. A reply whose run finishes after the page is gone has no entry in the timeline. The
  transcript is not merely un-watched; it is un-recorded.
- **Supervision stops.** The stall guard and turn watchdog exist because a stalled model stream
  leaves a channel busy and a composer locked. Both only run while somebody is watching — the
  failure they exist to catch is invisible in exactly the case you'd want them.
- **Nothing advances.** Not "the UI stops updating" — the *work* stops. There is no server row that
  says a job is in progress, so there is nothing for anything to pick up.

And even while you *are* watching, the shape leaks into the product: a five-coworker broadcast waits
for five round trips in series, not because anyone chose sequence, but because the page can only
hold one live agent binding at a time. The client's shape became the product's behaviour.

Underneath all of it sits a dependency that makes the position structural rather than accidental:
the product **cannot boot** without a proprietary hosted service supplying threads, memory and run
locks. The durable half of "who said what, and is a run in flight" was never ours.

> The test it fails: **start a job, shut the laptop, open your phone, watch it finish.**

---

## 3. What "remote first" means, and why the client already has it

The Grok Bot desktop app — the client we now build behind — was never the place the work happened.
Its agent works on a **box**: a computer that is not your laptop. The app is a viewport onto work
running somewhere else. Close the lid and the box keeps going; open another device and you are
looking at the same coworker, mid-task.

That is visible in the client's own seams — `source/host/box/` models a box as
`{ host, port, authToken }` and supports both a loopback box and a remote one. The architecture
assumed from day one that the intelligence is elsewhere.

It is also the shape of every product in this category worth copying. Anthropic's Claude Cowork
describes itself the same way: hand it a job, it runs on their machines, shut your laptop, come back
to finished work. ([The Cowork Picture
Book](https://claude.ai/code/artifact/9b07e7f0-d235-4534-a7fe-3b19fb73f0e2) — see
[`DIAGRAMS.md`](DIAGRAMS.md) §5.)

We ended up with the inverse: a beautiful client tethered to a browser tab, talking to a server that
is mostly a proxy, in front of somebody else's state service.

---

## 4. Why we didn't just patch it

The obvious question. We could add a fan-out queue table to OpenSesame and a worker to drain it —
maybe a week. It would fix the broadcast. It would not fix the product, because the tether is not in
one layer:

| Layer | Where it is today | What "fixing it" means |
|---|---|---|
| **Orchestration** — the queue, the walk, the retry | React state in the page | a durable ledger and a worker |
| **The turn** — who starts a run, who hears it finish | the browser opens it and subscribes to it | the server starts it, streams to whoever is watching, and finishes it alone |
| **The record** — the timeline index | reported by the client after the fact | written by the thing that ran the turn, in the same transaction |
| **Durable state** — threads, memory, run locks | a proprietary hosted service, no degraded mode | ours, in Postgres |
| **The computer** — where tools actually run | a local process beside the server | a remote box the coworker owns |
| **Policy** — what a caller may make a coworker do | partly client-shaped | enforced server-side on every action |

Each row is a rewrite of a different layer. Do all six and you have not patched OpenSesame — you
have written the server this project is. Doing it deliberately, in one language, with the seams
chosen up front, is cheaper and far more honest than six retrofits that each leave a scar.

**And the capability is already proven.** OpenSesame's own routines runner takes a turn on a
coworker's thread with no browser attached anywhere — scheduled work already runs headless. The
server *can* do this. It simply isn't where the interaction lives. The pivot is not a leap of faith;
it is moving the interaction to where the capability already sits.

One more reason, worth saying plainly: a browser tab can never be the right home for this even if
every bug were fixed. Phones sleep. Laptops close. Browsers evict background tabs. A job that takes
twenty minutes cannot depend on a person not switching apps.

---

## 5. So the shape inverts

| | OpenSesame (now) | OpenGrok (next) |
|---|---|---|
| Where the harness runs | driven from the page | on the server |
| Where the queue lives | React state | a table |
| Who writes the record | the client, after the fact | the runner, in the same transaction |
| Threads / memory / locks | a hosted service we cannot boot without | ours |
| The coworker's computer | a local process | a remote box the coworker owns |
| A broadcast | sequential, dies on refresh | parallel, survives everything |
| Close the laptop | the work stops | the work continues |
| Clients | one web app | desktop, web, CLI — all windows |

The picture version of this table is [The Coworkers Move
Out](https://claude.ai/code/artifact/1c526721-d19b-406c-b4f9-feef43a507dd).

---

## 6. What survives

Almost all the thinking, and possibly the app itself.

- **The designs**, each learned the hard way and documented in
  [`research/lessons-opensesame.md`](research/lessons-opensesame.md): one thread per
  (person, channel, coworker); the woven timeline with its own ordering index; the fan-out collapse;
  per-coworker model pins and the dialect rule; speaker and author attribution; the five planes
  behind "a bot is not an API key".
- **The gateway.** Already Rust, already ours, already serving — it becomes the model door in
  OpenGrok's own wall rather than a service beside it.
- **The bug hunt.** Three upstream gateway fixes, verified against real traffic.
- **The app itself, as a second client.** Its interaction model is good. Once the server owns the
  work, an OpenSesame-shaped web client is a window like any other — and a useful proof that the
  contract isn't secretly desktop-only.

The rewrite is of the *server*, and of where the work lives. Not of what we learned.
