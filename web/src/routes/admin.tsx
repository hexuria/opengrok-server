import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { disableUser, enableUser, issueInvite, listInvites, listUsers } from "../api/admin";
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

export function AdminPage() {
  return (
    <AuthedFrame requireAdmin>
      {() => (
        <div className="stack">
          <UsersCard />
          <InvitesCard />
        </div>
      )}
    </AuthedFrame>
  );
}
