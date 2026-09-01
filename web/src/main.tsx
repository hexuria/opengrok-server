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
import { AdminPage } from "./routes/admin";
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
const adminRoute = createRoute({ getParentRoute: () => rootRoute, path: "/admin", component: AdminPage });

const routeTree = rootRoute.addChildren([indexRoute, loginRoute, accountRoute, coworkersRoute, adminRoute]);
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
