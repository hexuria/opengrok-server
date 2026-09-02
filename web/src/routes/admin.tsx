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
  getSpendLimits,
  setOrgSpendLimit,
  setMemberSpendLimit,
  setCoworkerSpendLimit,
  type SpendLimit,
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

/** Three inputs and a Save: the row's limits, or blank for "follows the layer above". */
function LimitEditor({
  label,
  current,
  onSave,
}: {
  label: string;
  current: SpendLimit | null;
  onSave: (limit: SpendLimit) => Promise<void>;
}) {
  const queryClient = useQueryClient();
  const [five, setFive] = useState(current?.fiveHourUsd ?? "");
  const [seven, setSeven] = useState(current?.sevenDayUsd ?? "");
  const [month, setMonth] = useState(current?.monthUsd ?? "");
  const save = useMutation({
    mutationFn: () =>
      onSave({
        fiveHourUsd: five.trim() ? five.trim() : null,
        sevenDayUsd: seven.trim() ? seven.trim() : null,
        monthUsd: month.trim() ? month.trim() : null,
      }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["admin", "spend"] });
      queryClient.invalidateQueries({ queryKey: ["spend"] });
    },
  });
  const field = (value: string, set: (v: string) => void, name: string) => (
    <input
      type="text"
      value={value}
      onChange={(e) => set(e.target.value)}
      placeholder="—"
      aria-label={`${name} limit for ${label}`}
      style={{ width: "6rem" }}
    />
  );
  return (
    <span className="row">
      {field(five, setFive, "5-hour")}
      {field(seven, setSeven, "7-day")}
      {field(month, setMonth, "monthly")}
      <button onClick={() => save.mutate()} disabled={save.isPending}>
        Save
      </button>
      {save.error ? <span className="error">{errorText(save.error, "could not save")}</span> : null}
    </span>
  );
}

/**
 * Spend limits: a rolling 5-hour window, a rolling 7-day window and the calendar month, in
 * USD, at three scopes. The org default applies to every coworker; a member's row overrides it
 * for that member's coworkers; a coworker's row overrides both. Per window, the most specific
 * value wins and a blank means "follows the layer above". No limits anywhere means nothing is
 * metered and nothing is refused. Enforced before every model call from the gateway's ledger.
 */
function SpendLimitsCard() {
  const limits = useQuery({ queryKey: ["admin", "spend"], queryFn: getSpendLimits, retry: false });
  if (limits.error instanceof ApiError && limits.error.status === 403) {
    return (
      <section className="card">
        <h2>Spend limits</h2>
        <p className="muted">Admins only — you do not manage this organization.</p>
      </section>
    );
  }
  return (
    <section className="card">
      <h2>Spend limits</h2>
      <p className="muted">
        USD per rolling 5 hours, per rolling 7 days, and per calendar month. Blank follows the
        layer above; nothing set anywhere means no limit. A coworker at a limit has its turn
        refused with a sentence that names the window and when it frees up.
      </p>
      {limits.isLoading ? (
        <p className="muted">Loading…</p>
      ) : limits.error ? (
        <p className="error">{errorText(limits.error, "could not load limits")}</p>
      ) : limits.data ? (
        <div className="stack">
          <div>
            <h3>Org default</h3>
            <LimitEditor label="the org" current={limits.data.org} onSave={setOrgSpendLimit} />
          </div>
          <div>
            <h3>Members</h3>
            <table>
              <thead>
                <tr>
                  <th>Member</th>
                  <th>5 h / 7 d / month</th>
                </tr>
              </thead>
              <tbody>
                {limits.data.members.map((m) => (
                  <tr key={m.id}>
                    <td>{m.email}</td>
                    <td>
                      <LimitEditor label={m.email} current={m.limits} onSave={(l) => setMemberSpendLimit(m.id, l)} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          <div>
            <h3>Coworkers</h3>
            {limits.data.coworkers.length === 0 ? (
              <p className="muted">No coworkers hired yet.</p>
            ) : (
              <table>
                <thead>
                  <tr>
                    <th>Coworker</th>
                    <th>Hired by</th>
                    <th>5 h / 7 d / month</th>
                  </tr>
                </thead>
                <tbody>
                  {limits.data.coworkers.map((c) => (
                    <tr key={c.id}>
                      <td>{c.name}</td>
                      <td>{c.ownerEmail}</td>
                      <td>
                        <LimitEditor label={c.name} current={c.limits} onSave={(l) => setCoworkerSpendLimit(c.id, l)} />
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

/** One template's fields; blank limits mean "this template says nothing about that window". */
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
        limits: {
          fiveHourUsd: draft.limits.fiveHourUsd?.trim() ? draft.limits.fiveHourUsd.trim() : null,
          sevenDayUsd: draft.limits.sevenDayUsd?.trim() ? draft.limits.sevenDayUsd.trim() : null,
          monthUsd: draft.limits.monthUsd?.trim() ? draft.limits.monthUsd.trim() : null,
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
          value={draft.limits.fiveHourUsd ?? ""}
          onChange={(e) => setDraft({ ...draft, limits: { ...draft.limits, fiveHourUsd: e.target.value } })}
          placeholder="5 h USD"
          aria-label="Template 5-hour limit"
          style={{ width: "6rem" }}
        />
        <input
          type="text"
          value={draft.limits.sevenDayUsd ?? ""}
          onChange={(e) => setDraft({ ...draft, limits: { ...draft.limits, sevenDayUsd: e.target.value } })}
          placeholder="7 d USD"
          aria-label="Template 7-day limit"
          style={{ width: "6rem" }}
        />
        <input
          type="text"
          value={draft.limits.monthUsd ?? ""}
          onChange={(e) => setDraft({ ...draft, limits: { ...draft.limits, monthUsd: e.target.value } })}
          placeholder="month USD"
          aria-label="Template monthly limit"
          style={{ width: "6rem" }}
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
  limits: {},
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
    template.limits.fiveHourUsd ? `$${template.limits.fiveHourUsd} / 5 h` : null,
    template.limits.sevenDayUsd ? `$${template.limits.sevenDayUsd} / 7 d` : null,
    template.limits.monthUsd ? `$${template.limits.monthUsd} / month` : null,
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
                limits: template.limits,
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
          <SpendLimitsCard />
          <TemplatesCard />
          <ComputersCard />
          <DomainsCard />
          <InvitesCard />
        </div>
      )}
    </AuthedFrame>
  );
}
