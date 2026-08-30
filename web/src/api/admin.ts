// The org-admin surface. Every call here is admin-only server-side; a 403 means "not an admin".
import { getJson, postJson, request, ApiError } from "./client";
import type { Account } from "./account";

export function listUsers(): Promise<{ users: Account[] }> {
  return getJson<{ users: Account[] }>("/admin/users");
}

export function enableUser(id: string): Promise<Account> {
  return postJson<Account>(`/admin/users/${encodeURIComponent(id)}/enable`);
}

export function disableUser(id: string): Promise<Account> {
  return postJson<Account>(`/admin/users/${encodeURIComponent(id)}/disable`);
}

export interface Invite {
  code: string;
  state: "open" | "redeemed" | "revoked";
}

export function listInvites(): Promise<{ invites: Invite[] }> {
  return getJson<{ invites: Invite[] }>("/admin/invites");
}

export function issueInvite(): Promise<{ code: string; link: string }> {
  return postJson<{ code: string; link: string }>("/admin/invites");
}

// ---- Org computer credentials (admin dashboard) ----

export interface OrgComputer {
  kind: string;
  label: string;
  configured: boolean;
}

export function listOrgComputers(): Promise<{ computers: OrgComputer[] }> {
  return getJson<{ computers: OrgComputer[] }>("/admin/computers");
}

export function setBoxKey(apiKey: string): Promise<void> {
  return postJson<void>("/admin/computers/ascii", { apiKey });
}

export async function clearBoxKey(): Promise<void> {
  const res = await request("/admin/computers/ascii", { method: "DELETE" });
  if (!res.ok) throw new ApiError(res.status, "could not clear the key");
}

export function testBoxConnection(): Promise<{ ok: boolean; detail: string }> {
  return postJson<{ ok: boolean; detail: string }>("/admin/computers/ascii/test");
}
