/**
 * The whole web client, for now: sign in, see the session, rotate it, sign out.
 *
 * The session is read on the SERVER and handed down. That is not ceremony — the tokens live in
 * httpOnly cookies precisely so the browser cannot read them, so the only place that can answer
 * "who is signed in" without shipping a token to the page is here. After a mutation the client
 * component calls `router.refresh()` and this runs again.
 *
 * Deliberately one page. It exists to prove slice 1 end to end from a real browser and to be the
 * surface every later slice is verified against — the roster, the transcript and the stream all
 * land here.
 */
import { cookies } from "next/headers";

import SessionPanel, { type Session } from "./session-panel";
import {
  ACCESS_COOKIE,
  REFRESH_COOKIE,
  isTokenExpiringSoon,
  parseJwtPayload,
  sessionPresent,
} from "@/lib/opengrok";

async function readSession(): Promise<Session> {
  const jar = await cookies();
  const accessToken = jar.get(ACCESS_COOKIE)?.value;
  const refreshToken = jar.get(REFRESH_COOKIE)?.value;

  if (!sessionPresent({ accessToken, refreshToken })) return { status: "logged-out" };

  const claims = accessToken ? parseJwtPayload(accessToken) : null;
  // A token we cannot read is not a session we can honour — the desktop client reaches the same
  // conclusion by getting `null` out of `parseJwtPayload`.
  if (!claims) return { status: "logged-out", reason: "unreadable token" };

  return {
    status: "logged-in",
    authId: claims.sub,
    email: claims.email,
    plan: claims.plan,
    sessionId: claims.sid,
    expiresAt: claims.exp * 1000,
    expiringSoon: accessToken ? isTokenExpiringSoon(accessToken) : true,
  };
}

export default async function Home() {
  return <SessionPanel session={await readSession()} />;
}
