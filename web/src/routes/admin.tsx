import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import {
  clearBoxKey,
  disableUser,
  enableUser,
  issueInvite,
  listInvites,
  listOrgComputers,
  listUsers,
  setBoxKey,
  testBoxConnection,
} from "../api/admin";
import { ApiError } from "../api/client";
import type { Account } from "../api/account";
import { AuthedFrame } from "../components/authed-frame";

function UserRow({ user }: { user: Account }) {
  const queryClient = useQueryClient();
  const toggle = useMutation({
    mutationFn: () => (user.enabled ? disableUser(user.id) : enableUser(user.id)),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["admin", "users"] }),
  });
  const name = [user.firstName, user.lastName].filter(Boolean).join(" ") || "—";
  return (
    <tr>
      <td>{user.email}</td>
      <td>{name}</td>
      <td>
        <span className={`pill ${user.enabled ? "on" : "off"}`}>{user.enabled ? "enabled" : "disabled"}</span>
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

  return (
    <section className="card">
      <h2>Computers</h2>
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

export function AdminPage() {
  return (
    <AuthedFrame requireAdmin>
      {() => (
        <div className="stack">
          <UsersCard />
          <ComputersCard />
          <InvitesCard />
        </div>
      )}
    </AuthedFrame>
  );
}
