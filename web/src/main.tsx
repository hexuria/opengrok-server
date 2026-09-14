import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  Outlet,
  RouterProvider,
  createRootRoute,
  createRoute,
  createRouter,
  redirect,
} from "@tanstack/react-router";

import "./styles.css";
import { LoginPage } from "./routes/login";
import { AccountPage } from "./routes/account";
import {
  AdminComputersPage,
  AdminDomainsPage,
  AdminGatewayPage,
  AdminInvitesPage,
  AdminPointsPage,
  AdminTemplatesPage,
  AdminUsersPage,
} from "./routes/admin";
import { CoworkersPage } from "./routes/coworkers";

const rootRoute = createRootRoute({ component: () => <Outlet /> });
const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  beforeLoad: () => {
    throw redirect({ to: "/account" });
  },
});
const loginRoute = createRoute({ getParentRoute: () => rootRoute, path: "/login", component: LoginPage });
const accountRoute = createRoute({ getParentRoute: () => rootRoute, path: "/account", component: AccountPage });
const coworkersRoute = createRoute({ getParentRoute: () => rootRoute, path: "/coworkers", component: CoworkersPage });
// Admin is seven sections, one route each — it used to be seven cards in one route, which meant a
// link could only ever point at the top of the pile. `/admin` keeps working and lands on Users.
const adminIndexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/admin",
  beforeLoad: () => {
    throw redirect({ to: "/admin/users" });
  },
});
const ADMIN_SECTIONS = [
  ["/admin/users", AdminUsersPage],
  ["/admin/invites", AdminInvitesPage],
  ["/admin/domains", AdminDomainsPage],
  ["/admin/access", AdminGatewayPage],
  ["/admin/points", AdminPointsPage],
  ["/admin/templates", AdminTemplatesPage],
  ["/admin/computers", AdminComputersPage],
] as const;
const adminRoutes = ADMIN_SECTIONS.map(([path, component]) =>
  createRoute({ getParentRoute: () => rootRoute, path, component }),
);

const routeTree = rootRoute.addChildren([
  indexRoute,
  loginRoute,
  accountRoute,
  coworkersRoute,
  adminIndexRoute,
  ...adminRoutes,
]);
// Served under /console by the Rust server, so the router lives under that basepath.
const router = createRouter({ routeTree, basepath: "/console" });

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}

const queryClient = new QueryClient({
  defaultOptions: { queries: { staleTime: 5_000, refetchOnWindowFocus: false } },
});

const root = document.getElementById("root");
if (root) {
  createRoot(root).render(
    <StrictMode>
      <QueryClientProvider client={queryClient}>
        <RouterProvider router={router} />
      </QueryClientProvider>
    </StrictMode>,
  );
}
