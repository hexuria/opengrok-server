import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The console is served by the Rust server under /console, so every asset URL is prefixed with it.
// The API lives at the SAME origin (the Rust server), so in production the SPA calls /account,
// /auth/login, /admin/* directly and the httpOnly cookies ride along with no CORS.
//
// In dev (`bun run dev`), Vite serves the SPA and proxies those API paths to the Rust server named
// by OG_DEV_API (default the gate/dev port), so cookie semantics match production closely enough to
// build against. The real proof is Slice D: the built assets served by Axum, true same-origin.
const API = process.env.OG_DEV_API ?? "http://127.0.0.1:1474";
const proxy = Object.fromEntries(
  ["/account", "/admin", "/auth"].map((p) => [p, { target: API, changeOrigin: false }]),
);

export default defineConfig({
  base: "/console/",
  plugins: [react()],
  server: { proxy },
  test: {
    environment: "node",
  },
});
