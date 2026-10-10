# Moved: deterministic resonance library → Instinct

This spec now lives in its own repo, **Instinct** (formerly PUA, the Predictable Universal Advisor;
renamed in hexuria/instinct#27):

**https://github.com/hexuria/instinct/blob/main/docs/spec.md**

- Deterministic: the same input gives the same scores, with no sampling. Instinct abstains when unsure.
- Instinct is tier 0, below Jev. Jev remains the heavier tier above it.
- The `opengrok-reso-*` crates proposed in the earlier draft of this file (commit `d441c92`) became
  `pua-*` and are now `instinct-*` crates in `hexuria/instinct`. Instinct is domain-agnostic: the
  domain packs (including the autosteer pack) left the repo in hexuria/instinct#23, and the steer
  logic rehomes to opengrok-server ingress (see [`delivery-advisor-spec.md`](./delivery-advisor-spec.md)).
- opengrok-server would consume Instinct as an optional git dependency pinned by tag or rev, like the
  other consumers. No Instinct crate depends on any opengrok crate.

Review the engine spec in `hexuria/instinct`, not here.
