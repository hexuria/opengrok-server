import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import {
  getSpend,
  hireCoworker,
  listCoworkers,
  listTemplates,
  listMcpCalls,
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

/**
 * What this coworker's bot keys have been used for: the door's audit, newest first. Fetched only
 * when opened — a coworker that has never been called over MCP costs nothing here.
 */
function McpCalls({ coworker }: { coworker: Coworker }) {
  const calls = useQuery({
    queryKey: ["mcp-calls", coworker.id],
    queryFn: () => listMcpCalls(coworker.id),
    retry: false,
  });
  if (calls.isLoading) return <p className="muted">Loading…</p>;
  if (calls.error) return <p className="error">{errorText(calls.error, "could not load calls")}</p>;
  if (!calls.data || calls.data.length === 0) {
    return <p className="muted">No calls through the MCP door yet.</p>;
  }
  return (
    <table>
      <thead>
        <tr>
          <th>When</th>
          <th>Tool</th>
          <th>Outcome</th>
          <th>Arguments</th>
          <th>Request</th>
        </tr>
      </thead>
      <tbody>
        {calls.data.map((call, index) => (
          <tr key={`${call.callId}-${index}`}>
            <td>{new Date(call.atMs).toLocaleString()}</td>
            <td>
              <code>{call.tool}</code>
            </td>
            <td>{call.outcome}</td>
            <td>
              <code>{JSON.stringify(call.arguments)}</code>
            </td>
            <td>
              <code>{call.requestId}</code>
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

/** "14:32 UTC" for a rolling window, "1 Oct" for the month. */
function whenText(window: string, iso: string | null | undefined): string | null {
  if (!iso) return null;
  const at = new Date(iso);
  if (Number.isNaN(at.getTime())) return null;
  if (window === "month") {
    return `resets ${at.toLocaleDateString(undefined, { day: "numeric", month: "short" })}`;
  }
  return `frees up ${at.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" })}`;
}

const WINDOW_LABEL: Record<string, string> = { "5h": "5 h", "7d": "7 d", month: "month" };

/**
 * The coworker's three meters. Limits are the admin's to write (admin page); a member sees
 * used / limit and when each window next frees up, so the window in the way is visible before
 * it is hit. A coworker with no key of its own says why it is not metered.
 */
function SpendCell({ coworker }: { coworker: Coworker }) {
  const spend = useQuery({
    queryKey: ["spend", coworker.id],
    queryFn: () => getSpend(coworker.id),
    retry: false,
  });
  if (spend.isLoading) return <span className="muted">…</span>;
  if (spend.error) return <span className="error">{errorText(spend.error, "could not load spend")}</span>;
  const data = spend.data;
  if (!data) return null;
  const limited = data.windows.some((w) => w.limitUsd);
  if (!data.metered) {
    return <span className="muted">Not metered: {data.note ?? "no key of its own"}</span>;
  }
  return (
    <span className="stack">
      {data.windows.map((w) => (
        <span key={w.window}>
          <strong>{WINDOW_LABEL[w.window] ?? w.window}</strong>{" "}
          ${w.usedUsd ?? "?"}
          {w.limitUsd ? ` / $${w.limitUsd}` : limited ? " / no limit" : ""}
          {w.limitUsd && whenText(w.window, w.freesAt) ? (
            <span className="muted"> ({whenText(w.window, w.freesAt)})</span>
          ) : null}
        </span>
      ))}
      {!limited ? <span className="muted">No limits set; the org admin can set them.</span> : null}
      {data.note ? <span className="muted">{data.note}</span> : null}
    </span>
  );
}

function CoworkerRow({ coworker, models }: { coworker: Coworker; models: string[] }) {
  const queryClient = useQueryClient();
  const [model, setModel] = useState(coworker.model);
  const [showCalls, setShowCalls] = useState(false);
  const repin = useMutation({
    mutationFn: () => repinCoworker(coworker.id, model),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["coworkers"] }),
  });

  return (
    <>
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
        <span className="row">
          <TestButton model={model} />
          <button onClick={() => setShowCalls((open) => !open)}>
            {showCalls ? "Hide door calls" : "Door calls"}
          </button>
        </span>
      </td>
      <td>
        <SpendCell coworker={coworker} />
      </td>
    </tr>
    {showCalls ? (
      <tr>
        <td colSpan={5}>
          <McpCalls coworker={coworker} />
        </td>
      </tr>
    ) : null}
    </>
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
  const templates = useQuery({ queryKey: ["templates"], queryFn: listTemplates, retry: false });
  const [name, setName] = useState("");
  const [model, setModel] = useState("");
  const [templateId, setTemplateId] = useState("");

  const ids = catalogue.data?.models.map((model) => model.id) ?? [];

  const hire = useMutation({
    mutationFn: () => hireCoworker(name, model, templateId),
    onSuccess: () => {
      setName("");
      setModel("");
      setTemplateId("");
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
              {templates.data && templates.data.templates.length > 0 ? (
                <select
                  value={templateId}
                  onChange={(e) => setTemplateId(e.target.value)}
                  aria-label="Template"
                >
                  <option value="">No template</option>
                  {templates.data.templates.map((t) => (
                    <option key={t.id} value={t.id}>
                      {t.name}
                      {t.model ? ` · ${t.model}` : ""}
                    </option>
                  ))}
                </select>
              ) : null}
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
                    <th>Spend</th>
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
