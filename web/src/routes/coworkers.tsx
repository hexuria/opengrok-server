import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import {
  hireCoworker,
  listCoworkers,
  listModels,
  probeModel,
  repinCoworker,
  type Coworker,
  type ProbeResult,
} from "../api/coworkers";
import { ApiError } from "../api/client";
import { AuthedFrame } from "../components/authed-frame";

function errorText(error: unknown, fallback: string): string {
  if (error instanceof ApiError) return error.message;
  return error ? fallback : "";
}

/**
 * A route field: the gateway's own catalogue, plus free text.
 *
 * Free text is not a fallback for a missing picker — it is the point. The catalogue is what this
 * gateway advertises, and a route it does not list may still be servable (and one it does list may
 * not be, which is what Test is for).
 */
function ModelField({
  value,
  onChange,
  models,
  label,
}: {
  value: string;
  onChange: (value: string) => void;
  models: string[];
  label: string;
}) {
  return (
    <>
      <input
        type="text"
        list="model-catalogue"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder="e.g. openai/gpt-5.5"
        aria-label={label}
      />
      <datalist id="model-catalogue">
        {models.map((id) => (
          <option key={id} value={id} />
        ))}
      </datalist>
    </>
  );
}

/** Prove a route before saving it: the gateway answers, or says why it will not. */
function TestButton({ model }: { model: string }) {
  const [result, setResult] = useState<ProbeResult | null>(null);
  const probe = useMutation({
    mutationFn: () => probeModel(model),
    onSuccess: setResult,
    onError: () => setResult({ ok: false, detail: "the probe could not be run" }),
  });
  return (
    <span className="row">
      <button onClick={() => probe.mutate()} disabled={!model.trim() || probe.isPending}>
        {probe.isPending ? "Testing…" : "Test"}
      </button>
      {result ? (
        <span className={result.ok ? "muted" : "error"}>
          {result.ok ? `answered as ${result.served}` : result.detail}
        </span>
      ) : null}
    </span>
  );
}

function CoworkerRow({ coworker, models }: { coworker: Coworker; models: string[] }) {
  const queryClient = useQueryClient();
  const [model, setModel] = useState(coworker.model);
  const repin = useMutation({
    mutationFn: () => repinCoworker(coworker.id, model),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["coworkers"] }),
  });

  return (
    <tr>
      <td>{coworker.name}</td>
      <td>
        <code>{coworker.model}</code>
      </td>
      <td>
        <span className="row">
          <ModelField
            value={model}
            onChange={setModel}
            models={models}
            label={`Route for ${coworker.name}`}
          />
          <button
            onClick={() => repin.mutate()}
            disabled={repin.isPending || model.trim() === coworker.model}
          >
            Repin
          </button>
        </span>
        {repin.error ? (
          <p className="error">{errorText(repin.error, "could not repin")}</p>
        ) : null}
      </td>
      <td>
        <TestButton model={model} />
      </td>
    </tr>
  );
}

/**
 * Coworkers and the route each one thinks through.
 *
 * The model is shown as its own column, never as the description — the roster's habit of using
 * the pin as a subtitle is a blank-agent defence in the desktop client, not a statement about
 * what a person chose.
 */
export function CoworkersPage() {
  const queryClient = useQueryClient();
  const coworkers = useQuery({ queryKey: ["coworkers"], queryFn: listCoworkers, retry: false });
  const catalogue = useQuery({ queryKey: ["models"], queryFn: listModels, retry: false });
  const [name, setName] = useState("");
  const [model, setModel] = useState("");

  const ids = catalogue.data?.models.map((model) => model.id) ?? [];

  const hire = useMutation({
    mutationFn: () => hireCoworker(name, model),
    onSuccess: () => {
      setName("");
      setModel("");
      queryClient.invalidateQueries({ queryKey: ["coworkers"] });
    },
  });

  return (
    <AuthedFrame>
      {() => (
        <div className="stack">
          <section className="card">
            <h2>Hire a coworker</h2>
            <p className="muted">
              A route is a path through the gateway, never a key. Leave it blank to use this
              deployment&rsquo;s default.
            </p>
            <div className="row">
              <input
                type="text"
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="Name"
                aria-label="Name"
              />
              <ModelField value={model} onChange={setModel} models={ids} label="Route" />
              <button onClick={() => hire.mutate()} disabled={!name.trim() || hire.isPending}>
                Hire
              </button>
            </div>
            <TestButton model={model} />
            {hire.error ? <p className="error">{errorText(hire.error, "could not hire")}</p> : null}
            {catalogue.data?.note ? <p className="muted">{catalogue.data.note}</p> : null}
          </section>

          <section className="card">
            <h2>Coworkers</h2>
            {coworkers.isLoading ? (
              <p className="muted">Loading…</p>
            ) : coworkers.data && coworkers.data.length > 0 ? (
              <table>
                <thead>
                  <tr>
                    <th>Name</th>
                    <th>Route</th>
                    <th>Change to</th>
                    <th />
                  </tr>
                </thead>
                <tbody>
                  {coworkers.data.map((coworker) => (
                    <CoworkerRow key={coworker.id} coworker={coworker} models={ids} />
                  ))}
                </tbody>
              </table>
            ) : (
              <p className="muted">No coworkers yet.</p>
            )}
            {coworkers.error ? (
              <p className="error">{errorText(coworkers.error, "could not list coworkers")}</p>
            ) : null}
          </section>
        </div>
      )}
    </AuthedFrame>
  );
}
