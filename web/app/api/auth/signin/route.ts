/**
 * Sign in against OpenGrok's dev-login endpoint.
 *
 * The reply is camelCase (`accessToken`/`refreshToken`) — that is the shape the desktop client
 * reads at `cursor-auth.ts:315-316`, and the server answers it for both clients rather than
 * growing a second dialect for the web.
 */
import { NextResponse } from "next/server";

import { ACCESS_COOKIE, API_BASE, REFRESH_COOKIE, cookieOptions } from "@/lib/opengrok";

export async function POST(request: Request) {
  const body = (await request.json().catch(() => ({}))) as {
    email?: string;
    plan?: string;
    trial?: boolean;
  };

  const url = new URL("/auth/cursor_dev_session_token", API_BASE);
  url.searchParams.set("plan", body.plan ?? "pro");
  // Only ever sent when true, matching how the client builds it (`cursor-auth.ts:313`).
  if (body.trial) url.searchParams.set("trial", "true");
  if (body.email) url.searchParams.set("email", body.email);

  let upstream: Response;
  try {
    upstream = await fetch(url, { headers: { accept: "application/json" }, cache: "no-store" });
  } catch {
    // The server being down is not the person's fault and not a sign-out.
    return NextResponse.json({ error: "opengrok is unreachable" }, { status: 503 });
  }

  if (!upstream.ok) {
    return NextResponse.json(
      { error: `sign-in failed: ${upstream.status}` },
      { status: upstream.status },
    );
  }

  const tokens = (await upstream.json()) as { accessToken?: string; refreshToken?: string };
  if (!tokens.accessToken || !tokens.refreshToken) {
    return NextResponse.json({ error: "sign-in reply was missing a token" }, { status: 502 });
  }

  const response = NextResponse.json({ ok: true });
  response.cookies.set(ACCESS_COOKIE, tokens.accessToken, cookieOptions);
  response.cookies.set(REFRESH_COOKIE, tokens.refreshToken, cookieOptions);
  return response;
}
