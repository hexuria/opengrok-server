# Per-coworker model pins — verification

Slice 18, verified 1 Sep 2026 against the **real** open-ai-gateway on `:29080` (not a stand-in:
the stand-in is what `tests/against_model_pins.rs` and `scripts/slice22-model-pins-smoke.sh` use
to pin our own rules). Server booted with `OG_GATEWAY_URL=http://127.0.0.1:29080`,
`OG_MODEL=openai/gpt-5.5`.

## The catalogue is the gateway's, and the key stays here

`GET /models`, through our server, signed in as an ordinary person:

```json
{"models":["oag/auto","oag/cheap","oag/balanced","oag/frontier","openai/gpt-5.5","openai/gpt-5.5@api"],"note":null}
```

`grep oag_live_` over that reply: nothing. The browser learns ids; the credential that fetched
them never leaves the process. Without a token the same route answers `401`.

## An advertised id is not a servable one — which is why Test exists

This is the finding the slice turned on. `oag/auto` **is** in the catalogue above, and is what the
investigation doc recommended as the new default. Probing it:

```
POST /models/probe {"model":"oag/auto"}
  → {"ok":false,"detail":"no credential available for provider anthropic on this route"}

POST /models/probe {"model":"openai/gpt-5.5"}
  → {"ok":true,"served":"gpt-5.5"}
```

The route's tier ladder is Anthropic rungs and this deployment has only an `openai` credential, so
`oag/auto` classifies into a rung it cannot serve. Had we taken the doc's recommendation, **every
new hire would have been pinned to a route that cannot answer.** Nothing in the catalogue would
have shown it; only asking did. `OG_MODEL` is therefore left as the deployment's own choice, and
the dialect plus this caveat are recorded in `.env.example` and `docs/setup/environment.md`.

## A pin is the coworker's, and it moves

```
POST /coworkers {"name":"Pinned","model":"openai/gpt-5.5"}
  → hired cw_01a05b45-1c1f-7bd3-b1a6-a83230564c19 on openai/gpt-5.5

a real turn on that coworker → "pinned and answering"

PATCH /coworkers/{id} {"model":"oag/auto"}
  → {"id":"cw_01a05b45-…","model":"oag/auto"}

the NEXT turn on that same coworker →
  the model gateway refused: 503 {"type":"error","error":{"type":"no_credential",
  "message":"no credential available for provider anthropic on this route"}}
```

Both halves of the goal in one sequence: the repin took effect on the very next turn, and a pin
the gateway will not serve **fails loudly, in the gateway's own words**, rather than quietly
answering on the deployment's model — which is the failure this project's `slice5` smoke was
written to catch in the first place.

## What is pinned by tests rather than by this page

- `crates/opengrok-core/src/coworker.rs` — repin changes only the model; a retired coworker
  refuses; a blank pin is refused on **hire and repin** (it was storable until this slice, and
  would have been asked of the gateway verbatim); a pin is stored trimmed.
- `crates/opengrok-server/tests/against_model_pins.rs` — the real router against a stand-in
  gateway: hire honours a pin, PATCH changes it without renaming, blank repin is 400 and leaves
  the old pin standing, another account's coworker is 404 (never 403), and the catalogue reply
  contains no `oag_live_`.
- `scripts/slice22-model-pins-smoke.sh` (in the gate) — the mock door names the model it was asked
  for, so: the pin reaches the door, the **next** turn after a repin reaches it on the new route,
  and a run with no coworker still answers on the deployment's model (slice5's invariant, held).

## Two limits worth knowing, found in review

**A resumed run thinks with the pin its turn started on.** Stored on `RunEvent::Started`;
`pin_for_resume` uses that, and only falls back to the coworker's current pin for logs written
before the field existed. Gateway `resume_gateway_run` and AG-UI `continue_run` both honour it.
(`ROADMAP 18.pin`.)

**Seam B has no repin path.** `UpdateGrokBotAgent` handles rename and profile only. Repinning is
reachable from REST `PATCH /coworkers/{id}` and the gateway's `updateAgent`; a seam-B client that
sent a model today would be silently ignored. Deliberate scope, recorded so it is not mistaken for
an oversight.

## Deliberately not changed

A run that NAMES a coworker it cannot load still answers on the deployment model. That behaviour
carries a written rationale in `agui/routes.rs` ("the model is how the turn is answered, not
whether it is allowed, and that question was just asked"), and overturning a recorded decision by
drift is what the invariants forbid. Raised in review instead.
