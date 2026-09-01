# One identity, two doors — end-to-end verification

Slice 17, verified 1 Sep 2026 against the **real** open-ai-gateway (`:29080` inference, `:29081`
admin) with the new admin endpoints, and a real Claude Code run. Not a stand-in: the stand-in is
what `scripts/slice21-org-keys-smoke.sh` and `tests/against_the_gateway_keys.rs` use to pin OUR
rules; this page is the live proof that the two services actually meet.

## The model, confirmed on real rows

| Ours | Gateway | Verified |
|---|---|---|
| org `org_01a05a70-…` | principal `org-org_01a05a70-…@gateway.local` | the address is derived from the org id, not stored |
| member `dev@oneid…test` | api_key `oag_live_57f3705…` on that principal | minted from the console API, labelled with the member |
| org budget | `principal.monthly_budget_usd` = `20.000000` | set from the console, read back live |
| member cap | `api_key.quota_usd` = `5.00` | passed through the mint |

## What was actually driven

**1. The admin sets a budget and mints a member a key**, through the console's own API with the
admin's cookie session:

```
PUT  /admin/gateway/budget   {"monthlyBudgetUsd":"20.00"}   → 204
POST /admin/gateway/keys     {"memberId":"acct_01a05a70-…","quotaUsd":"5.00"}
  → 201 { label: "dev@oneid11609.test", keyPrefix: "oag_live_57f3705", key: "oag_live_…" }
```

The plaintext appears in that reply and nowhere else — we store only the prefix, the gateway only
a SHA-256.

**2. Claude Code used that member's key.** With nothing but the two environment variables and the
key an org admin had just handed out:

```sh
ANTHROPIC_BASE_URL=http://127.0.0.1:29080 ANTHROPIC_AUTH_TOKEN=<the member's key> claude -p …
→ one identity, both doors
```

**3. The spend rolled up to the org, natively.** The console's reading and the gateway's own
ledger agree, because they are the same number — we keep no copy:

```
console  GET /admin/gateway/usage
  {"monthlyBudgetUsd":"20.000000","monthToDateUsd":"0.097560","requests":1,"provisioned":true}

gateway  select p.email, count(*), sum(cost_usd) … where p.email = 'org-org_01a05a70-…@gateway.local'
  org-org_01a05a70-5514-7893-ba42-ab793dc76264@gateway.local | 1 | 0.097560
```

**4. Revoking from the console kills the key.**

```
DELETE /admin/gateway/keys/<id>            → 204
GET    :29080/v1/models  (that same key)   → 401
```

## Authority, proven separately

The rules that are ours are pinned where they can be run every time, not just here:

- `crates/opengrok-server/tests/against_the_gateway_keys.rs` — a member is refused 403 on every
  verb **and never reaches the gateway**; another org's key id is 404, not 403 (a 403 would confirm
  it exists); a listing never carries a secret; revoke reaches the gateway before the local row is
  mirrored; the request we send names the derived principal and the member's label.
- `scripts/slice21-org-keys-smoke.sh` (in the gate) — the same walk over HTTP with curl.
- In the gateway's repo: a minted key authenticates, carries its cap, **is never `admin`** (a
  minted key asking to mint another is refused 403), and stops authenticating once revoked.

## What this leans on

The gateway admin connection is `OG_GATEWAY_ADMIN_URL` + `OG_GATEWAY_ADMIN_TOKEN` — an **admin**
key (`oag admin key create --admin`), deliberately separate from the inference key a run spends.
Unset turns the console's Gateway access card off rather than failing the boot. This deployment's
gateway routes to the machine's local opencodex door (see `docs/verification/door1/README.md`);
production swaps that for real provider credentials with no change here.
