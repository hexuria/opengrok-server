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

// ---- Points: the reference price on the gateway, members' pools, the overview ----

/** A month's cap and a day's brake, in points; null or absent means "no limit here". */
export interface PointsLimit {
  monthPoints?: number | null;
  dayPoints?: number | null;
}

export interface PointsOverview {
  /** null while the gateway has no reference price (or is older than open-ai-gateway #52). */
  reference: { usdPerMtok: string } | null;
  note: string | null;
  members: { id: string; email: string; pool: number | null; setBy: string | null; usedPoints: number | null }[];
  coworkers: {
    id: string;
    name: string;
    ownerEmail: string;
    cap: number | null;
    dayCap: number | null;
    usedPoints: number | null;
  }[];
}

export function getPointsOverview(): Promise<PointsOverview> {
  return getJson<PointsOverview>("/admin/points");
}

async function putJson(path: string, body: unknown): Promise<void> {
  const res = await request(path, {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!res.ok) throw new ApiError(res.status, await res.text());
}

/** The reference price, USD per million tokens: one point is one token at it. */
export function setPointsReference(usdPerMtok: string): Promise<void> {
  return putJson("/admin/points/reference", { usdPerMtok });
}

/** A member's monthly pool; null removes it. */
export function setMemberPool(accountId: string, pool: number | null): Promise<void> {
  return putJson(`/admin/points/members/${encodeURIComponent(accountId)}`, { pool });
}

// ---- Coworker templates: types the admin writes, members hire from ----

export interface TemplateInput {
  name: string;
  description: string;
  model: string | null;
  tools: string[];
  needsApproval: string[];
  points: PointsLimit;
}

export interface CoworkerTemplate extends TemplateInput {
  id: string;
  updatedAtMs: number;
}

export function listTemplates(): Promise<{ templates: CoworkerTemplate[]; tools: string[] }> {
  return getJson<{ templates: CoworkerTemplate[]; tools: string[] }>("/admin/templates");
}

export function createTemplate(input: TemplateInput): Promise<CoworkerTemplate> {
  return postJson<CoworkerTemplate>("/admin/templates", input);
}

export async function updateTemplate(id: string, input: TemplateInput): Promise<CoworkerTemplate> {
  const res = await request(`/admin/templates/${encodeURIComponent(id)}`, {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(input),
  });
  if (!res.ok) throw new ApiError(res.status, await res.text());
  return (await res.json()) as CoworkerTemplate;
}

export async function deleteTemplate(id: string): Promise<void> {
  const res = await request(`/admin/templates/${encodeURIComponent(id)}`, { method: "DELETE" });
  if (!res.ok) throw new ApiError(res.status, await res.text());
}
