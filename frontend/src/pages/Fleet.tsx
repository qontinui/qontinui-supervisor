import { Fragment, useCallback, useEffect, useState } from 'react';
import { useUIElement } from '@qontinui/ui-bridge/react';
import { api, WebFleetRunner } from '../lib/api';

// LocalStorage keys — the supervisor dashboard persists the user's backend URL
// and JWT here between sessions. The supervisor itself never stores them;
// it just proxies each request with the Authorization header the dashboard
// attaches.
const LS_BACKEND_URL_KEY = 'qontinui.supervisor.web.backend_url';
const LS_JWT_KEY = 'qontinui.supervisor.web.jwt';

function loadFromStorage(key: string): string {
  try {
    return localStorage.getItem(key) ?? '';
  } catch {
    return '';
  }
}

function saveToStorage(key: string, value: string) {
  try {
    if (value) {
      localStorage.setItem(key, value);
    } else {
      localStorage.removeItem(key);
    }
  } catch {
    // quota / private mode — non-fatal
  }
}

/**
 * Format a timestamp as relative time (e.g. "12s ago", "3m ago", "2h ago").
 * Returns `—` if the timestamp is null/empty or unparseable.
 */
function relativeTime(isoTs: string | null): string {
  if (!isoTs) return '—';
  const parsed = Date.parse(isoTs);
  if (Number.isNaN(parsed)) return '—';
  const deltaSecs = Math.max(0, (Date.now() - parsed) / 1000);
  if (deltaSecs < 60) return `${Math.round(deltaSecs)}s ago`;
  if (deltaSecs < 3600) return `${Math.round(deltaSecs / 60)}m ago`;
  if (deltaSecs < 86400) return `${Math.round(deltaSecs / 3600)}h ago`;
  return `${Math.round(deltaSecs / 86400)}d ago`;
}

/**
 * Map a fleet status string to a badge variant. Statuses come from
 * qontinui-web's `Runner.status` column; common values are:
 * `healthy`, `unhealthy`, `offline`, `registered`. Anything else falls back
 * to a neutral "warning" badge.
 */
function statusBadgeClass(status: string): string {
  const s = status.toLowerCase();
  if (s === 'healthy' || s === 'online' || s === 'ready') return 'badge-success';
  if (s === 'offline' || s === 'unhealthy' || s === 'dead') return 'badge-danger';
  return 'badge-warning';
}

/**
 * Format an ISO timestamp for display. Falls back to the raw string if the
 * parse fails so operators always see something.
 */
function formatTs(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleString();
}

// ─── Fleet row (per-runner) ──────────────────────────────────────────────────
// Extracted as a component so we can call useUIElement per-row with stable
// IDs derived from the runner's id (not an array index). Each row exposes
// two optional badges — ui-error and rust-crash — whose click `toggle`s the
// expanded detail panel below the row.

interface FleetRowProps {
  runner: WebFleetRunner;
  expanded: boolean;
  toggle: () => void;
}

function FleetRow({ runner: r, expanded, toggle }: FleetRowProps) {
  const hasError = r.ui_error != null || r.recent_crash != null;
  const rowToggle = () => hasError && toggle();

  // Register per-row UI Bridge elements for the two error badges so automation
  // can flip the detail panel without DOM scraping. Both hooks always run (IDs
  // are stable per runner id); they no-op for rows without an error by
  // returning a ref that's attached to nothing.
  const { ref: uiErrorBadgeRef, trigger: triggerUiError } = useUIElement({
    id: `fleet-row-ui-error-${r.id}`,
    type: 'button',
    label: `UI error badge for ${r.name}`,
    actions: ['click'],
    customActions: {
      toggle: {
        id: 'toggle',
        description: 'Toggle the expanded ui_error detail panel',
        handler: () => toggle(),
      },
    },
  });
  const { ref: rustCrashBadgeRef, trigger: triggerRustCrash } = useUIElement({
    id: `fleet-row-rust-crash-${r.id}`,
    type: 'button',
    label: `Rust crash badge for ${r.name}`,
    actions: ['click'],
    customActions: {
      toggle: {
        id: 'toggle',
        description: 'Toggle the expanded rust-crash detail panel',
        handler: () => toggle(),
      },
    },
  });
  // Keep unused-var lint quiet — triggers are exposed for consumers who want
  // to fire the custom action from inside the component (we don't here).
  void triggerUiError;
  void triggerRustCrash;

  return (
    <Fragment>
      <tr>
        <td>
          <strong>{r.name}</strong>
          <div className="text-muted" style={{ fontSize: '0.7rem' }}>
            {r.id}
          </div>
        </td>
        <td className="text-mono">
          {r.hostname}:{r.port}
        </td>
        <td>
          <div className="flex gap-2" style={{ alignItems: 'center', flexWrap: 'wrap' }}>
            <span className={`badge ${statusBadgeClass(r.status)}`}>{r.status}</span>
            {r.ui_error && (
              <span
                ref={uiErrorBadgeRef as React.RefCallback<HTMLSpanElement>}
                data-ui-bridge-value={String(expanded)}
                className="badge badge-danger badge-clickable"
                style={{ fontSize: '0.7rem' }}
                title={`UI error: ${r.ui_error.message}`}
                role="button"
                tabIndex={0}
                onClick={rowToggle}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' || e.key === ' ') {
                    e.preventDefault();
                    rowToggle();
                  }
                }}
              >
                ui error
                <span style={{ marginLeft: '0.25rem', opacity: 0.7, fontSize: '0.65rem' }}>
                  {expanded ? '▾' : '▸'}
                </span>
              </span>
            )}
            {r.recent_crash && (
              <span
                ref={rustCrashBadgeRef as React.RefCallback<HTMLSpanElement>}
                data-ui-bridge-value={String(expanded)}
                className="badge badge-danger badge-clickable"
                style={{ fontSize: '0.7rem' }}
                title={`Rust crash: ${r.recent_crash.panic_message ?? 'runner restarted after Rust panic'}${r.recent_crash.panic_location ? ` @ ${r.recent_crash.panic_location}` : ''}`}
                role="button"
                tabIndex={0}
                onClick={rowToggle}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' || e.key === ' ') {
                    e.preventDefault();
                    rowToggle();
                  }
                }}
              >
                rust crash
                <span style={{ marginLeft: '0.25rem', opacity: 0.7, fontSize: '0.65rem' }}>
                  {expanded ? '▾' : '▸'}
                </span>
              </span>
            )}
          </div>
        </td>
        <td>{relativeTime(r.last_heartbeat)}</td>
        <td>
          <div className="flex gap-2" style={{ flexWrap: 'wrap' }}>
            {r.capabilities.length === 0 ? (
              <span className="text-muted">—</span>
            ) : (
              r.capabilities.map((cap) => (
                <span
                  key={cap}
                  className="badge"
                  style={{
                    background: 'var(--bg-tertiary)',
                    color: 'var(--text-secondary)',
                    fontSize: '0.7rem',
                  }}
                >
                  {cap}
                </span>
              ))
            )}
          </div>
        </td>
        <td>
          <div className="flex gap-2" style={{ flexWrap: 'wrap' }}>
            {r.server_mode && (
              <span className="badge badge-success" style={{ fontSize: '0.7rem' }}>
                server
              </span>
            )}
            {r.restate_enabled && (
              <span
                className={`badge ${r.restate_healthy ? 'badge-success' : 'badge-warning'}`}
                style={{ fontSize: '0.7rem' }}
              >
                restate{r.restate_healthy ? '' : ' (unhealthy)'}
              </span>
            )}
            {!r.server_mode && !r.restate_enabled && (
              <span className="text-muted">—</span>
            )}
          </div>
        </td>
      </tr>
      {expanded && hasError && (
        <tr>
          <td colSpan={6} style={{ padding: 0 }}>
            <div
              style={{
                margin: '0.25rem 0.5rem 0.5rem 0.5rem',
                padding: '0.6rem 0.75rem',
                background: 'rgba(239,68,68,0.08)',
                border: '1px solid rgba(239,68,68,0.3)',
                borderRadius: 4,
                fontSize: '0.75rem',
              }}
            >
              {r.ui_error && (
                <div style={{ marginBottom: r.recent_crash ? '0.6rem' : 0 }}>
                  <div style={{ marginBottom: '0.4rem' }}>
                    <strong className="text-danger">UI Error:</strong>{' '}
                    <span style={{ fontFamily: 'var(--font-mono)' }}>
                      {r.ui_error.message}
                    </span>
                  </div>
                  <div
                    className="text-muted"
                    style={{
                      fontSize: '0.7rem',
                      display: 'flex',
                      gap: '1rem',
                      flexWrap: 'wrap',
                    }}
                  >
                    <span>
                      <strong>First seen:</strong> {formatTs(r.ui_error.first_seen)}
                    </span>
                    <span>
                      <strong>Last reported:</strong>{' '}
                      {formatTs(r.ui_error.reported_at)}
                    </span>
                    <span>
                      <strong>Count:</strong> {r.ui_error.count}
                    </span>
                    {r.ui_error.digest && (
                      <span>
                        <strong>Digest:</strong>{' '}
                        <span style={{ fontFamily: 'var(--font-mono)' }}>
                          {r.ui_error.digest}
                        </span>
                      </span>
                    )}
                  </div>
                </div>
              )}
              {r.recent_crash && (
                <div>
                  <div style={{ marginBottom: '0.4rem' }}>
                    <strong className="text-danger">Rust Crash:</strong>{' '}
                    <span style={{ fontFamily: 'var(--font-mono)' }}>
                      {r.recent_crash.panic_message ??
                        'runner restarted after Rust panic (no message captured)'}
                    </span>
                  </div>
                  <div
                    className="text-muted"
                    style={{
                      fontSize: '0.7rem',
                      display: 'flex',
                      gap: '1rem',
                      flexWrap: 'wrap',
                      marginBottom: '0.4rem',
                    }}
                  >
                    <span>
                      <strong>Reported:</strong>{' '}
                      {formatTs(r.recent_crash.reported_at)}
                    </span>
                    {r.recent_crash.panic_location && (
                      <span>
                        <strong>Location:</strong>{' '}
                        <span style={{ fontFamily: 'var(--font-mono)' }}>
                          {r.recent_crash.panic_location}
                        </span>
                      </span>
                    )}
                    {r.recent_crash.thread && (
                      <span>
                        <strong>Thread:</strong>{' '}
                        <span style={{ fontFamily: 'var(--font-mono)' }}>
                          {r.recent_crash.thread}
                        </span>
                      </span>
                    )}
                  </div>
                  <div className="text-muted" style={{ fontSize: '0.7rem' }}>
                    <strong>Dump file:</strong>{' '}
                    <span
                      style={{
                        fontFamily: 'var(--font-mono)',
                        wordBreak: 'break-all',
                      }}
                    >
                      {r.recent_crash.file_path}
                    </span>
                  </div>
                </div>
              )}
            </div>
          </td>
        </tr>
      )}
    </Fragment>
  );
}

export default function Fleet() {
  const [backendUrl, setBackendUrl] = useState<string>(() => loadFromStorage(LS_BACKEND_URL_KEY));
  const [jwt, setJwt] = useState<string>(() => loadFromStorage(LS_JWT_KEY));
  const [runners, setRunners] = useState<WebFleetRunner[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [lastFetchedAt, setLastFetchedAt] = useState<string | null>(null);
  const [hasFetched, setHasFetched] = useState(false);
  // Which runner rows have their error-detail row expanded. Scoped to the
  // component so expansion state survives periodic refreshes without
  // disappearing on every tick; keyed by runner id.
  const [expandedIds, setExpandedIds] = useState<Set<string>>(() => new Set());

  const toggleExpanded = (id: string) => {
    setExpandedIds((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  // Persist form inputs to localStorage whenever they change so the user
  // doesn't have to re-enter the JWT on every page reload.
  useEffect(() => {
    saveToStorage(LS_BACKEND_URL_KEY, backendUrl);
  }, [backendUrl]);
  useEffect(() => {
    saveToStorage(LS_JWT_KEY, jwt);
  }, [jwt]);

  const refresh = useCallback(async () => {
    if (!backendUrl.trim() || !jwt.trim()) {
      setError('Backend URL and JWT are both required.');
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const result = await api.webFleet(backendUrl.trim(), jwt.trim());
      setRunners(Array.isArray(result) ? result : []);
      setLastFetchedAt(new Date().toISOString());
      setHasFetched(true);
    } catch (e) {
      setRunners([]);
      setError(e instanceof Error ? e.message : String(e));
      setHasFetched(true);
    } finally {
      setLoading(false);
    }
  }, [backendUrl, jwt]);

  const configured = backendUrl.trim() !== '' && jwt.trim() !== '';

  // Register the primary dashboard controls (refresh / clear / backend URL /
  // JWT input) with UI Bridge so automation can drive them without DOM
  // scraping. IDs are stable — no array index, no derived data.
  const { ref: refreshBtnRef } = useUIElement({
    id: 'fleet-refresh',
    type: 'button',
    label: 'Refresh fleet list',
    actions: ['click'],
  });
  const { ref: clearBtnRef } = useUIElement({
    id: 'fleet-clear',
    type: 'button',
    label: 'Clear backend URL and JWT',
    actions: ['click'],
  });
  const { ref: backendUrlRef } = useUIElement({
    id: 'fleet-backend-url',
    type: 'input',
    label: 'Backend URL input',
  });
  const { ref: jwtRef } = useUIElement({
    id: 'fleet-jwt',
    type: 'input',
    label: 'JWT password input',
  });

  return (
    <div>
      <div className="page-header">
        <h1 className="page-title">Web Fleet</h1>
      </div>
      <p className="page-desc">
        Read-only view of the qontinui-web runner registry. The supervisor proxies{' '}
        <span className="text-mono">GET /api/v1/runners</span> at the backend URL below, attaching
        the JWT you supply. Credentials live only in your browser's localStorage.
      </p>

      {/* Config card */}
      <div className="card" style={{ marginBottom: '1rem' }}>
        <div className="card-header">
          <span className="card-title">Backend Configuration</span>
          {lastFetchedAt && (
            <span className="text-muted" style={{ fontSize: '0.75rem' }}>
              Last fetched {relativeTime(lastFetchedAt)}
            </span>
          )}
        </div>
        <div
          style={{
            display: 'grid',
            gap: '0.5rem',
            gridTemplateColumns: '1fr',
            marginTop: '0.5rem',
          }}
        >
          <label className="text-muted" style={{ fontSize: '0.75rem' }}>
            Backend URL
            <input
              ref={backendUrlRef as React.RefCallback<HTMLInputElement>}
              type="text"
              className="log-filter"
              style={{ display: 'block', width: '100%', marginTop: '0.2rem', padding: '0.4rem' }}
              placeholder="https://api.qontinui.io"
              value={backendUrl}
              onChange={(e) => setBackendUrl(e.target.value)}
              spellCheck={false}
              autoComplete="off"
            />
          </label>
          <label className="text-muted" style={{ fontSize: '0.75rem' }}>
            JWT (user access token)
            <input
              ref={jwtRef as React.RefCallback<HTMLInputElement>}
              type="password"
              className="log-filter"
              style={{
                display: 'block',
                width: '100%',
                marginTop: '0.2rem',
                padding: '0.4rem',
                fontFamily: 'var(--font-mono)',
              }}
              placeholder="eyJhbGciOiJIUzI1NiIs…"
              value={jwt}
              onChange={(e) => setJwt(e.target.value)}
              spellCheck={false}
              autoComplete="off"
            />
          </label>
          <div className="flex gap-2" style={{ marginTop: '0.5rem' }}>
            <button
              ref={refreshBtnRef as React.RefCallback<HTMLButtonElement>}
              className="btn btn-primary"
              disabled={loading || !configured}
              onClick={refresh}
            >
              {loading ? 'Refreshing…' : 'Refresh'}
            </button>
            <button
              ref={clearBtnRef as React.RefCallback<HTMLButtonElement>}
              className="btn"
              disabled={loading}
              onClick={() => {
                setBackendUrl('');
                setJwt('');
                setRunners([]);
                setError(null);
                setHasFetched(false);
                setLastFetchedAt(null);
              }}
            >
              Clear
            </button>
          </div>
        </div>
      </div>

      {/* Error state */}
      {error && (
        <div
          className="card"
          style={{ marginBottom: '1rem', borderColor: 'var(--danger)' }}
        >
          <div className="card-header">
            <span className="card-title text-danger">Error</span>
          </div>
          <div
            className="text-mono text-danger"
            style={{ fontSize: '0.8rem', whiteSpace: 'pre-wrap', wordBreak: 'break-word' }}
          >
            {error}
          </div>
        </div>
      )}

      {/* Table / empty state */}
      <div className="card">
        <div className="card-header">
          <span className="card-title">Registered Runners</span>
          <span className={`badge ${runners.length > 0 ? 'badge-success' : 'badge-warning'}`}>
            {runners.length} total
          </span>
        </div>

        {!configured && !hasFetched && (
          <div className="text-muted" style={{ marginTop: '0.5rem' }}>
            Configure the backend URL + JWT above to see the fleet.
          </div>
        )}

        {configured && !hasFetched && !loading && (
          <div className="text-muted" style={{ marginTop: '0.5rem' }}>
            Press <strong>Refresh</strong> to load the fleet from{' '}
            <span className="text-mono">{backendUrl}</span>.
          </div>
        )}

        {hasFetched && !error && runners.length === 0 && (
          <div className="text-muted" style={{ marginTop: '0.5rem' }}>
            No runners registered with this backend.
          </div>
        )}

        {runners.length > 0 && (
          <div className="table-container" style={{ marginTop: '0.5rem' }}>
            <table>
              <thead>
                <tr>
                  <th>Name</th>
                  <th>Host</th>
                  <th>Status</th>
                  <th>Last heartbeat</th>
                  <th>Capabilities</th>
                  <th>Mode</th>
                </tr>
              </thead>
              <tbody>
                {runners.map((r) => (
                  <FleetRow
                    key={r.id}
                    runner={r}
                    expanded={expandedIds.has(r.id)}
                    toggle={() => toggleExpanded(r.id)}
                  />
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  );
}
