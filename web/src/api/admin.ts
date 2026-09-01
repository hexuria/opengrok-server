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

// ---- Computer sharing mode (admin) ----

export type SharingMode = "per-org" | "per-account" | "per-bot";

export function getOrgMode(): Promise<{ mode: SharingMode; modes: SharingMode[] }> {
  return getJson<{ mode: SharingMode; modes: SharingMode[] }>("/admin/computers/mode");
}

export async function setOrgMode(mode: SharingMode): Promise<void> {
  const res = await request("/admin/computers/mode", {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ mode }),
  });
  if (!res.ok) throw new ApiError(res.status, "could not set the mode");
}

export async function setAccountMode(id: string, mode: SharingMode): Promise<void> {
  const res = await request(`/admin/computers/mode/account/${encodeURIComponent(id)}`, {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ mode }),
  });
  if (!res.ok) throw new ApiError(res.status, "could not set the override");
}

export async function clearAccountMode(id: string): Promise<void> {
  const res = await request(`/admin/computers/mode/account/${encodeURIComponent(id)}`, {
    method: "DELETE",
  });
  if (!res.ok) throw new ApiError(res.status, "could not clear the override");
}

// ---- Gateway keys: one identity, two doors ----
//
// A member's key opens the model door (open-ai-gateway). The secret comes back exactly ONCE, in
// the mint reply — the server never stores it and cannot show it again, so the UI must hand it to
// the person there and then.

export interface GatewayKey {
  id: string;
  memberId: string;
  keyPrefix: string;
  label: string;
  revoked: boolean;
  createdAtMs: number;
}

export interface MintedGatewayKey extends Omit<GatewayKey, "revoked" | "createdAtMs"> {
  /** Shown once. Never fetchable again. */
  key: string;
}

export interface GatewayUsage {
  monthlyBudgetUsd: string | null;
  monthToDateUsd: string;
  requests: number;
  provisioned: boolean;
}

export function listGatewayKeys(): Promise<{ keys: GatewayKey[] }> {
  return getJson<{ keys: GatewayKey[] }>("/admin/gateway/keys");
}

export function mintGatewayKey(memberId: string, quotaUsd?: string): Promise<MintedGatewayKey> {
  return postJson<MintedGatewayKey>("/admin/gateway/keys", {
    memberId,
    quotaUsd: quotaUsd && quotaUsd.trim() ? quotaUsd.trim() : undefined,
  });
}

export async function revokeGatewayKey(id: string): Promise<void> {
  const res = await request(`/admin/gateway/keys/${encodeURIComponent(id)}`, { method: "DELETE" });
  if (!res.ok) throw new ApiError(res.status, await res.text());
}

export async function setGatewayKeyQuota(id: string, quotaUsd: string | null): Promise<void> {
  const res = await request(`/admin/gateway/keys/${encodeURIComponent(id)}/quota`, {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ quotaUsd }),
  });
  if (!res.ok) throw new ApiError(res.status, await res.text());
}

export async function setGatewayBudget(monthlyBudgetUsd: string | null): Promise<void> {
  const res = await request("/admin/gateway/budget", {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ monthlyBudgetUsd }),
  });
  if (!res.ok) throw new ApiError(res.status, await res.text());
}

export function gatewayUsage(): Promise<GatewayUsage> {
  return getJson<GatewayUsage>("/admin/gateway/usage");
}
