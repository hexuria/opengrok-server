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

// ---- Domains: claim, publish the TXT record, verify ----
//
// A verified domain admits signups; a pending one admits nobody until the record we hand back
// resolves. The server does the lookup on `verifyDomain` and says exactly why it failed (409) or
// that it could not look at all (503) — the card shows that sentence rather than a generic "no".

export interface DomainRecord {
  name: string;
  type: "TXT";
  value: string;
}

export interface OrgDomain {
  domain: string;
  status: "verified" | "pending";
  record?: DomainRecord;
}

export function listDomains(): Promise<{ domains: OrgDomain[] }> {
  return getJson<{ domains: OrgDomain[] }>("/admin/domains");
}

export function claimDomain(domain: string): Promise<OrgDomain> {
  return postJson<OrgDomain>("/admin/domains", { domain });
}

export function verifyDomain(domain: string): Promise<OrgDomain> {
  return postJson<OrgDomain>(`/admin/domains/${encodeURIComponent(domain)}/verify`);
}

export async function withdrawDomain(domain: string): Promise<void> {
  const res = await request(`/admin/domains/${encodeURIComponent(domain)}`, { method: "DELETE" });
  if (!res.ok) throw new ApiError(res.status, await res.text());
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
  /** null for a key the gateway holds for this org that the console never recorded. */
  memberId: string | null;
  keyPrefix: string;
  label: string;
  revoked: boolean;
  createdAtMs: number | null;
  /** True when the gateway knows the key and we do not — revoke it there, or mint anew. */
  unattributed: boolean;
}

export interface MintedGatewayKey extends Omit<GatewayKey, "revoked" | "createdAtMs" | "unattributed"> {
  /** Shown once. Never fetchable again. null when this press had already minted (alreadyMinted). */
  key: string | null;
  /** The same press again: the key exists, its secret was shown the first time. */
  alreadyMinted: boolean;
  note?: string;
}

export interface GatewayUsage {
  monthlyBudgetUsd: string | null;
  monthToDateUsd: string;
  requests: number;
  provisioned: boolean;
}

/** `reconciled` is false when the gateway did not answer and the rows are our own record only. */
export function listGatewayKeys(): Promise<{ keys: GatewayKey[]; reconciled: boolean }> {
  return getJson<{ keys: GatewayKey[]; reconciled: boolean }>("/admin/gateway/keys");
}

/**
 * `clientNonce` names THIS press: a retry of the same press (a lost reply, a double click) sends
 * the same nonce and gets the key it already minted back — without the secret — instead of a
 * second real key.
 */
export function mintGatewayKey(memberId: string, quotaUsd: string | undefined, clientNonce: string): Promise<MintedGatewayKey> {
  return postJson<MintedGatewayKey>("/admin/gateway/keys", {
    memberId,
    quotaUsd: quotaUsd && quotaUsd.trim() ? quotaUsd.trim() : undefined,
    clientNonce,
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

// ---- Spend limits: three windows, three scopes, the admin writes them ----

/** The three limits, each optional; null or absent means "this layer says nothing". */
export interface SpendLimit {
  fiveHourUsd?: string | null;
  sevenDayUsd?: string | null;
  monthUsd?: string | null;
}

export interface SpendLimits {
  org: SpendLimit | null;
  members: { id: string; email: string; limits: SpendLimit | null }[];
  coworkers: { id: string; name: string; ownerEmail: string; limits: SpendLimit | null }[];
}

export function getSpendLimits(): Promise<SpendLimits> {
  return getJson<SpendLimits>("/admin/spend");
}

async function putLimit(path: string, limit: SpendLimit): Promise<void> {
  const res = await request(path, {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(limit),
  });
  if (!res.ok) throw new ApiError(res.status, await res.text());
}

export function setOrgSpendLimit(limit: SpendLimit): Promise<void> {
  return putLimit("/admin/spend/org", limit);
}

export function setMemberSpendLimit(accountId: string, limit: SpendLimit): Promise<void> {
  return putLimit(`/admin/spend/members/${encodeURIComponent(accountId)}`, limit);
}

export function setCoworkerSpendLimit(coworkerId: string, limit: SpendLimit): Promise<void> {
  return putLimit(`/admin/spend/coworkers/${encodeURIComponent(coworkerId)}`, limit);
}
