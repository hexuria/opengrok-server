import { afterEach, describe, expect, it, vi } from "vitest";
import { ApiError, getJson, request } from "./client";

afterEach(() => {
  vi.restoreAllMocks();
});

describe("request", () => {
  it("refreshes once on a 401 and replays the original request", async () => {
    const calls: string[] = [];
    const fetchMock = vi.fn(async (url: string, init?: RequestInit & { _retried?: boolean }) => {
      calls.push(`${init?.method ?? "GET"} ${url}`);
      if (url === "/auth/refresh") return new Response(null, { status: 200 });
      if (init?._retried) return new Response(JSON.stringify({ ok: true }), { status: 200 });
      return new Response("unauthorized", { status: 401 });
    });
    vi.stubGlobal("fetch", fetchMock);

    const res = await request("/account");
    expect(res.status).toBe(200);
    expect(calls).toEqual(["GET /account", "POST /auth/refresh", "GET /account"]);
  });

  it("does not loop when the refresh itself fails", async () => {
    const calls: string[] = [];
    const fetchMock = vi.fn(async (url: string) => {
      calls.push(url);
      return new Response("no", { status: 401 });
    });
    vi.stubGlobal("fetch", fetchMock);

    const res = await request("/account");
    expect(res.status).toBe(401);
    // Exactly one attempt at /account, plus the single failed refresh — no retry.
    expect(calls.filter((c) => c === "/account").length).toBe(1);
    expect(calls).toContain("/auth/refresh");
  });
});

describe("getJson", () => {
  it("throws an ApiError carrying the server's message", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response(JSON.stringify({ error: "avatar must be a data:image/ URL" }), { status: 422 })),
    );
    await expect(getJson("/account")).rejects.toMatchObject({
      status: 422,
      message: "avatar must be a data:image/ URL",
    });
    await expect(getJson("/account")).rejects.toBeInstanceOf(ApiError);
  });
});
