import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import {
  getLimit,
  setLimit,
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

function commas(points: number | null | undefined): string {
  if (points == null) return "—";
  return points.toLocaleString("en-US");
}

function pointsFromInput(raw: string): number | null {
  const text = raw.replace(/[,\s]/g, "");
  if (!text) return null;
  return Number(text);
}

/** "frees up 14:32" / "resets 1 Oct" for the instants the limit read carries. */
function whenText(kind: "day" | "month", iso: string | null | undefined): string | null {
  if (!iso) return null;
  const at = new Date(iso);
  if (Number.isNaN(at.getTime())) return null;
  if (kind === "month") {
    return `resets ${at.toLocaleDateString(undefined, { day: "numeric", month: "short" })}`;
  }
  return `frees up ${at.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" })}`;
}

/**
 * The coworker's points: what it used this month and today, the cap and brake its owner set
 * (editable here — the owner's, at most the pool), and the pool it draws on. A coworker with no
 * key of its own says why it is not metered.
 */
function PointsCell({ coworker }: { coworker: Coworker }) {
  const queryClient = useQueryClient();
  const limit = useQuery({
    queryKey: ["limit", coworker.id],
    queryFn: () => getLimit(coworker.id),
    retry: false,
  });
  const [cap, setCap] = useState<string | null>(null);
  const [dayCap, setDayCap] = useState<string | null>(null);
  const save = useMutation({
    mutationFn: () =>
      setLimit(coworker.id, {
        ...(cap == null ? {} : { cap: pointsFromInput(cap) }),
        ...(dayCap == null ? {} : { dayCap: pointsFromInput(dayCap) }),
      }),
    onSuccess: () => {
      setCap(null);
      setDayCap(null);
      queryClient.invalidateQueries({ queryKey: ["limit", coworker.id] });
      queryClient.invalidateQueries({ queryKey: ["admin", "points"] });
    },
  });
  if (limit.isLoading) return <span className="muted">…</span>;
  if (limit.error) return <span className="error">{errorText(limit.error, "could not load points")}</span>;
  const data = limit.data;
  if (!data) return null;
  const capShown = cap ?? (data.cap == null ? "" : String(data.cap));
  const dayShown = dayCap ?? (data.dayCap == null ? "" : String(data.dayCap));
  return (
    <span className="stack">
      {!data.metered ? <span className="muted">Not metered: {data.note ?? "no key of its own"}</span> : null}
      <span>
        <strong>month</strong> {commas(data.usedPoints)}
        {data.effectiveCap != null ? ` / ${commas(data.effectiveCap)}` : ""}
        {whenText("month", data.pool.resetsAt) ? <span className="muted"> ({whenText("month", data.pool.resetsAt)})</span> : null}
      </span>
      <span>
        <strong>today</strong> {commas(data.usedToday)}
        {data.dayCap != null ? ` / ${commas(data.dayCap)}` : ""}
        {data.dayCap != null && whenText("day", data.dayFreesAt) ? (
          <span className="muted"> ({whenText("day", data.dayFreesAt)})</span>
        ) : null}
      </span>
      <span className="muted">
        {data.pool.max == null
          ? "No pool: your admin has not set one."
          : `Your pool: ${commas(data.pool.used)} of ${commas(data.pool.max)} used this month.`}
      </span>
      <span className="row">
        <input
          type="text"
          inputMode="numeric"
          value={capShown}
          onChange={(e) => setCap(e.target.value)}
          placeholder="cap / month"
          aria-label={`Monthly cap for ${coworker.name}`}
          style={{ width: "8rem" }}
        />
        <input
          type="text"
          inputMode="numeric"
          value={dayShown}
          onChange={(e) => setDayCap(e.target.value)}
          placeholder="brake / day"
          aria-label={`Daily brake for ${coworker.name}`}
          style={{ width: "8rem" }}
        />
        <button onClick={() => save.mutate()} disabled={save.isPending || (cap == null && dayCap == null)}>
          Save
        </button>
        {save.error ? <span className="error">{errorText(save.error, "could not save")}</span> : null}
      </span>
      {data.note && data.metered ? <span className="muted">{data.note}</span> : null}
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
        <PointsCell coworker={coworker} />
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

  const [hireNote, setHireNote] = useState<string | null>(null);
  const hire = useMutation({
    mutationFn: () => hireCoworker(name, model, templateId),
    onSuccess: (hired) => {
      setHireNote(hired.templateNote ?? null);
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
            {hireNote ? <p className="error">{hireNote}</p> : null}
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
                    <th>Points</th>
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
