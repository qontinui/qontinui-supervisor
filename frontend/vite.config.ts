/// <reference types="vitest" />
import { defineConfig, type ProxyOptions } from 'vite';
import type { ClientRequest, IncomingMessage } from 'node:http';
import react from '@vitejs/plugin-react';
import path from 'path';
import { readFileSync } from 'fs';

const pkg = JSON.parse(readFileSync(path.resolve(__dirname, 'package.json'), 'utf-8'));

const SUPERVISOR = 'http://localhost:9875';
const DEV_PORT = 5174;
/** This dev server's own origins: the only ones rewritten to the supervisor's. */
const DEV_ORIGINS = new Set([
  `http://localhost:${DEV_PORT}`,
  `http://127.0.0.1:${DEV_PORT}`,
  `http://[::1]:${DEV_PORT}`,
]);

/**
 * Proxy one path prefix to the supervisor so it passes the supervisor's origin
 * and Host guard (src/origin_guard.rs).
 *
 * - `changeOrigin` rewrites `Host` from this dev server (`localhost:5174`) to
 *   `localhost:9875`; the Host gate refuses any other port.
 * - The browser sends `Origin: http://localhost:5174` on this page's POSTs.
 *   The supervisor admits only its own origin, so an `Origin` in the fixed
 *   DEV_ORIGINS set (built from `server.port`, never from the request's
 *   `Host`, which a client controls) is rewritten to the supervisor's. Any
 *   other origin is forwarded untouched and refused, so the dev server never
 *   launders a foreign page's request. That is why this lives here and the
 *   supervisor does not admit `:5174` itself: any page on that port, from any
 *   project, would then reach every supervisor route.
 */
function toSupervisor(ws = false): ProxyOptions {
  const rewriteOwnOrigin = (proxyReq: ClientRequest, req: IncomingMessage) => {
    const origin = req.headers.origin;
    if (origin && DEV_ORIGINS.has(origin)) {
      proxyReq.setHeader('origin', SUPERVISOR);
    }
  };
  return {
    target: ws ? SUPERVISOR.replace(/^http/, 'ws') : SUPERVISOR,
    ws,
    changeOrigin: true,
    configure: (proxy) => {
      proxy.on('proxyReq', rewriteOwnOrigin);
      proxy.on('proxyReqWs', rewriteOwnOrigin);
    },
  };
}

export default defineConfig({
  plugins: [react()],
  define: {
    __APP_VERSION__: JSON.stringify(pkg.version),
  },
  test: {
    globals: true,
    environment: 'jsdom',
    setupFiles: ['./src/test-setup.ts'],
  },
  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
    },
  },
  build: {
    outDir: '../dist',
    emptyOutDir: true,
    rollupOptions: {
      output: {
        manualChunks: {
          recharts: ['recharts'],
          react: ['react', 'react-dom', 'react-router-dom'],
        },
      },
    },
  },
  server: {
    port: DEV_PORT,
    // The proxy's Origin rewrite matches only DEV_PORT; a silent fallback to
    // another port would turn every dashboard POST into a 403.
    strictPort: true,
    /**
     * One entry per `origin_guard::API_SEGMENTS` value, pinned by the Rust
     * tripwire `every_api_segment_is_reachable_through_the_dev_proxy`.
     *
     * The list has to be complete now, where before the guard it did not.
     * A dashboard page used to be able to reach an unproxied route by asking
     * `http://localhost:9875` directly, cross-origin, because CORS answered
     * `*`; the Origin gate closes that, deliberately and for good. So an API
     * segment missing from this list is not "inconvenient in dev" — it is
     * unreachable in dev, silently, with the Vite server answering the SPA
     * shell in place of the route. `/builds`, `/build/{id}/status`,
     * `/web-fleet` and `/supervisor-bridge/boot-id` were in that state: all
     * four are called by the dashboard (`lib/api.ts`,
     * `components/BootIdWatcher.tsx`).
     *
     * The failure is silent because the supervisor's SPA fallback
     * (`src/routes/dashboard.rs`) answers an unmatched path with **200 and
     * index.html**, not a 404 — and, being registered `get`-only, answers an
     * unmatched POST with 405. So an unproxied API route does not error; it
     * returns an HTML page to a caller expecting JSON, which `fetchJson` then
     * reports as a parse failure if it reports anything at all.
     *
     * Vite matches a string key as a plain prefix, so `/runner` would already
     * carry `/runner-api` and `/runners`. Every segment is still listed, so
     * this block reads as the API surface it mirrors and the tripwire's
     * failure names the one segment to add.
     *
     * Prefix matching cuts both ways, and three PRE-EXISTING keys shadow a
     * client route on a hard load in dev: `/runner` over `/runner-monitor`,
     * `/eval` over `/evaluation`, `/velocity` over `/velocity` itself. Those
     * URLs are then served by the supervisor's EMBEDDED production SPA rather
     * than this dev server (in-app navigation is unaffected). None of the keys
     * added for API_SEGMENTS coverage introduces a new shadow — each was
     * checked against `src/App.tsx`'s routes — and that is the test to apply
     * before adding another.
     */
    proxy: {
      '/actions': toSupervisor(),
      '/ai': toSupervisor(),
      '/build': toSupervisor(),
      '/builds': toSupervisor(),
      '/ci-runner': toSupervisor(),
      '/control': toSupervisor(),
      '/diagnostics': toSupervisor(),
      '/eval': toSupervisor(),
      '/expo': toSupervisor(),
      '/graphql': toSupervisor(),
      '/health': toSupervisor(),
      '/help': toSupervisor(),
      '/lkg': toSupervisor(),
      '/logs': toSupervisor(),
      '/runner': toSupervisor(),
      '/runner-api': toSupervisor(),
      '/runners': toSupervisor(),
      '/spawn-worktrees': toSupervisor(),
      '/supervisor': toSupervisor(),
      '/supervisor-bridge': toSupervisor(),
      '/test-login': toSupervisor(),
      '/ui-bridge': toSupervisor(),
      '/web-fleet': toSupervisor(),
      '/ws': toSupervisor(true),
      // Shared segments: an API route AND a dashboard client route live under
      // each (`origin_guard::SHARED_SPA_SEGMENTS`, which has FOUR entries), so
      // the tripwire cannot require them — proxying the bare prefix shadows the
      // client route, and not proxying it strands the API route.
      //
      // Three are proxied at the prefix because that is what this dev server
      // has always done, shadow included:
      '/velocity': toSupervisor(),
      '/velocity-tests': toSupervisor(),
      '/velocity-improvement': toSupervisor(),
      // The fourth, `/lineage`, was proxied NOWHERE, so `/lineage/recent`,
      // `/lineage/stats` and the two `/lineage/{sessions,commits}/...` routes
      // (src/server.rs) got the SPA shell in dev: `fetchJson` threw a parse
      // error that `pages/Lineage.tsx` swallows with `.catch(() => …)`, leaving
      // a blank page and no error — the exact silent failure described above,
      // live. Proxied per API SUBPATH rather than at `/lineage`, because the
      // client route is exactly `/lineage` (App.tsx) and a bare prefix would
      // shadow the page this fixes.
      '/lineage/recent': toSupervisor(),
      '/lineage/stats': toSupervisor(),
      '/lineage/sessions': toSupervisor(),
      '/lineage/commits': toSupervisor(),
      // No supervisor route answers either prefix today, while the dashboard
      // calls both (`api.devStartStatus` runs on every Dashboard load;
      // `api.wlStatus`). Kept so the wiring is here if the routes return, and
      // recorded as its own defect rather than silently deleted: proxied or
      // not the outcome is the same, and it is NOT a 404 — the SPA fallback
      // answers `GET /dev-start/status` with 200 + index.html, and the
      // `get`-only fallback answers `api.devStartAction`'s POST with 405.
      // Coord finding 91da7570-5d89-4f1e-aafe-2d7104f701d3.
      '/dev-start': toSupervisor(),
      '/workflow-loop': toSupervisor(),
    },
  },
});
