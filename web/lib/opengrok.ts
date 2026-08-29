/**
 * The server's address, and the session rules the desktop client already enforces.
 *
 * WHY THE TOKENS NEVER REACH THE BROWSER. Every call to OpenGrok goes through a route handler in
 * `app/api/auth/*`, which holds the access and refresh tokens in httpOnly cookies. A refresh token
 * readable by scripts on the page is a refresh token one XSS away from being someone else's
 * session, and unlike an access token it does not expire on its own.
 *
 * The session predicates below are transcribed from the client, not invented — the web app should
 * decide "am I signed in" exactly as the desktop app does, so a divergence is a bug in one place
 * rather than a difference of opinion between two clients.
 */

export const API_BASE = process.env.OPENGROK_API_URL ?? "http://127.0.0.1:1337";

/** Cookie names. Prefixed so they cannot collide with anything the app adds later. */
export const ACCESS_COOKIE = "og_access";
export const REFRESH_COOKIE = "og_refresh";

/**
 * What the access token carries. The desktop client reads exactly these three
 * (`cursor-auth.ts:67-73`) to build its `logged-in` status; `plan` and `sid` are ours.
 */
export interface AccessClaims {
  sub: string;
  email: string;
  /** SECONDS since the epoch, not milliseconds — `cursor-auth.ts:71` multiplies by 1000. */
  exp: number;
  sid: string;
  plan: string;
}

/**
 * Decode without verifying.
 *
 * Verification is the server's job and it holds the only key; a client that "verified" would be
 * checking a signature against a secret it must not have. This is what the desktop client does too
 * (`cursor-token.ts:9-22`) — read the claims, trust the server for the rest.
 */
export function parseJwtPayload(token: string): AccessClaims | null {
  const payload = token.split(".")[1];
  if (!payload) return null;
  try {
    const json = Buffer.from(payload, "base64url").toString("utf8");
    const parsed: unknown = JSON.parse(json);
    if (typeof parsed !== "object" || parsed === null) return null;
    const record = parsed as Record<string, unknown>;
    if (typeof record.sub !== "string" || typeof record.exp !== "number") return null;
    return record as unknown as AccessClaims;
  } catch {
    return null;
  }
}

/** Five minutes, matching `TOKEN_REFRESH_LEEWAY_MS` (`cursor-token.ts:6`). */
export const TOKEN_REFRESH_LEEWAY_MS = 5 * 60 * 1000;

/** `isTokenExpiringSoon` (`cursor-token.ts:27-30`): no exp counts as expiring. */
export function isTokenExpiringSoon(token: string, now = Date.now()): boolean {
  const exp = parseJwtPayload(token)?.exp;
  return exp == null || exp * 1000 - now < TOKEN_REFRESH_LEEWAY_MS;
}

/**
 * `cursorSessionPresent` (`cursor-session-policy.ts:24-29`): a session exists only when BOTH
 * tokens are held. Holding one is not a degraded session, it is no session.
 */
export function sessionPresent(tokens: {
  accessToken?: string | null;
  refreshToken?: string | null;
}): boolean {
  return (
    typeof tokens.accessToken === "string" &&
    tokens.accessToken.length > 0 &&
    typeof tokens.refreshToken === "string" &&
    tokens.refreshToken.length > 0
  );
}

/** How the cookies are set everywhere, so no route handler can quietly relax one. */
export const cookieOptions = {
  httpOnly: true,
  sameSite: "lax",
  path: "/",
  // Off in development because the dev server is plain http; a hardcoded `true` would silently
  // drop every cookie and look like a broken login.
  secure: process.env.NODE_ENV === "production",
} as const;
