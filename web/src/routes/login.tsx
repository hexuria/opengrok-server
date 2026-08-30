import { useState } from "react";
import { useNavigate } from "@tanstack/react-router";
import { useMutation, useQueryClient } from "@tanstack/react-query";

import { login } from "../api/account";
import { ApiError } from "../api/client";
import { CenterCard } from "../components/shell";

export function LoginPage() {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");

  const signIn = useMutation({
    mutationFn: () => login(email, password),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["account"] });
      navigate({ to: "/account" });
    },
  });

  const error = signIn.error instanceof ApiError ? signIn.error.message : signIn.error ? "Could not sign in." : null;

  return (
    <CenterCard subtitle="Sign in to your console.">
      <form
        onSubmit={(e) => {
          e.preventDefault();
          signIn.mutate();
        }}
      >
        {error ? <p className="err">{error}</p> : null}
        <label htmlFor="email">Email</label>
        <input
          id="email"
          type="email"
          autoComplete="username"
          autoFocus
          required
          value={email}
          onChange={(e) => setEmail(e.target.value)}
        />
        <label htmlFor="password">Password</label>
        <input
          id="password"
          type="password"
          autoComplete="current-password"
          required
          value={password}
          onChange={(e) => setPassword(e.target.value)}
        />
        <button className="wide" type="submit" disabled={signIn.isPending}>
          {signIn.isPending ? "Signing in…" : "Sign in →"}
        </button>
      </form>
    </CenterCard>
  );
}
