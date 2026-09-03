import { useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import {
  claimDomain,
  clearAccountMode,
  clearBoxKey,
  disableUser,
  enableUser,
  gatewayUsage,
  getOrgMode,
  issueInvite,
  listDomains,
  listGatewayKeys,
  listInvites,
  listOrgComputers,
  listUsers,
  mintGatewayKey,
  revokeGatewayKey,
  setAccountMode,
  setBoxKey,
  setGatewayBudget,
  getPointsOverview,
  setPointsReference,
  setMemberPool,
  listTemplates,
  createTemplate,
  updateTemplate,
  deleteTemplate,
  type CoworkerTemplate,
  type TemplateInput,
  setGatewayKeyQuota,
  setOrgMode,
  testBoxConnection,
  verifyDomain,
  withdrawDomain,
  type GatewayKey,
  type OrgDomain,
  type SharingMode,
} from "../api/admin";
import { ApiError } from "../api/client";

function errorText(error: unknown, fallback: string): string {
  if (error instanceof ApiError) return error.message || fallback;
  if (error instanceof Error) return error.message || fallback;
  return fallback;
}
import type { Account } from "../api/account";
import { AuthedFrame } from "../components/authed-frame";

function UserRow({ user }: { user: Account }) {
  const queryClient = useQueryClient();
  const refresh = () => queryClient.invalidateQueries({ queryKey: ["admin", "users"] });
  const toggle = useMutation({
    mutationFn: () => (user.enabled ? disableUser(user.id) : enableUser(user.id)),
    onSuccess: refresh,
  });
  const setMode = useMutation({
    mutationFn: (mode: SharingMode | "") =>
      mode === "" ? clearAccountMode(user.id) : setAccountMode(user.id, mode),
    onSuccess: refresh,
  });
  const name = [user.firstName, user.lastName].filter(Boolean).join(" ") || "—";
  return (
    <tr>
      <td>{user.email}</td>
      <td>{name}</td>
      <td>
        <span className={`pill ${user.enabled ? "on" : "off"}`}>{user.enabled ? "enabled" : "disabled"}</span>
      </td>
      <td>
        <select
          aria-label="computer sharing"
          value={user.computerMode ?? ""}
          disabled={setMode.isPending}
          onChange={(e) => setMode.mutate(e.target.value as SharingMode | "")}
          style={{ width: "auto", padding: "0.3rem 0.5rem", fontSize: "0.82rem" }}
        >
          <option value="">Org default</option>
          <option value="per-org">Per-org</option>
          <option value="per-account">Per-account</option>
          <option value="per-bot">Per-bot</option>
        </select>
      </td>
      <td style={{ textAlign: "right" }}>
        <button className="ghost" onClick={() => toggle.mutate()} disabled={toggle.isPending}>
          {user.enabled ? "Disable" : "Enable"}
        </button>
      </td>
    </tr>
  );
}

function UsersCard() {
  const users = useQuery({ queryKey: ["admin", "users"], queryFn: listUsers, retry: false });

  if (users.error instanceof ApiError && users.error.status === 403) {
    return (
      <section className="card">
        <h2>Users</h2>
        <p className="muted">Admins only — you do not manage this organization.</p>
      </section>
    );
  }

  return (
    <section className="card">
      <h2>Users</h2>
      {users.isLoading ? (
        <p className="muted">Loading…</p>
      ) : users.data && users.data.users.length > 0 ? (
        <table>
          <thead>
            <tr>
              <th>Email</th>
              <th>Name</th>
              <th>State</th>
              <th>Computer</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {users.data.users.map((user) => (
              <UserRow key={user.id} user={user} />
            ))}
          </tbody>
        </table>
      ) : (
        <p className="muted">No members yet.</p>
      )}
    </section>
  );
}

function InvitesCard() {
  const queryClient = useQueryClient();
  const invites = useQuery({ queryKey: ["admin", "invites"], queryFn: listInvites, retry: false });
  const [issued, setIssued] = useState<{ code: string; link: string } | null>(null);
  const [copied, setCopied] = useState(false);

  const issue = useMutation({
    mutationFn: issueInvite,
    onSuccess: async (result) => {
      setIssued(result);
      setCopied(false);
      await queryClient.invalidateQueries({ queryKey: ["admin", "invites"] });
    },
  });

  if (invites.error instanceof ApiError && invites.error.status === 403) {
    return null; // The Users card already explains the admin gate.
  }

  async function copy(link: string) {
    try {
      await navigator.clipboard.writeText(link);
      setCopied(true);
    } catch {
      setCopied(false);
    }
  }

  return (
    <section className="card">
      <div className="spread">
        <h2 style={{ margin: 0 }}>Invites</h2>
        <button onClick={() => issue.mutate()} disabled={issue.isPending}>
          {issue.isPending ? "Issuing…" : "Issue invite"}
        </button>
      </div>

      {issued ? (
        <div className="note" style={{ marginTop: "1rem" }}>
          <div className="row spread">
            <code className="mono">{issued.link}</code>
            <button className="ghost" onClick={() => void copy(issued.link)}>
              {copied ? "Copied" : "Copy link"}
            </button>
          </div>
          <p className="muted" style={{ margin: "0.4rem 0 0", fontSize: "0.82rem" }}>
            Code <code className="mono">{issued.code}</code> — the person can paste it on the signup
            page, or just open this link.
          </p>
        </div>
      ) : null}

      <div style={{ marginTop: "1.25rem" }}>
        {invites.isLoading ? (
          <p className="muted">Loading…</p>
        ) : invites.data && invites.data.invites.length > 0 ? (
          <table>
            <thead>
              <tr>
                <th>Code</th>
                <th>State</th>
              </tr>
            </thead>
            <tbody>
              {invites.data.invites.map((invite) => (
                <tr key={invite.code}>
                  <td className="mono">{invite.code}</td>
                  <td>
                    <span className={`pill ${invite.state === "open" ? "on" : "off"}`}>{invite.state}</span>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        ) : (
          <p className="muted">No invites yet. Issue one to add a teammate.</p>
        )}
      </div>
    </section>
  );
}

function DomainRow({ entry }: { entry: OrgDomain }) {
  const queryClient = useQueryClient();
  const refresh = () => queryClient.invalidateQueries({ queryKey: ["admin", "domains"] });
  const [copied, setCopied] = useState(false);
  const verify = useMutation({ mutationFn: () => verifyDomain(entry.domain), onSuccess: refresh });
  const withdraw = useMutation({ mutationFn: () => withdrawDomain(entry.domain), onSuccess: refresh });
  const failure =
    verify.error instanceof ApiError ? verify.error.message : verify.error ? "Could not verify." : null;

  async function copy(value: string) {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
    } catch {
      setCopied(false);
    }
  }

  return (
    <li style={{ listStyle: "none", padding: "0.75rem 0", borderTop: "1px solid var(--border)" }}>
      <div className="spread">
        <div className="row">
          <span className="mono" style={{ fontSize: "0.95rem" }}>{entry.domain}</span>
          <span className={`pill ${entry.status === "verified" ? "on" : "off"}`}>{entry.status}</span>
        </div>
        {entry.status === "pending" ? (
          <div className="row">
            <button onClick={() => verify.mutate()} disabled={verify.isPending}>
              {verify.isPending ? "Checking…" : "Verify"}
            </button>
            <button className="ghost" onClick={() => withdraw.mutate()} disabled={withdraw.isPending}>
              Withdraw
            </button>
          </div>
        ) : null}
      </div>
      {entry.record ? (
        <div className="note" style={{ marginTop: "0.6rem" }}>
          <p className="muted" style={{ margin: "0 0 0.4rem", fontSize: "0.82rem" }}>
            Add this DNS record, then click Verify. Records can take a few minutes to appear.
          </p>
          <table style={{ margin: 0 }}>
            <tbody>
              <tr>
                <td className="muted">Type</td>
                <td className="mono">{entry.record.type}</td>
              </tr>
              <tr>
                <td className="muted">Name</td>
                <td className="mono">{entry.record.name}</td>
              </tr>
              <tr>
                <td className="muted">Value</td>
                <td>
                  <div className="row spread">
                    <code className="mono">{entry.record.value}</code>
                    <button className="ghost" onClick={() => void copy(entry.record?.value ?? "")}>
                      {copied ? "Copied" : "Copy"}
                    </button>
                  </div>
                </td>
              </tr>
            </tbody>
          </table>
          {failure ? <p className="err">{failure}</p> : null}
        </div>
      ) : null}
    </li>
  );
}

function DomainsCard() {
  const queryClient = useQueryClient();
  const domains = useQuery({ queryKey: ["admin", "domains"], queryFn: listDomains, retry: false });
  const [draft, setDraft] = useState("");
  const claim = useMutation({
    mutationFn: () => claimDomain(draft.trim()),
    onSuccess: async () => {
      setDraft("");
      await queryClient.invalidateQueries({ queryKey: ["admin", "domains"] });
    },
  });

  if (domains.error instanceof ApiError && domains.error.status === 403) {
    return null; // The Users card already explains the admin gate.
  }
  const claimError =
    claim.error instanceof ApiError ? claim.error.message : claim.error ? "Could not claim that domain." : null;

  return (
    <section className="card">
      <h2 style={{ margin: 0 }}>Domains</h2>
      <p className="muted" style={{ margin: "0.4rem 0 0", fontSize: "0.88rem" }}>
        People can sign up with an invite only from a verified domain. Claim one, publish the TXT
        record we give you, then verify it.
      </p>
      <form
        className="row"
        style={{ marginTop: "1rem" }}
        onSubmit={(e) => {
          e.preventDefault();
          if (draft.trim()) claim.mutate();
        }}
      >
        <input
          aria-label="domain to claim"
          placeholder="example.com"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          style={{ flex: 1, minWidth: "12rem" }}
        />
        <button type="submit" disabled={claim.isPending || !draft.trim()}>
          {claim.isPending ? "Claiming…" : "Claim domain"}
        </button>
      </form>
      {claimError ? <p className="err">{claimError}</p> : null}
      <div style={{ marginTop: "1rem" }}>
        {domains.isLoading ? (
          <p className="muted">Loading…</p>
        ) : domains.data && domains.data.domains.length > 0 ? (
          <ul style={{ margin: 0, padding: 0 }}>
            {domains.data.domains.map((entry) => (
              <DomainRow key={entry.domain} entry={entry} />
            ))}
          </ul>
        ) : (
          <p className="muted">No domains yet.</p>
        )}
      </div>
    </section>
  );
}

function ComputersCard() {
  const queryClient = useQueryClient();
  const computers = useQuery({ queryKey: ["admin", "computers"], queryFn: listOrgComputers, retry: false });
  const [key, setKey] = useState("");
  const [testResult, setTestResult] = useState<{ ok: boolean; detail: string } | null>(null);

  const save = useMutation({
    mutationFn: () => setBoxKey(key),
    onSuccess: async () => {
      setKey("");
      await queryClient.invalidateQueries({ queryKey: ["admin", "computers"] });
    },
  });
  const clear = useMutation({
    mutationFn: clearBoxKey,
    onSuccess: async () => {
      setTestResult(null);
      await queryClient.invalidateQueries({ queryKey: ["admin", "computers"] });
    },
  });
  const test = useMutation({ mutationFn: testBoxConnection, onSuccess: setTestResult });

  if (computers.error instanceof ApiError && computers.error.status === 403) return null;

  const box = computers.data?.computers.find((c) => c.kind === "ascii");
  const boxConfigured = box?.configured ?? false;

  const orgMode = useQuery({ queryKey: ["admin", "orgmode"], queryFn: getOrgMode, retry: false });
  const setMode = useMutation({
    mutationFn: setOrgMode,
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["admin", "orgmode"] }),
  });

  return (
    <section className="card">
      <h2>Computers</h2>
      <div style={{ marginBottom: "1.25rem" }}>
        <label htmlFor="orgmode">Default sharing mode</label>
        <select
          id="orgmode"
          value={orgMode.data?.mode ?? "per-account"}
          disabled={setMode.isPending || orgMode.isLoading}
          onChange={(e) => setMode.mutate(e.target.value as SharingMode)}
        >
          <option value="per-org">One computer for the whole org (shared filesystem)</option>
          <option value="per-account">One computer per member (default)</option>
          <option value="per-bot">A dedicated computer per bot (most isolated)</option>
        </select>
        <p className="muted" style={{ fontSize: "0.8rem", margin: "0.3rem 0 0" }}>
          How members’ bots share computers. Override per member in the Users list above.
        </p>
      </div>
      <p className="muted" style={{ marginTop: 0, fontSize: "0.88rem" }}>
        Where your organization’s bots run. Set a provider key and every member’s computer is
        provisioned from it — the key is sealed on the server and never leaves it.
      </p>

      <div style={{ marginTop: "1rem" }}>
        <div className="spread">
          <strong>box.ascii.dev</strong>
          <span className={`pill ${boxConfigured ? "on" : "off"}`}>
            {boxConfigured ? "configured" : "not configured"}
          </span>
        </div>
        <label htmlFor="boxkey">
          API key{boxConfigured ? " (saved — paste a new one to replace)" : ""}
        </label>
        <input
          id="boxkey"
          type="password"
          autoComplete="off"
          placeholder={boxConfigured ? "Replace saved key" : "box_…"}
          value={key}
          onChange={(e) => setKey(e.target.value)}
        />
        <div className="row" style={{ marginTop: "0.8rem" }}>
          <button onClick={() => save.mutate()} disabled={save.isPending || key.trim().length === 0}>
            {save.isPending ? "Saving…" : "Save key"}
          </button>
          <button className="ghost" onClick={() => test.mutate()} disabled={test.isPending || !boxConfigured}>
            {test.isPending ? "Testing…" : "Test connection"}
          </button>
          {boxConfigured ? (
            <button className="danger" onClick={() => clear.mutate()} disabled={clear.isPending}>
              Remove
            </button>
          ) : null}
        </div>
        {save.isError ? (
          <p className="err">{save.error instanceof ApiError ? save.error.message : "Could not save."}</p>
        ) : null}
        {testResult ? <p className={testResult.ok ? "note" : "err"}>{testResult.detail}</p> : null}
      </div>

      <div style={{ marginTop: "1.5rem", opacity: 0.55 }}>
        <div className="spread">
          <strong>Windows 365</strong>
          <span className="pill off">coming soon</span>
        </div>
        <p className="muted" style={{ fontSize: "0.82rem", margin: "0.3rem 0 0" }}>
          Windows 365 for Agents isn’t configurable yet.
        </p>
      </div>
    </section>
  );
}

/** One member's key row: the prefix, its cap, and revoke. */
function GatewayKeyRow({
  gkey,
  email,
}: {
  gkey: GatewayKey;
  email: string;
}) {
  const queryClient = useQueryClient();
  const refresh = () => queryClient.invalidateQueries({ queryKey: ["admin", "gateway", "keys"] });
  const [cap, setCap] = useState("");

  const revoke = useMutation({ mutationFn: () => revokeGatewayKey(gkey.id), onSuccess: refresh });
  const saveCap = useMutation({
    mutationFn: () => setGatewayKeyQuota(gkey.id, cap.trim() ? cap.trim() : null),
    onSuccess: refresh,
  });

  return (
    <tr>
      <td>{email}</td>
      <td>
        <code>{gkey.keyPrefix}…</code>
      </td>
      <td>{gkey.revoked ? "Revoked" : "Active"}</td>
      <td>
        {gkey.revoked ? (
          <span className="muted">—</span>
        ) : (
          <span className="row">
            <input
              type="text"
              inputMode="decimal"
              placeholder="no cap"
              value={cap}
              onChange={(e) => setCap(e.target.value)}
              aria-label={`Monthly cap for ${email}`}
              size={8}
            />
            <button onClick={() => saveCap.mutate()} disabled={saveCap.isPending}>
              Set cap
            </button>
          </span>
        )}
      </td>
      <td>
        {gkey.revoked ? null : (
          <button onClick={() => revoke.mutate()} disabled={revoke.isPending}>
            Revoke
          </button>
        )}
      </td>
    </tr>
  );
}

/**
 * Gateway access: the keys that open the model door for this org's members.
 *
 * A minted key is shown ONCE — the server keeps only its prefix, and the gateway only its hash —
 * so the reveal below is the single moment it can be copied. The budget and spend are read live
 * from the gateway rather than mirrored here, because a second copy of a number about money is a
 * number that will eventually disagree.
 */
function GatewayAccessCard() {
  const queryClient = useQueryClient();
  const users = useQuery({ queryKey: ["admin", "users"], queryFn: listUsers, retry: false });
  const keys = useQuery({
    queryKey: ["admin", "gateway", "keys"],
    queryFn: listGatewayKeys,
    retry: false,
  });
  const usage = useQuery({
    queryKey: ["admin", "gateway", "usage"],
    queryFn: gatewayUsage,
    retry: false,
  });

  const [member, setMember] = useState("");
  const [quota, setQuota] = useState("");
  const [budget, setBudget] = useState("");
  const [revealed, setRevealed] = useState<{ email: string; key: string; note?: string } | null>(null);

  // One nonce per PRESS, kept until the press succeeds: a retry after a lost reply carries the
  // same nonce and the server answers with the key that press already minted.
  const pressNonce = useRef<string | null>(null);
  const mint = useMutation({
    mutationFn: () => {
      pressNonce.current ??= crypto.randomUUID();
      return mintGatewayKey(member, quota, pressNonce.current);
    },
    onSuccess: (minted) => {
      pressNonce.current = null;
      setRevealed({ email: minted.label, key: minted.key ?? "", note: minted.note });
      setQuota("");
      queryClient.invalidateQueries({ queryKey: ["admin", "gateway"] });
    },
  });
  const saveBudget = useMutation({
    mutationFn: () => setGatewayBudget(budget.trim() ? budget.trim() : null),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["admin", "gateway", "usage"] }),
  });

  const notAdmin =
    keys.error instanceof ApiError && keys.error.status === 403;
  if (notAdmin) {
    return (
      <section className="card">
        <h2>Gateway access</h2>
        <p className="muted">Admins only — you do not manage this organization.</p>
      </section>
    );
  }

  // The deployment has not wired its gateway admin connection; say so plainly instead of
  // rendering controls that can only fail.
  const unwired =
    keys.error instanceof ApiError && keys.error.status === 503
      ? keys.error.message
      : usage.error instanceof ApiError && usage.error.status === 503
        ? usage.error.message
        : null;
  if (unwired) {
    return (
      <section className="card">
        <h2>Gateway access</h2>
        <p className="muted">{unwired}</p>
      </section>
    );
  }

  const emailFor = (id: string) =>
    users.data?.users.find((u) => u.id === id)?.email ?? id;

  return (
    <section className="card">
      <h2>Gateway access</h2>
      <p className="muted">
        A key here opens the model door for one member: they set it as their client's API token.
        Spending counts against this organization.
      </p>

      {usage.data ? (
        <p>
          <strong>{usage.data.monthToDateUsd}</strong> spent this month
          {usage.data.monthlyBudgetUsd ? ` of ${usage.data.monthlyBudgetUsd}` : " (no budget set)"} ·{" "}
          {usage.data.requests} requests
        </p>
      ) : null}

      <div className="row">
        <input
          type="text"
          inputMode="decimal"
          placeholder="Monthly budget, e.g. 50.00"
          value={budget}
          onChange={(e) => setBudget(e.target.value)}
          aria-label="Organization monthly budget"
        />
        <button onClick={() => saveBudget.mutate()} disabled={saveBudget.isPending}>
          Save budget
        </button>
      </div>
      {saveBudget.error instanceof Error ? (
        <p className="error">{saveBudget.error.message}</p>
      ) : null}

      <div className="row">
        <select value={member} onChange={(e) => setMember(e.target.value)} aria-label="Member">
          <option value="">Choose a member…</option>
          {users.data?.users.map((u) => (
            <option key={u.id} value={u.id}>
              {u.email}
            </option>
          ))}
        </select>
        <input
          type="text"
          inputMode="decimal"
          placeholder="Cap (optional)"
          value={quota}
          onChange={(e) => setQuota(e.target.value)}
          aria-label="Per-member cap"
          size={10}
        />
        <button onClick={() => mint.mutate()} disabled={!member || mint.isPending}>
          Mint key
        </button>
      </div>
      {mint.error instanceof Error ? <p className="error">{mint.error.message}</p> : null}

      {revealed ? (
        <div className="card inset">
          <p>
            <strong>{revealed.email}</strong>&rsquo;s key.{" "}
            {revealed.key ? "Copy it now — it is not shown again." : "This press had already minted a key."}
          </p>
          {revealed.key ? (
            <code className="wrap">{revealed.key}</code>
          ) : (
            <p className="muted">{revealed.note ?? "Its secret was shown once and cannot be shown again."}</p>
          )}
          <div className="row">
            {revealed.key ? (
              <button onClick={() => void navigator.clipboard?.writeText(revealed.key)}>Copy</button>
            ) : null}
            <button onClick={() => setRevealed(null)}>Done</button>
          </div>
        </div>
      ) : null}

      {keys.isLoading ? (
        <p className="muted">Loading…</p>
      ) : keys.data && keys.data.keys.length > 0 ? (
        <table>
          <thead>
            <tr>
              <th>Member</th>
              <th>Key</th>
              <th>State</th>
              <th>Cap</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {keys.data.keys.map((k) => (
              <GatewayKeyRow
                key={k.id}
                gkey={k}
                email={k.memberId ? emailFor(k.memberId) : `unattributed (${k.label || "no name"})`}
              />
            ))}
          </tbody>
        </table>
      ) : (
        <p className="muted">No keys yet.</p>
      )}
    </section>
  );
}

/** `1234567` → `1,234,567`, the way the server writes points in its sentences. */
function commas(points: number | null | undefined): string {
  if (points == null) return "—";
  return points.toLocaleString("en-US");
}

/** What N points are worth at the reference: N tokens at R dollars per million. */
function dollarsOf(points: number | null | undefined, usdPerMtok: string | null | undefined): string | null {
  if (points == null || !usdPerMtok) return null;
  const r = Number(usdPerMtok);
  if (!Number.isFinite(r)) return null;
  return `≈ $${((points * r) / 1_000_000).toFixed(2)}`;
}

/** A whole number of points from an input, null for blank; NaN is refused by the server. */
function pointsFromInput(raw: string): number | null {
  const text = raw.replace(/[,\s]/g, "");
  if (!text) return null;
  return Number(text);
}

/** One member's pool: a number and a Save; blank removes it. */
function PoolEditor({
  member,
  usdPerMtok,
}: {
  member: { id: string; email: string; pool: number | null };
  usdPerMtok: string | null;
}) {
  const queryClient = useQueryClient();
  const [pool, setPool] = useState(member.pool == null ? "" : String(member.pool));
  const save = useMutation({
    mutationFn: () => setMemberPool(member.id, pointsFromInput(pool)),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["admin", "points"] });
      queryClient.invalidateQueries({ queryKey: ["limit"] });
    },
  });
  return (
    <span className="row">
      <input
        type="text"
        inputMode="numeric"
        value={pool}
        onChange={(e) => setPool(e.target.value)}
        placeholder="no pool"
        aria-label={`Monthly pool for ${member.email}`}
        style={{ width: "9rem" }}
      />
      <span className="muted">{dollarsOf(pointsFromInput(pool), usdPerMtok) ?? ""}</span>
      <button onClick={() => save.mutate()} disabled={save.isPending}>
        Save
      </button>
      {save.error ? <span className="error">{errorText(save.error, "could not save")}</span> : null}
    </span>
  );
}

/**
 * Points: one point is one token at the reference price (set here, kept on the gateway with the
 * prices); every model's multiplier follows from its list price, so a subscription seat and an
 * API key count the same. The admin sets each member's monthly pool; a member caps their own
 * coworkers on the coworkers page, at most the pool. A coworker at a limit has its turn refused
 * with a sentence in the bubble. No pool and no cap anywhere means nothing is metered.
 */
function PointsCard() {
  const queryClient = useQueryClient();
  const overview = useQuery({ queryKey: ["admin", "points"], queryFn: getPointsOverview, retry: false });
  const [reference, setReference] = useState<string | null>(null);
  const saveReference = useMutation({
    mutationFn: () => setPointsReference((reference ?? "").trim()),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["admin", "points"] });
      queryClient.invalidateQueries({ queryKey: ["models"] });
      setReference(null);
    },
  });
  if (overview.error instanceof ApiError && overview.error.status === 403) {
    return (
      <section className="card">
        <h2>Points</h2>
        <p className="muted">Admins only — you do not manage this organization.</p>
      </section>
    );
  }
  const usdPerMtok = overview.data?.reference?.usdPerMtok ?? null;
  const shownReference = reference ?? usdPerMtok ?? "";
  return (
    <section className="card">
      <h2>Points</h2>
      <p className="muted">
        One point is one token at the reference price below (USD per million tokens); a model's
        multiplier is its list price over it, so a subscription seat counts the same as an API key.
        Set each member's monthly pool; members cap their own coworkers at most the pool. At a limit
        a turn is refused with a sentence that names the numbers and when it frees up.
      </p>
      {overview.isLoading ? (
        <p className="muted">Loading…</p>
      ) : overview.error ? (
        <p className="error">{errorText(overview.error, "could not load points")}</p>
      ) : overview.data ? (
        <div className="stack">
          {overview.data.note ? <p className="muted">{overview.data.note}</p> : null}
          <div>
            <h3>Reference price</h3>
            <span className="row">
              <input
                type="text"
                inputMode="decimal"
                value={shownReference}
                onChange={(e) => setReference(e.target.value)}
                placeholder="0.20"
                aria-label="Reference price, USD per million tokens"
                style={{ width: "7rem" }}
              />
              <span className="muted">USD per million tokens · 1,000,000 points {dollarsOf(1_000_000, shownReference) ?? ""}</span>
              <button onClick={() => saveReference.mutate()} disabled={saveReference.isPending || reference == null}>
                Save
              </button>
              {saveReference.error ? (
                <span className="error">{errorText(saveReference.error, "could not save")}</span>
              ) : null}
            </span>
          </div>
          <div>
            <h3>Members</h3>
            <table>
              <thead>
                <tr>
                  <th>Member</th>
                  <th>Used this month</th>
                  <th>Monthly pool</th>
                </tr>
              </thead>
              <tbody>
                {overview.data.members.map((m) => (
                  <tr key={m.id}>
                    <td>{m.email}</td>
                    <td>
                      {commas(m.usedPoints)}
                      <span className="muted"> {dollarsOf(m.usedPoints, usdPerMtok) ?? ""}</span>
                    </td>
                    <td>
                      <PoolEditor member={m} usdPerMtok={usdPerMtok} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          <div>
            <h3>Coworkers</h3>
            {overview.data.coworkers.length === 0 ? (
              <p className="muted">No coworkers hired yet.</p>
            ) : (
              <table>
                <thead>
                  <tr>
                    <th>Coworker</th>
                    <th>Hired by</th>
                    <th>Used this month</th>
                    <th>Cap / day</th>
                  </tr>
                </thead>
                <tbody>
                  {overview.data.coworkers.map((c) => (
                    <tr key={c.id}>
                      <td>{c.name}</td>
                      <td>{c.ownerEmail}</td>
                      <td>{commas(c.usedPoints)}</td>
                      <td>
                        {c.cap == null && c.dayCap == null ? (
                          <span className="muted">none — draws on the pool</span>
                        ) : (
                          <>
                            {c.cap == null ? "no cap" : commas(c.cap)} / {c.dayCap == null ? "no brake" : commas(c.dayCap)}
                          </>
                        )}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </div>
        </div>
      ) : null}
    </section>
  );
}

/** One template's fields; blank points mean "this template sets no limit". */
function TemplateEditor({
  tools,
  initial,
  onSave,
  saveLabel,
}: {
  tools: string[];
  initial: TemplateInput;
  onSave: (input: TemplateInput) => Promise<unknown>;
  saveLabel: string;
}) {
  const queryClient = useQueryClient();
  const [draft, setDraft] = useState<TemplateInput>(initial);
  const save = useMutation({
    mutationFn: () =>
      onSave({
        ...draft,
        name: draft.name.trim(),
        description: draft.description.trim(),
        model: draft.model && draft.model.trim() ? draft.model.trim() : null,
        points: {
          monthPoints: draft.points.monthPoints ?? null,
          dayPoints: draft.points.dayPoints ?? null,
        },
      }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["admin", "templates"] });
      queryClient.invalidateQueries({ queryKey: ["templates"] });
      if (saveLabel === "Create") setDraft(initial);
    },
  });
  const toggle = (list: string[], tool: string) =>
    list.includes(tool) ? list.filter((t) => t !== tool) : [...list, tool];
  return (
    <div className="stack">
      <span className="row">
        <input
          type="text"
          value={draft.name}
          onChange={(e) => setDraft({ ...draft, name: e.target.value })}
          placeholder="Name"
          aria-label="Template name"
        />
        <input
          type="text"
          value={draft.model ?? ""}
          onChange={(e) => setDraft({ ...draft, model: e.target.value })}
          placeholder="Route (blank = deployment default)"
          aria-label="Template route"
        />
      </span>
      <input
        type="text"
        value={draft.description}
        onChange={(e) => setDraft({ ...draft, description: e.target.value })}
        placeholder="Description (the coworker's profile)"
        aria-label="Template description"
      />
      <span className="row">
        {tools.map((tool) => (
          <label key={tool}>
            <input
              type="checkbox"
              checked={draft.tools.includes(tool)}
              onChange={() =>
                setDraft({
                  ...draft,
                  tools: toggle(draft.tools, tool),
                  needsApproval: draft.needsApproval.filter((t) => t !== tool || !draft.tools.includes(tool)),
                })
              }
            />{" "}
            {tool}
            {draft.tools.includes(tool) ? (
              <label className="muted">
                {" "}
                <input
                  type="checkbox"
                  checked={draft.needsApproval.includes(tool)}
                  onChange={() => setDraft({ ...draft, needsApproval: toggle(draft.needsApproval, tool) })}
                />{" "}
                ask first
              </label>
            ) : null}
          </label>
        ))}
      </span>
      <span className="row">
        <input
          type="text"
          inputMode="numeric"
          value={draft.points.monthPoints == null ? "" : String(draft.points.monthPoints)}
          onChange={(e) => setDraft({ ...draft, points: { ...draft.points, monthPoints: pointsFromInput(e.target.value) } })}
          placeholder="month points"
          aria-label="Template monthly cap in points"
          style={{ width: "8rem" }}
        />
        <input
          type="text"
          inputMode="numeric"
          value={draft.points.dayPoints == null ? "" : String(draft.points.dayPoints)}
          onChange={(e) => setDraft({ ...draft, points: { ...draft.points, dayPoints: pointsFromInput(e.target.value) } })}
          placeholder="day points"
          aria-label="Template daily brake in points"
          style={{ width: "8rem" }}
        />
        <button onClick={() => save.mutate()} disabled={save.isPending || !draft.name.trim()}>
          {saveLabel}
        </button>
        {save.error ? <span className="error">{errorText(save.error, "could not save")}</span> : null}
      </span>
    </div>
  );
}

const EMPTY_TEMPLATE: TemplateInput = {
  name: "",
  description: "",
  model: null,
  tools: ["shell", "read_file", "write_file"],
  needsApproval: [],
  points: {},
};

function TemplateRow({ template, tools }: { template: CoworkerTemplate; tools: string[] }) {
  const queryClient = useQueryClient();
  const [editing, setEditing] = useState(false);
  const remove = useMutation({
    mutationFn: () => deleteTemplate(template.id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["admin", "templates"] });
      queryClient.invalidateQueries({ queryKey: ["templates"] });
    },
  });
  const limits = [
    template.points.monthPoints != null ? `${commas(template.points.monthPoints)} points / month` : null,
    template.points.dayPoints != null ? `${commas(template.points.dayPoints)} points / day` : null,
  ].filter((x) => x != null);
  return (
    <>
      <tr>
        <td>
          <strong>{template.name}</strong>
          {template.description ? <span className="muted"> — {template.description}</span> : null}
        </td>
        <td>
          <code>{template.model ?? "default"}</code>
        </td>
        <td>
          {template.tools.join(", ") || "talk only"}
          {template.needsApproval.length > 0 ? (
            <span className="muted"> (ask first: {template.needsApproval.join(", ")})</span>
          ) : null}
        </td>
        <td>{limits.length > 0 ? limits.join(", ") : <span className="muted">none</span>}</td>
        <td>
          <span className="row">
            <button onClick={() => setEditing((open) => !open)}>{editing ? "Close" : "Edit"}</button>
            <button onClick={() => remove.mutate()} disabled={remove.isPending}>
              Delete
            </button>
          </span>
          {remove.error ? <p className="error">{errorText(remove.error, "could not delete")}</p> : null}
        </td>
      </tr>
      {editing ? (
        <tr>
          <td colSpan={5}>
            <TemplateEditor
              tools={tools}
              initial={{
                name: template.name,
                description: template.description,
                model: template.model,
                tools: template.tools,
                needsApproval: template.needsApproval,
                points: template.points,
              }}
              onSave={(input) => updateTemplate(template.id, input)}
              saveLabel="Save"
            />
            <p className="muted">
              Coworkers already hired from this template keep what they were hired with.
            </p>
          </td>
        </tr>
      ) : null}
    </>
  );
}

/**
 * Coworker templates: a type the admin writes once — route, tools, what needs a human yes,
 * spend limits — that members pick when they hire. What it says is copied to the coworker at
 * hire; editing or deleting the template changes no running coworker.
 */
function TemplatesCard() {
  const templates = useQuery({ queryKey: ["admin", "templates"], queryFn: listTemplates, retry: false });
  if (templates.error instanceof ApiError && templates.error.status === 403) {
    return (
      <section className="card">
        <h2>Coworker templates</h2>
        <p className="muted">Admins only — you do not manage this organization.</p>
      </section>
    );
  }
  const tools = templates.data?.tools ?? EMPTY_TEMPLATE.tools;
  return (
    <section className="card">
      <h2>Coworker templates</h2>
      <p className="muted">
        A coworker type: route, tools, what needs a human yes, spend limits. Members pick one
        when they hire; what it says is copied to the coworker then and there.
      </p>
      {templates.isLoading ? (
        <p className="muted">Loading…</p>
      ) : templates.error ? (
        <p className="error">{errorText(templates.error, "could not load templates")}</p>
      ) : (
        <div className="stack">
          {templates.data && templates.data.templates.length > 0 ? (
            <table>
              <thead>
                <tr>
                  <th>Template</th>
                  <th>Route</th>
                  <th>Tools</th>
                  <th>Limits</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {templates.data.templates.map((t) => (
                  <TemplateRow key={t.id} template={t} tools={tools} />
                ))}
              </tbody>
            </table>
          ) : (
            <p className="muted">No templates yet.</p>
          )}
          <h3>New template</h3>
          <TemplateEditor tools={tools} initial={EMPTY_TEMPLATE} onSave={createTemplate} saveLabel="Create" />
        </div>
      )}
    </section>
  );
}

export function AdminPage() {
  return (
    <AuthedFrame requireAdmin>
      {() => (
        <div className="stack">
          <UsersCard />
          <GatewayAccessCard />
          <PointsCard />
          <TemplatesCard />
          <ComputersCard />
          <DomainsCard />
          <InvitesCard />
        </div>
      )}
    </AuthedFrame>
  );
}
