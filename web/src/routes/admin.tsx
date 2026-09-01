import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import {
  clearAccountMode,
  clearBoxKey,
  disableUser,
  enableUser,
  gatewayUsage,
  getOrgMode,
  issueInvite,
  listGatewayKeys,
  listInvites,
  listOrgComputers,
  listUsers,
  mintGatewayKey,
  revokeGatewayKey,
  setAccountMode,
  setBoxKey,
  setGatewayBudget,
  setGatewayKeyQuota,
  setOrgMode,
  testBoxConnection,
  type GatewayKey,
  type SharingMode,
} from "../api/admin";
import { ApiError } from "../api/client";
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
  const [revealed, setRevealed] = useState<{ email: string; key: string } | null>(null);

  const mint = useMutation({
    mutationFn: () => mintGatewayKey(member, quota),
    onSuccess: (minted) => {
      setRevealed({ email: minted.label, key: minted.key });
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
            <strong>{revealed.email}</strong>&rsquo;s key. Copy it now — it is not shown again.
          </p>
          <code className="wrap">{revealed.key}</code>
          <div className="row">
            <button onClick={() => void navigator.clipboard?.writeText(revealed.key)}>Copy</button>
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
              <GatewayKeyRow key={k.id} gkey={k} email={emailFor(k.memberId)} />
            ))}
          </tbody>
        </table>
      ) : (
        <p className="muted">No keys yet.</p>
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
          <ComputersCard />
          <InvitesCard />
        </div>
      )}
    </AuthedFrame>
  );
}
