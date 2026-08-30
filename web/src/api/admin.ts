// The org-admin surface. Every call here is admin-only server-side; a 403 means "not an admin".
import { getJson, postJson } from "./client";
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
