/**
 * Rotate the session.
 *
 * This request/response pair is snake_case (`access_token`/`refresh_token`) while sign-in is
 * camelCase. Both are transcribed from the client — `parseOAuthTokenBody` (`cursor-auth.ts:160`)
 * rejects anything else — so the inconsistency is the contract, not an oversight here.
 *
 * A 401 means the session is genuinely gone: clear the cookies. Anything else (the server is
 * restarting, the database blinked) leaves them alone, because signing someone out during an
 * outage loses their work for a reason that will have fixed itself in a second.
 */
import { cookies } from "next/headers";
import { NextResponse } from "next/server";

import { ACCESS_COOKIE, API_BASE, REFRESH_COOKIE, cookieOptions } from "@/lib/opengrok";

export async function POST() {
  const jar = await cookies();
  const refreshToken = jar.get(REFRESH_COOKIE)?.value;
  if (!refreshToken) {
    return NextResponse.json({ error: "no session" }, { status: 401 });
  }

  let upstream: Response;
  try {
    upstream = await fetch(new URL("/oauth/token", API_BASE), {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ grant_type: "refresh_token", refresh_token: refreshToken }),
      cache: "no-store",
    });
  } catch {
    return NextResponse.json({ error: "opengrok is unreachable" }, { status: 503 });
  }

  if (upstream.status === 401) {
    const response = NextResponse.json({ error: "session expired" }, { status: 401 });
    response.cookies.delete(ACCESS_COOKIE);
    response.cookies.delete(REFRESH_COOKIE);
    return response;
  }
  if (!upstream.ok) {
    return NextResponse.json({ error: "refresh unavailable" }, { status: 503 });
  }

  const tokens = (await upstream.json()) as { access_token?: string; refresh_token?: string };
  if (!tokens.access_token || !tokens.refresh_token) {
    return NextResponse.json({ error: "refresh reply was missing a token" }, { status: 502 });
  }

  const response = NextResponse.json({ ok: true });
  response.cookies.set(ACCESS_COOKIE, tokens.access_token, cookieOptions);
  response.cookies.set(REFRESH_COOKIE, tokens.refresh_token, cookieOptions);
  return response;
}
