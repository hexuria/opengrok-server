// The account self-service surface — the JSON the server already speaks (account_api.rs).
import { getJson, postJson } from "./client";

/** The signed-in person's profile. camelCase exactly as the server sends it. */
export interface Account {
  id: string;
  email: string;
  firstName: string;
  lastName: string;
  avatarUrl: string | null;
  orgId: string | null;
  verified: boolean;
  enabled: boolean;
  /** Whether this caller is their org's admin. Only the GET /account (me) response carries it. */
  isAdmin?: boolean;
  /** This member's computer-sharing override (admin list only); null = follows the org default. */
  computerMode?: string | null;
}

export function getAccount(): Promise<Account> {
  return getJson<Account>("/account");
}

export interface ProfileUpdate {
  firstName?: string;
  lastName?: string;
  /** A data: URL to set it, "" to clear it, omitted to leave it unchanged. */
  avatarUrl?: string | null;
}

export function updateProfile(update: ProfileUpdate): Promise<Account> {
  return postJson<Account>("/account/profile", update);
}

export function changePassword(currentPassword: string, newPassword: string): Promise<void> {
  return postJson<void>("/account/password", { currentPassword, newPassword });
}

export function login(email: string, password: string): Promise<{ email: string }> {
  return postJson<{ email: string }>("/auth/login", { email, password });
}

export function logout(): Promise<void> {
  return postJson<void>("/auth/logout");
}
