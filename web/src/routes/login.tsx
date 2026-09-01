import { useState } from "react";
import { useNavigate } from "@tanstack/react-router";
import { useMutation, useQueryClient } from "@tanstack/react-query";

import { forgotPassword, login } from "../api/account";
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

  // Forgot-password rides on the same email field: one click, one honest sentence back.
  const forgot = useMutation({ mutationFn: () => forgotPassword(email) });
  const forgotNote = forgot.data
    ? forgot.data.mailer
      ? "If that address has an account here, a reset link is on its way. It works once and expires in an hour."
      : "This server is not set up to send email. Ask your administrator to reset your password."
    : forgot.error
      ? "Could not request a reset."
      : null;

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
      {forgotNote ? <p className="note">{forgotNote}</p> : null}
      <p className="foot">
        <a
          href="/forgot-password"
          onClick={(e) => {
            if (!email.trim()) return; // No address typed: fall through to the server's page.
            e.preventDefault();
            forgot.mutate();
          }}
        >
          Forgot your password?
        </a>
      </p>
    </CenterCard>
  );
}
