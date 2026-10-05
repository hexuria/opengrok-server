# Moved: deterministic resonance library → PUA

This spec now lives in its own repo as **PUA, the Predictable Universal Advisor**:

**https://github.com/hexuria/pua/blob/main/docs/spec.md**

- "Predictable" means deterministic: the same input gives the same scores, with no sampling.
- PUA is tier 0, below Jev. Jev remains the heavier tier above it.
- The `opengrok-reso-*` crates proposed in the earlier draft of this file (commit `d441c92`) are now
  `pua-*` crates in `hexuria/pua`. The autosteer pack ([`delivery-advisor-spec.md`](./delivery-advisor-spec.md))
  is `pua-pack-autosteer`.
- opengrok-server would consume PUA as an optional git dependency pinned by tag or rev, like the other
  consumers. No PUA crate depends on any opengrok crate.

Review the spec in `hexuria/pua`, not here.
