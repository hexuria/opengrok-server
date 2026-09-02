// The coworker surface — the JSON the server already speaks (agui/routes.rs).
//
// A coworker's `model` is a ROUTE through the gateway (`openai/gpt-5.5`), never a key. The
// catalogue below is the gateway's own list, fetched by the server on our behalf: the browser
// never holds the credential that can ask for it.
import { getJson, postJson, request, ApiError } from "./client";

export interface Coworker {
  id: string;
  name: string;
  /** The route this coworker thinks through. */
  model: string;
  boxId?: string | null;
}

export function listCoworkers(): Promise<Coworker[]> {
  return getJson<Coworker[]>("/coworkers");
}

export function hireCoworker(name: string, model?: string): Promise<Coworker> {
  return postJson<Coworker>("/coworkers", {
    name,
    // Absent means "the deployment's default" — the server decides what that is.
    model: model && model.trim() ? model.trim() : undefined,
  });
}

export async function repinCoworker(id: string, model: string): Promise<Coworker> {
  const res = await request(`/coworkers/${encodeURIComponent(id)}`, {
    method: "PATCH",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ model }),
  });
  if (!res.ok) throw new ApiError(res.status, await res.text());
  return (await res.json()) as Coworker;
}

/** One call through the MCP door, as the server's audit recorded it (arguments already redacted). */
export interface McpCall {
  callId: string;
  tool: string;
  arguments: unknown;
  /** ok | failed | refused | awaiting | error — see the server's McpCallView. */
  outcome: string;
  requestId: string;
  atMs: number;
}

export function listMcpCalls(id: string, limit = 20): Promise<McpCall[]> {
  return getJson<McpCall[]>(`/coworkers/${encodeURIComponent(id)}/mcp-calls?limit=${limit}`);
}

/** The three limits, each optional; an absent one means "that layer says nothing". */
export interface SpendLimit {
  fiveHourUsd?: string | null;
  sevenDayUsd?: string | null;
  monthUsd?: string | null;
}

/** One window's meter: used, the limit it is under, and when it next frees up (RFC 3339). */
export interface WindowMeter {
  window: "5h" | "7d" | "month";
  usedUsd?: string | null;
  limitUsd?: string | null;
  freesAt?: string | null;
}

/**
 * A coworker's spend: whether it is metered at all (a key of its own), why not when it is not,
 * the limits it is under (org default → member → its own, most specific wins) and the three
 * meters. Read-only here; the org admin writes limits on the admin page.
 */
export interface CoworkerSpend {
  metered: boolean;
  note?: string | null;
  keyPrefix?: string | null;
  limits: SpendLimit;
  windows: WindowMeter[];
}

export function getSpend(id: string): Promise<CoworkerSpend> {
  return getJson<CoworkerSpend>(`/coworkers/${encodeURIComponent(id)}/spend`);
}

export interface Catalogue {
  models: { id: string }[];
  /** Why the list is empty, when it is. An empty list is never silently "there are no models". */
  note: string | null;
}

export function listModels(): Promise<Catalogue> {
  return getJson<Catalogue>("/models");
}

export interface ProbeResult {
  ok: boolean;
  /** The model the gateway actually served, when it did. */
  served?: string;
  /** The gateway's own words when it would not. */
  detail?: string;
}

export function probeModel(model: string): Promise<ProbeResult> {
  return postJson<ProbeResult>("/models/probe", { model });
}
