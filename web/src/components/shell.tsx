// Shared chrome: the Open Grok mark, the centred card the login sits in, and the signed-in frame.
//
// The signed-in frame is a rail, not a tab strip. Three tabs used to hide nine views — Admin alone
// stacked seven cards in one route, 4,085px tall, and the only way to learn which section you were
// in was to scroll back to its heading. The rail names every view at once and says which one is
// open, so a section is reached rather than scrolled past.
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

/** The centred single-card layout the sign-in page uses. */
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

type NavItem = { to: string; label: string };
type NavGroup = { heading: string; items: NavItem[]; adminOnly?: boolean };

/**
 * The rail, grouped by the question a person came to answer rather than by which handler serves it.
 * "Who can use this" is one errand; "what it may spend" is another. Splitting Admin's seven cards
 * along that line is the whole point of the reorganisation — users, invites and domains are the
 * same errand, and were three scroll-lengths apart.
 */
const NAV: NavGroup[] = [
  {
    heading: "Workspace",
    items: [
      { to: "/account", label: "Account" },
      { to: "/coworkers", label: "Coworkers" },
    ],
  },
  {
    heading: "People",
    adminOnly: true,
    items: [
      { to: "/admin/users", label: "Users" },
      { to: "/admin/invites", label: "Invites" },
      { to: "/admin/domains", label: "Domains" },
    ],
  },
  {
    heading: "Model access",
    adminOnly: true,
    items: [
      { to: "/admin/access", label: "Gateway access" },
      { to: "/admin/points", label: "Points" },
      { to: "/admin/templates", label: "Coworker templates" },
    ],
  },
  {
    heading: "Infrastructure",
    adminOnly: true,
    items: [{ to: "/admin/computers", label: "Computers" }],
  },
];

function initial(email: string): string {
  return (email[0] ?? "?").toUpperCase();
}

/** The signed-in frame: brand, the person's email, a sign-out slot, and the navigation rail. */
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
  const groups = NAV.filter((group) => isAdmin || !group.adminOnly);
  return (
    <div className="app">
      <header className="topbar">
        <div className="brand">
          <Logo size={26} />
          <b>Open Grok</b>
        </div>
        <div className="who">
          <span className="avatar placeholder sm" aria-hidden="true">
            {initial(email)}
          </span>
          <span className="muted">{email}</span>
          <button className="ghost sm" onClick={onSignOut}>
            Sign out
          </button>
        </div>
      </header>

      <nav className="rail" aria-label="Console sections">
        {groups.map((group) => (
          <div className="group" key={group.heading}>
            <h6>{group.heading}</h6>
            {group.items.map((item) => (
              <Link
                key={item.to}
                to={item.to}
                className={path.endsWith(item.to) ? "active" : ""}
              >
                {item.label}
              </Link>
            ))}
          </div>
        ))}
      </nav>

      <main className="pane">
        <div className="inner">{children}</div>
      </main>
    </div>
  );
}

/** One view's heading. Every routed section renders exactly one, so the pane always says where it is. */
export function PageHead({ title, children }: { title: string; children?: ReactNode }) {
  return (
    <div className="page-head">
      <h1>{title}</h1>
      {children ? <p>{children}</p> : null}
    </div>
  );
}
