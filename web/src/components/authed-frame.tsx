// The gate every signed-in page sits behind. It loads the caller's account once; a 401 (no session,
// or a refresh that could not save it) routes to /login, and everything else renders inside the
// signed-in Chrome. Both the account and admin pages use it, so the redirect rule lives in one place.
import { useEffect, type ReactNode } from "react";
import { useNavigate, useRouter } from "@tanstack/react-router";
import { useQuery, useQueryClient } from "@tanstack/react-query";

import { getAccount, logout, type Account } from "../api/account";
import { ApiError } from "../api/client";
import { Chrome } from "./shell";

export function AuthedFrame({ children }: { children: (account: Account) => ReactNode }) {
  const navigate = useNavigate();
  const router = useRouter();
  const queryClient = useQueryClient();
  const { data, error, isLoading } = useQuery({
    queryKey: ["account"],
    queryFn: getAccount,
    retry: false,
  });

  useEffect(() => {
    if (error instanceof ApiError && error.status === 401) {
      navigate({ to: "/login" });
    }
  }, [error, navigate]);

  const signOut = async () => {
    await logout().catch(() => undefined);
    queryClient.clear();
    router.invalidate();
    navigate({ to: "/login" });
  };

  if (isLoading) return <div className="center muted">Loading…</div>;
  if (!data) return <div className="center muted">Redirecting…</div>;

  return (
    <Chrome email={data.email} onSignOut={signOut}>
      {children(data)}
    </Chrome>
  );
}
