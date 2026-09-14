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
import { PageHead } from "../components/shell";

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

/**
 * Prove a route before saving it: the gateway answers, or says why it will not.
 *
 * The verdict is a chip, not loose text. It used to render as bare body copy beside the button —
 * `.error` was never defined in the stylesheet — so a successful probe read as a stray fragment
 * ("answered as grok-4.6") floating next to the control that produced it.
 */
function TestButton({ model }: { model: string }) {
  const [result, setResult] = useState<ProbeResult | null>(null);
  const probe = useMutation({
    mutationFn: () => probeModel(model),
    onSuccess: setResult,
    onError: () => setResult({ ok: false, detail: "the probe could not be run" }),
  });
  return (
    <>
      <button
        className="ghost sm"
        onClick={() => probe.mutate()}
        disabled={!model.trim() || probe.isPending}
      >
        {probe.isPending ? "Testing…" : "Test"}
      </button>
      {result ? (
        <span className={result.ok ? "ok" : "error"}>
          {result.ok ? `answered as ${result.served}` : result.detail}
        </span>
      ) : null}
    </>
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
  if (calls.isLoading) return <p className="empty">Loading…</p>;
  if (calls.error) return <p className="error">{errorText(calls.error, "could not load calls")}</p>;
  if (!calls.data || calls.data.length === 0) {
    return <p className="empty">No calls through the MCP door yet.</p>;
  }
  return (
    <div className="table-wrap">
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
              <td className="muted small">{new Date(call.atMs).toLocaleString()}</td>
              <td>
                <span className="chip">{call.tool}</span>
              </td>
              <td>{call.outcome}</td>
              <td className="mono small">{JSON.stringify(call.arguments)}</td>
              <td className="mono small muted">{call.requestId}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
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

/** The one shared read behind both the summary line and the editor, so opening details refetches nothing. */
function useLimit(coworker: Coworker) {
  return useQuery({ queryKey: ["limit", coworker.id], queryFn: () => getLimit(coworker.id), retry: false });
}

/** What this coworker has spent, in one line, on the collapsed card. */
function PointsSummary({ coworker }: { coworker: Coworker }) {
  const limit = useLimit(coworker);
  if (limit.isLoading) return <span className="muted small">…</span>;
  const data = limit.data;
  if (limit.error || !data) return null;
  if (!data.metered) {
    return <span className="muted small">not metered — {data.note ?? "no key of its own"}</span>;
  }
  return (
    <span className="muted small">
      <strong>{commas(data.usedPoints)}</strong>
      {data.effectiveCap != null ? ` / ${commas(data.effectiveCap)}` : ""} this month
      <span className="dot" /> <strong>{commas(data.usedToday)}</strong>
      {data.dayCap != null ? ` / ${commas(data.dayCap)}` : ""} today
    </span>
  );
}

/**
 * The coworker's points: the cap and brake its owner set (editable here — the owner's, at most the
 * pool), and the pool it draws on. Lives in the card's details, not in a table cell: two inputs and
 * a Save button inside a `<td>` is a form pretending to be data.
 */
function PointsEditor({ coworker }: { coworker: Coworker }) {
  const queryClient = useQueryClient();
  const limit = useLimit(coworker);
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
  if (limit.isLoading) return <p className="empty">Loading…</p>;
  if (limit.error) return <p className="error">{errorText(limit.error, "could not load points")}</p>;
  const data = limit.data;
  if (!data) return null;
  const capShown = cap ?? (data.cap == null ? "" : String(data.cap));
  const dayShown = dayCap ?? (data.dayCap == null ? "" : String(data.dayCap));
  return (
    <div className="stack tight">
      <div className="row">
        <input
          className="tight"
          type="text"
          inputMode="numeric"
          value={capShown}
          onChange={(e) => setCap(e.target.value)}
          placeholder="cap / month"
          aria-label={`Monthly cap for ${coworker.name}`}
        />
        <input
          className="tight"
          type="text"
          inputMode="numeric"
          value={dayShown}
          onChange={(e) => setDayCap(e.target.value)}
          placeholder="brake / day"
          aria-label={`Daily brake for ${coworker.name}`}
        />
        <button
          className="sm"
          onClick={() => save.mutate()}
          disabled={save.isPending || (cap == null && dayCap == null)}
        >
          Save
        </button>
        {save.error ? <span className="error">{errorText(save.error, "could not save")}</span> : null}
      </div>
      <p className="muted small" style={{ margin: 0 }}>
        {whenText("month", data.pool.resetsAt) ? `${whenText("month", data.pool.resetsAt)} · ` : ""}
        {data.dayCap != null && whenText("day", data.dayFreesAt) ? `${whenText("day", data.dayFreesAt)} · ` : ""}
        {data.pool.max == null
          ? "No pool: your admin has not set one."
          : `Your pool: ${commas(data.pool.used)} of ${commas(data.pool.max)} used this month.`}
      </p>
      {data.note && data.metered ? <p className="muted small" style={{ margin: 0 }}>{data.note}</p> : null}
    </div>
  );
}

/**
 * One coworker, as a card rather than a table row.
 *
 * The roster was a five-column table whose last three columns were each a live form — a route
 * input with its Repin, a probe with its verdict, and a two-field points editor with its Save. A
 * `<td>` gives a form no room to lay out, so every button wrapped below its own input and the row
 * grew to four lines of controls with nothing telling you which coworker they belonged to. Here
 * the identity and the spend are always visible, and the editing is one disclosure away.
 */
function CoworkerCard({ coworker, models }: { coworker: Coworker; models: string[] }) {
  const queryClient = useQueryClient();
  const [model, setModel] = useState(coworker.model);
  const [open, setOpen] = useState<"none" | "settings" | "calls">("none");
  const repin = useMutation({
    mutationFn: () => repinCoworker(coworker.id, model),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["coworkers"] }),
  });
  const toggle = (which: "settings" | "calls") => setOpen((now) => (now === which ? "none" : which));

  return (
    <article className="item">
      <div className="item-head">
        <div className="item-id">
          <strong>{coworker.name}</strong>
          <span className="chip">{coworker.model}</span>
        </div>
        <PointsSummary coworker={coworker} />
        <div className="row item-actions">
          <button className="ghost sm" onClick={() => toggle("settings")} aria-expanded={open === "settings"}>
            {open === "settings" ? "Close" : "Settings"}
          </button>
          <button className="ghost sm" onClick={() => toggle("calls")} aria-expanded={open === "calls"}>
            Door calls
          </button>
        </div>
      </div>

      {open === "settings" ? (
        <div className="item-body stack">
          <div>
            <label htmlFor={`route-${coworker.id}`}>Route</label>
            <div className="row" id={`route-${coworker.id}`}>
              <ModelField
                value={model}
                onChange={setModel}
                models={models}
                label={`Route for ${coworker.name}`}
              />
              <button
                className="sm"
                onClick={() => repin.mutate()}
                disabled={repin.isPending || model.trim() === coworker.model}
              >
                Repin
              </button>
              <TestButton model={model} />
            </div>
            {repin.error ? <p className="error">{errorText(repin.error, "could not repin")}</p> : null}
          </div>
          <div>
            <label>Spend limits</label>
            <PointsEditor coworker={coworker} />
          </div>
        </div>
      ) : null}

      {open === "calls" ? (
        <div className="item-body">
          <McpCalls coworker={coworker} />
        </div>
      ) : null}
    </article>
  );
}

/**
 * Coworkers and the route each one thinks through.
 *
 * The model is shown as its own field, never as the description — the roster's habit of using
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
        <>
          <PageHead title="Coworkers">
            Everyone you have hired and the route each one thinks through. A route is a path through
            the gateway, never a key.
          </PageHead>

          <div className="stack">
            <section className="card">
              <h2>Hire a coworker</h2>
              <p className="hint">
                Leave the route blank to use this deployment&rsquo;s default. Test proves the
                gateway will actually serve it before you commit a name to it.
              </p>
              <form
                className="row"
                onSubmit={(e) => {
                  e.preventDefault();
                  if (name.trim()) hire.mutate();
                }}
              >
                <input
                  className="tight"
                  type="text"
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  placeholder="Name"
                  aria-label="Name"
                />
                <ModelField value={model} onChange={setModel} models={ids} label="Route" />
                {templates.data && templates.data.templates.length > 0 ? (
                  <select
                    className="tight"
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
                <button type="submit" disabled={!name.trim() || hire.isPending}>
                  {hire.isPending ? "Hiring…" : "Hire"}
                </button>
                <TestButton model={model} />
              </form>
              {hire.error ? <p className="err">{errorText(hire.error, "could not hire")}</p> : null}
              {hireNote ? <p className="err">{hireNote}</p> : null}
              {catalogue.data?.note ? <p className="muted small">{catalogue.data.note}</p> : null}
            </section>

            <section className="card">
              <h2>
                Your roster
                {coworkers.data ? <span className="count">{coworkers.data.length}</span> : null}
              </h2>
              {coworkers.isLoading ? (
                <p className="empty">Loading…</p>
              ) : coworkers.data && coworkers.data.length > 0 ? (
                <div className="list">
                  {coworkers.data.map((coworker) => (
                    <CoworkerCard key={coworker.id} coworker={coworker} models={ids} />
                  ))}
                </div>
              ) : (
                <p className="empty">No coworkers yet — hire one above.</p>
              )}
              {coworkers.error ? (
                <p className="err">{errorText(coworkers.error, "could not list coworkers")}</p>
              ) : null}
            </section>
          </div>
        </>
      )}
    </AuthedFrame>
  );
}
