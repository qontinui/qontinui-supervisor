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
    proxy: {
      '/health': toSupervisor(),
      '/runner': toSupervisor(),
      '/logs': toSupervisor(),
      '/ai': toSupervisor(),
      '/dev-start': toSupervisor(),
      '/velocity': toSupervisor(),
      '/workflow-loop': toSupervisor(),
      '/diagnostics': toSupervisor(),
      '/ui-bridge': toSupervisor(),
      '/runner-api': toSupervisor(),
      '/expo': toSupervisor(),
      '/eval': toSupervisor(),
      '/velocity-tests': toSupervisor(),
      '/velocity-improvement': toSupervisor(),
      '/ws': toSupervisor(true),
    },
  },
});
