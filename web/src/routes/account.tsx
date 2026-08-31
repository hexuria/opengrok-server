import { useRef, useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";

import { changePassword, updateProfile, type Account, type ProfileUpdate } from "../api/account";
import { ApiError } from "../api/client";
import { checkAvatar, fileToDataUrl } from "../lib/avatar";
import { AuthedFrame } from "../components/authed-frame";

function errorText(error: unknown, fallback: string): string {
  if (error instanceof ApiError) return error.message;
  return error ? fallback : "";
}

function ProfileCard({ account }: { account: Account }) {
  const queryClient = useQueryClient();
  const fileInput = useRef<HTMLInputElement>(null);
  const [firstName, setFirstName] = useState(account.firstName);
  const [lastName, setLastName] = useState(account.lastName);
  // undefined = unchanged, "" = clear, string = a new data URL.
  const [pendingAvatar, setPendingAvatar] = useState<string | undefined>(undefined);
  const [avatarError, setAvatarError] = useState<string | null>(null);

  const preview = pendingAvatar !== undefined ? pendingAvatar : account.avatarUrl;

  const save = useMutation({
    mutationFn: () => {
      const update: ProfileUpdate = { firstName, lastName };
      if (pendingAvatar !== undefined) update.avatarUrl = pendingAvatar;
      return updateProfile(update);
    },
    onSuccess: async () => {
      setPendingAvatar(undefined);
      await queryClient.invalidateQueries({ queryKey: ["account"] });
    },
  });

  async function onPick(file: File | undefined) {
    setAvatarError(null);
    if (!file) return;
    try {
      const dataUrl = await fileToDataUrl(file);
      const check = checkAvatar(dataUrl);
      if (!check.ok) {
        setAvatarError(check.reason);
        return;
      }
      setPendingAvatar(dataUrl);
    } catch {
      setAvatarError("Could not read that file.");
    }
  }

  return (
    <section className="card">
      <h2>Profile</h2>
      <div className="row" style={{ alignItems: "flex-start", gap: "1.25rem" }}>
        {preview ? (
          <img className="avatar" src={preview} alt="Your avatar" />
        ) : (
          <div className="avatar placeholder" aria-hidden="true">
            {(firstName[0] ?? account.email[0] ?? "?").toUpperCase()}
          </div>
        )}
        <div className="row">
          <input
            ref={fileInput}
            type="file"
            accept="image/*"
            style={{ display: "none" }}
            onChange={(e) => void onPick(e.target.files?.[0])}
          />
          <button className="ghost" type="button" onClick={() => fileInput.current?.click()}>
            Choose image
          </button>
          {preview ? (
            <button
              className="danger"
              type="button"
              onClick={() => {
                setPendingAvatar("");
                setAvatarError(null);
              }}
            >
              Remove
            </button>
          ) : null}
        </div>
      </div>
      {avatarError ? <p className="err">{avatarError}</p> : null}

      <form
        onSubmit={(e) => {
          e.preventDefault();
          save.mutate();
        }}
      >
        <label htmlFor="first">First name</label>
        <input id="first" value={firstName} onChange={(e) => setFirstName(e.target.value)} />
        <label htmlFor="last">Last name</label>
        <input id="last" value={lastName} onChange={(e) => setLastName(e.target.value)} />
        <label htmlFor="email">Email</label>
        <input id="email" value={account.email} readOnly title="Your email cannot be changed." />
        <p className="muted" style={{ fontSize: "0.8rem", marginTop: "0.3rem" }}>
          Email is fixed — it is the identity your organization and invite were bound to.
        </p>
        {save.isError ? <p className="err">{errorText(save.error, "Could not save.")}</p> : null}
        {save.isSuccess && !save.isPending ? <p className="note">Saved.</p> : null}
        <button className="wide" type="submit" disabled={save.isPending}>
          {save.isPending ? "Saving…" : "Save profile"}
        </button>
      </form>
    </section>
  );
}

function PasswordCard() {
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");

  const change = useMutation({
    mutationFn: () => changePassword(current, next),
    onSuccess: () => {
      setCurrent("");
      setNext("");
    },
  });

  return (
    <section className="card">
      <h2>Password</h2>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          change.mutate();
        }}
      >
        <label htmlFor="current">Current password</label>
        <input
          id="current"
          type="password"
          autoComplete="current-password"
          required
          value={current}
          onChange={(e) => setCurrent(e.target.value)}
        />
        <label htmlFor="new">New password</label>
        <input
          id="new"
          type="password"
          autoComplete="new-password"
          minLength={8}
          required
          value={next}
          onChange={(e) => setNext(e.target.value)}
        />
        {change.isError ? <p className="err">{errorText(change.error, "Could not change password.")}</p> : null}
        {change.isSuccess ? <p className="note">Password changed.</p> : null}
        <button className="wide" type="submit" disabled={change.isPending}>
          {change.isPending ? "Changing…" : "Change password"}
        </button>
      </form>
    </section>
  );
}

export function AccountPage() {
  return (
    <AuthedFrame>
      {(account) => (
        <div className="stack">
          <ProfileCard account={account} />
          <PasswordCard />
        </div>
      )}
    </AuthedFrame>
  );
}
