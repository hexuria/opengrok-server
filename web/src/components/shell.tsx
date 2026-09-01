// Shared chrome: the Open Grok mark, the centered card the login sits in, and the signed-in frame
// with its tabs. Kept deliberately small and plain — the brand is carried by styles.css, itself a
// transcription of the server's auth/pages.rs shell.
import type { ReactNode } from "react";
import { Link, useRouterState } from "@tanstack/react-router";

export function Logo({ size = 40 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 100 100" fill="none" aria-hidden="true">
      <rect width="100" height="100" rx="30" fill="#fff" />
      <circle cx="38" cy="46" r="7" fill="#111" />
      <circle cx="66" cy="46" r="7" fill="#111" />
      <path d="M36 64 Q50 74 64 64" stroke="#111" strokeWidth="6" strokeLinecap="round" fill="none" />
    </svg>
  );
}

/** The centered single-card layout the sign-in page uses. */
export function CenterCard({ subtitle, children }: { subtitle?: string; children: ReactNode }) {
  return (
    <div className="center">
      <main className="card narrow">
        <div className="brand">
          <Logo />
          <b>Open Grok</b>
        </div>
        {subtitle ? <p className="sub">{subtitle}</p> : null}
        {children}
      </main>
    </div>
  );
}

/** The signed-in frame: brand, the person's email, a sign-out slot, and the Account/Admin tabs. */
export function Chrome({
  email,
  isAdmin,
  onSignOut,
  children,
}: {
  email: string;
  isAdmin: boolean;
  onSignOut: () => void;
  children: ReactNode;
}) {
  const path = useRouterState({ select: (s) => s.location.pathname });
  return (
    <div className="wrap">
      <div className="spread" style={{ marginBottom: "1.5rem" }}>
        <div className="brand">
          <Logo size={32} />
          <b>Open Grok</b>
        </div>
        <div className="row">
          <span className="muted">{email}</span>
          <button className="ghost" onClick={onSignOut}>
            Sign out
          </button>
        </div>
      </div>
      <nav className="tabs">
        <Link to="/account" className={path.endsWith("/account") ? "active" : ""}>
          Account
        </Link>
        <Link to="/coworkers" className={path.endsWith("/coworkers") ? "active" : ""}>
          Coworkers
        </Link>
        {isAdmin ? (
          <Link to="/admin" className={path.endsWith("/admin") ? "active" : ""}>
            Admin
          </Link>
        ) : null}
      </nav>
      {children}
    </div>
  );
}
