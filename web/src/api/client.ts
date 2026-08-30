// The one place the console talks to the server.
//
// AUTH IS COOKIES, NOT TOKENS. Every request goes out with `credentials: "include"` so the browser
// carries the httpOnly `og_access` cookie; this code never reads or stores a token — it cannot, the
// cookies are httpOnly by design (see the server's auth/cookies.rs). On a 401 we try ONE rotation
// through `POST /auth/refresh` (which reads the refresh cookie and re-sets the access cookie) and
// replay the original request; a second 401 means the session is truly gone and the caller routes
// to /login.

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

/** True once a refresh has already been attempted for this request, so we never loop. */
type Init = RequestInit & { _retried?: boolean };

async function raw(path: string, init: Init): Promise<Response> {
  return fetch(path, { ...init, credentials: "include" });
}

/** Fetch with the cookie session and a single transparent refresh-and-retry on 401. */
export async function request(path: string, init: Init = {}): Promise<Response> {
  const res = await raw(path, init);
  if (res.status === 401 && !init._retried) {
    const refreshed = await raw("/auth/refresh", { method: "POST" });
    if (refreshed.ok) {
      return request(path, { ...init, _retried: true });
    }
  }
  return res;
}

async function readError(res: Response): Promise<string> {
  const text = await res.text().catch(() => "");
  if (!text) return `Request failed (${res.status}).`;
  try {
    const parsed = JSON.parse(text) as { error?: string };
    if (parsed && typeof parsed.error === "string") return parsed.error;
  } catch {
    // Not JSON — the plain text is the message.
  }
  return text;
}

/** GET JSON, throwing an ApiError with the server's message on a non-2xx. */
export async function getJson<T>(path: string): Promise<T> {
  const res = await request(path);
  if (!res.ok) throw new ApiError(res.status, await readError(res));
  return (await res.json()) as T;
}

/** POST JSON, throwing an ApiError on a non-2xx. Returns the parsed body, or `undefined` for 204. */
export async function postJson<T>(path: string, body?: unknown): Promise<T> {
  const res = await request(path, {
    method: "POST",
    headers: body === undefined ? undefined : { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!res.ok) throw new ApiError(res.status, await readError(res));
  if (res.status === 204) return undefined as T;
  const text = await res.text();
  return (text ? JSON.parse(text) : undefined) as T;
}
