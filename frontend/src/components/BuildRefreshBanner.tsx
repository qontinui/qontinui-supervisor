import { useState } from 'react';
import { useBuildIdWatcher } from '@qontinui/ui-bridge/react';

/**
 * Watches the supervisor's build-id signal and renders a non-blocking refresh
 * banner when the server has shipped a new bundle.
 *
 * Flow:
 *   1. Supervisor injects `<meta name="build-id">` into the served HTML at
 *      a value tied to the embedded `dist/index.html` mtime.
 *   2. Supervisor emits `buildId` on `/health/stream` SSE events.
 *   3. `useBuildIdWatcher` compares (1) against (2) and toggles `stale` when
 *      they diverge — i.e. the supervisor was rebuilt + restarted while this
 *      tab was open.
 *   4. The banner offers a one-click reload that picks up the new bundle.
 *
 * Inline styles keep this self-contained — no new CSS rules and no
 * coordination with `index.css`.
 */
export function BuildRefreshBanner() {
  const [stale, setStale] = useState(false);
  useBuildIdWatcher({
    healthStreamUrl: '/health/stream',
    onBuildIdChange: () => setStale(true),
  });

  if (!stale) return null;

  return (
    <div
      role="status"
      aria-live="polite"
      style={{
        position: 'fixed',
        bottom: 16,
        right: 16,
        zIndex: 9999,
        display: 'flex',
        alignItems: 'center',
        gap: 12,
        padding: '10px 14px',
        background: 'var(--bg-tertiary, #242837)',
        color: 'var(--text-primary, #e4e4e7)',
        border: '1px solid var(--accent, #6366f1)',
        borderRadius: 8,
        boxShadow: '0 4px 16px rgba(0, 0, 0, 0.35)',
        fontSize: '0.875rem',
      }}
    >
      <span>New version available — refresh to update</span>
      <button
        type="button"
        onClick={() => window.location.reload()}
        style={{
          background: 'var(--accent, #6366f1)',
          color: '#fff',
          border: 'none',
          borderRadius: 4,
          padding: '4px 10px',
          fontSize: '0.8125rem',
          fontWeight: 600,
          cursor: 'pointer',
        }}
      >
        Refresh
      </button>
    </div>
  );
}
