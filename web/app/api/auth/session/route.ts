/**
 * Who is signed in, as the page should render it.
 *
 * Returns claims, never tokens: the browser has no use for the token itself and every copy of it
 * outside the cookie is a place it can leak from.
 */
import { cookies } from "next/headers";
import { NextResponse } from "next/server";

import {
  ACCESS_COOKIE,
  REFRESH_COOKIE,
  isTokenExpiringSoon,
  parseJwtPayload,
  sessionPresent,
} from "@/lib/opengrok";

export async function GET() {
  const jar = await cookies();
  const accessToken = jar.get(ACCESS_COOKIE)?.value;
  const refreshToken = jar.get(REFRESH_COOKIE)?.value;

  if (!sessionPresent({ accessToken, refreshToken })) {
    return NextResponse.json({ status: "logged-out" });
  }

  const claims = accessToken ? parseJwtPayload(accessToken) : null;
  if (!claims) {
    // A token we cannot read is not a session we can honour — the desktop client reaches the same
    // conclusion by getting `null` out of `parseJwtPayload`.
    return NextResponse.json({ status: "logged-out", reason: "unreadable token" });
  }

  return NextResponse.json({
    status: "logged-in",
    authId: claims.sub,
    email: claims.email,
    plan: claims.plan,
    sessionId: claims.sid,
    expiresAt: claims.exp * 1000,
    expiringSoon: accessToken ? isTokenExpiringSoon(accessToken) : true,
  });
}
