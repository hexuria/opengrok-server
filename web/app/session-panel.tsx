"use client";

import { useRouter } from "next/navigation";
import { useState } from "react";

export interface Session {
  status: "logged-in" | "logged-out";
  authId?: string;
  email?: string;
  plan?: string;
  sessionId?: string;
  expiresAt?: number;
  expiringSoon?: boolean;
  reason?: string;
}

const PLANS = ["free", "pro", "pro_plus", "enterprise", "ultra"] as const;

export default function SessionPanel({ session }: { session: Session }) {
  const router = useRouter();
  const [email, setEmail] = useState("you@example.com");
  const [plan, setPlan] = useState<string>("pro");
  const [trial, setTrial] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function call(path: string, body?: unknown) {
    setBusy(true);
    setError(null);
    try {
      const response = await fetch(path, {
        method: "POST",
        headers: body ? { "content-type": "application/json" } : undefined,
        body: body ? JSON.stringify(body) : undefined,
      });
      if (!response.ok) {
        const detail = (await response.json().catch(() => ({}))) as { error?: string };
        setError(detail.error ?? `${path} failed with ${response.status}`);
      }
      // Re-runs the server component, which re-reads the cookies.
      router.refresh();
    } catch {
      setError("could not reach the web server");
    } finally {
      setBusy(false);
    }
  }

  const loggedIn = session.status === "logged-in";

  return (
    <main className="mx-auto flex min-h-screen max-w-2xl flex-col gap-8 p-10 font-mono text-sm">
      <header>
        <h1 className="text-xl font-semibold">OpenGrok</h1>
        <p className="text-neutral-500">
          slice 1 — the client signs in against us, not against Cursor
        </p>
      </header>

      <section className="rounded border border-neutral-300 p-5 dark:border-neutral-700">
        <h2 className="mb-3 font-semibold">session</h2>
        {loggedIn ? (
          <dl className="grid grid-cols-[9rem_1fr] gap-y-1">
            <dt className="text-neutral-500">status</dt>
            <dd className="text-green-600 dark:text-green-400">logged-in</dd>
            <dt className="text-neutral-500">authId</dt>
            <dd className="break-all">{session.authId}</dd>
            <dt className="text-neutral-500">email</dt>
            <dd>{session.email}</dd>
            <dt className="text-neutral-500">plan</dt>
            <dd>{session.plan}</dd>
            <dt className="text-neutral-500">sessionId</dt>
            <dd className="break-all">{session.sessionId}</dd>
            <dt className="text-neutral-500">expires</dt>
            <dd>
              {session.expiresAt ? new Date(session.expiresAt).toLocaleTimeString() : "—"}
              {session.expiringSoon ? " (refresh due)" : ""}
            </dd>
          </dl>
        ) : (
          <p className="text-neutral-500">
            logged-out{session.reason ? ` — ${session.reason}` : ""}
          </p>
        )}
      </section>

      {!loggedIn && (
        <section className="rounded border border-neutral-300 p-5 dark:border-neutral-700">
          <h2 className="mb-3 font-semibold">sign in</h2>
          <div className="flex flex-col gap-3">
            <label className="flex items-center gap-3">
              <span className="w-16 text-neutral-500">email</span>
              <input
                className="flex-1 rounded border border-neutral-300 bg-transparent px-2 py-1 dark:border-neutral-700"
                value={email}
                onChange={(event) => setEmail(event.target.value)}
              />
            </label>
            <div className="flex items-center gap-3">
              <span className="w-16 text-neutral-500">plan</span>
              <select
                className="rounded border border-neutral-300 bg-transparent px-2 py-1 dark:border-neutral-700"
                value={plan}
                onChange={(event) => setPlan(event.target.value)}
              >
                {PLANS.map((option) => (
                  <option key={option} value={option}>
                    {option}
                  </option>
                ))}
              </select>
              <label className="flex items-center gap-2">
                <input
                  type="checkbox"
                  checked={trial}
                  onChange={(event) => setTrial(event.target.checked)}
                />
                <span className="text-neutral-500">trial</span>
              </label>
            </div>
            <button
              className="self-start rounded bg-neutral-900 px-4 py-1.5 text-white disabled:opacity-40 dark:bg-white dark:text-neutral-900"
              disabled={busy}
              onClick={() => call("/api/auth/signin", { email, plan, trial })}
            >
              sign in
            </button>
          </div>
        </section>
      )}

      {loggedIn && (
        <div className="flex gap-3">
          <button
            className="rounded border border-neutral-300 px-4 py-1.5 disabled:opacity-40 dark:border-neutral-700"
            disabled={busy}
            onClick={() => call("/api/auth/refresh")}
          >
            refresh session
          </button>
          <button
            className="rounded border border-neutral-300 px-4 py-1.5 disabled:opacity-40 dark:border-neutral-700"
            disabled={busy}
            onClick={() => call("/api/auth/signout")}
          >
            sign out
          </button>
        </div>
      )}

      {error && <p className="text-red-600 dark:text-red-400">{error}</p>}
    </main>
  );
}
