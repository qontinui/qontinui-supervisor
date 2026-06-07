import { useCallback, useEffect, useState } from 'react';
import { useUIElement } from '@qontinui/ui-bridge/react';
import { api, LineageRow, LineageStats } from '../lib/api';
import { SessionChip } from '../components/SessionChip';
import { LS_JWT_KEY, loadFromStorage, saveToStorage } from './Fleet';

const GITHUB = 'https://github.com';

// Stable module-level empty refs — never hand a fresh literal to identity-
// memoing consumers (memory: stable-empty-refs).
const EMPTY_ROWS: LineageRow[] = [];

function shortSha(sha: string): string {
  return sha.slice(0, 7);
}

function formatTs(iso: string | null): string {
  if (!iso) return '—';
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString();
}

/** Heuristic: a leading 401/403 in a proxy error means the JWT was missing,
 *  expired, or rejected by coord — an auth problem, not coord-unreachable. */
function isAuthError(msg: string): boolean {
  return /^\s*40[13]\b/.test(msg);
}

export default function Lineage() {
  // Reuse the Fleet tab's operator-JWT key so one paste serves both pages.
  const [jwt, setJwt] = useState<string>(() => loadFromStorage(LS_JWT_KEY));
  const [stats, setStats] = useState<LineageStats | null>(null);
  const [rows, setRows] = useState<LineageRow[]>(EMPTY_ROWS);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // True when the failure is specifically a missing/rejected credential — the
  // page then prompts for a JWT rather than showing a "coord unavailable" banner.
  const [authError, setAuthError] = useState(false);
  const [lastFetchedAt, setLastFetchedAt] = useState<string | null>(null);
  const [hasFetched, setHasFetched] = useState(false);

  const hasJwt = jwt.trim() !== '';

  // Persist the JWT to the shared key so a paste here also satisfies Fleet.
  useEffect(() => {
    saveToStorage(LS_JWT_KEY, jwt);
  }, [jwt]);

  const loadData = useCallback(async () => {
    const token = jwt.trim();
    if (!token) {
      // No credential — don't hit coord; the empty-state prompt handles this.
      setError(null);
      setAuthError(false);
      return;
    }
    setLoading(true);
    setError(null);
    setAuthError(false);
    try {
      const [s, r] = await Promise.all([
        api.lineageStats(token).catch(() => null),
        api.lineageRecent(100, token).catch(() => EMPTY_ROWS),
      ]);
      setStats(s);
      setRows(Array.isArray(r) && r.length > 0 ? r : EMPTY_ROWS);
      setLastFetchedAt(new Date().toISOString());
      setHasFetched(true);
      // Surface a hard error only when BOTH calls failed; re-probe once for a
      // real message and classify it as auth vs coord-unreachable.
      if (s === null && (!Array.isArray(r) || r.length === 0)) {
        await api.lineageStats(token);
      }
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      setError(msg);
      setAuthError(isAuthError(msg));
      setHasFetched(true);
    } finally {
      setLoading(false);
    }
  }, [jwt]);

  useEffect(() => {
    loadData();
  }, [loadData]);

  const { ref: refreshBtnRef } = useUIElement({
    id: 'lineage-refresh',
    type: 'button',
    label: 'Refresh lineage data',
    actions: ['click'],
  });
  const { ref: jwtRef } = useUIElement({
    id: 'lineage-jwt',
    type: 'input',
    label: 'Operator JWT input',
  });

  const totals = stats?.totals;
  const attributionPct =
    totals && totals.commits > 0 ? Math.round((totals.attributed / totals.commits) * 100) : 0;

  return (
    <div>
      <div className="page-header">
        <h1 className="page-title">Commit Lineage</h1>
        <div className="flex gap-2 items-center">
          {lastFetchedAt && (
            <span className="text-muted" style={{ fontSize: '0.8rem' }}>
              Updated {formatTs(lastFetchedAt)}
            </span>
          )}
          <button
            ref={refreshBtnRef as React.RefCallback<HTMLButtonElement>}
            className="btn btn-primary"
            onClick={loadData}
            disabled={loading || !hasJwt}
          >
            {loading ? 'Loading…' : 'Refresh'}
          </button>
        </div>
      </div>
      <p className="page-desc">
        Which Claude Code session produced which commit — recorded server-side by coord (merge
        orchestrator + trailer backfill). Click a session chip to see all of its commits.
      </p>

      {/* Credential card — coord's lineage reads are operator-scoped. Reuses the
          SAME JWT the Fleet tab stores, so a paste on either page serves both. */}
      <div className="card" style={{ marginBottom: '1rem' }}>
        <div className="card-header">
          <span className="card-title">Operator credential</span>
          {hasJwt && (
            <span className="badge badge-success" style={{ fontSize: '0.7rem' }}>
              JWT set
            </span>
          )}
        </div>
        <label className="text-muted" style={{ fontSize: '0.75rem', display: 'block' }}>
          Operator JWT (same token as the Fleet tab)
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
            autoComplete="new-password"
            data-form-type="other"
            data-1p-ignore
            data-lpignore="true"
            name="lineage-jwt-token"
          />
        </label>
        {!hasJwt && (
          <div className="text-muted" style={{ marginTop: '0.5rem', fontSize: '0.78rem' }}>
            Paste an operator JWT (same as the Fleet tab) to view lineage. Coord's lineage reads are
            operator-scoped and reject anonymous requests.
          </div>
        )}
      </div>

      {/* Auth error — credential present but rejected (expired/invalid/wrong scope). */}
      {error && authError && (
        <div className="card" style={{ marginBottom: '1rem', borderColor: 'var(--danger)' }}>
          <div className="card-header">
            <span className="card-title text-danger">Credential rejected</span>
          </div>
          <div
            className="text-mono text-danger"
            style={{ fontSize: '0.8rem', whiteSpace: 'pre-wrap', wordBreak: 'break-word' }}
          >
            {error}
          </div>
          <div className="text-muted" style={{ marginTop: '0.5rem', fontSize: '0.78rem' }}>
            Coord returned 401/403. The JWT is missing the operator scope, has expired, or is for a
            different backend. Paste a fresh operator JWT above (same token the Fleet tab uses).
          </div>
        </div>
      )}

      {/* Coord-unreachable / transport error — distinct from the auth case. */}
      {error && !authError && (
        <div className="card" style={{ marginBottom: '1rem', borderColor: 'var(--danger)' }}>
          <div className="card-header">
            <span className="card-title text-danger">Coord unavailable</span>
          </div>
          <div
            className="text-mono text-danger"
            style={{ fontSize: '0.8rem', whiteSpace: 'pre-wrap', wordBreak: 'break-word' }}
          >
            {error}
          </div>
          <div className="text-muted" style={{ marginTop: '0.5rem', fontSize: '0.78rem' }}>
            Set <span className="text-mono">COORD_HTTP_URL</span> or configure a coord_url in{' '}
            <span className="text-mono">~/.qontinui/profiles.json</span>. Staging coord is{' '}
            <span className="text-mono">https://coord.staging.qontinui.io</span>.
          </div>
        </div>
      )}

      {!hasJwt ? (
        <div className="card">
          <div className="text-muted" style={{ fontSize: '0.85rem' }}>
            Paste an operator JWT above to load commit lineage from coord.
          </div>
        </div>
      ) : (
        <>
      {/* Stats tiles */}
      <div className="card-grid" style={{ marginBottom: '1rem' }}>
        <div className="card">
          <div className="card-title">Commits</div>
          <div style={{ fontSize: '1.6rem', fontWeight: 600 }}>{totals?.commits ?? '—'}</div>
          <div className="text-muted" style={{ fontSize: '0.75rem' }}>
            {totals ? `${totals.attributed} attributed (${attributionPct}%)` : 'no data'}
          </div>
        </div>
        <div className="card">
          <div className="card-title">Sessions</div>
          <div style={{ fontSize: '1.6rem', fontWeight: 600 }}>{totals?.sessions ?? '—'}</div>
        </div>
        <div className="card">
          <div className="card-title">Repos</div>
          <div style={{ fontSize: '1.6rem', fontWeight: 600 }}>{totals?.repos ?? '—'}</div>
        </div>
        <div className="card">
          <div className="card-title">By source</div>
          <div className="flex gap-2" style={{ flexWrap: 'wrap', marginTop: '0.3rem' }}>
            {stats && stats.by_source.length > 0 ? (
              stats.by_source.map((s) => (
                <span
                  key={s.source}
                  className="badge"
                  style={{ fontSize: '0.7rem', background: 'var(--bg-tertiary)' }}
                  title={`${s.commits} commits`}
                >
                  {s.source}: {s.commits}
                </span>
              ))
            ) : (
              <span className="text-muted">—</span>
            )}
          </div>
        </div>
      </div>

      {/* Top sessions */}
      <div className="card" style={{ marginBottom: '1rem' }}>
        <div className="card-header">
          <span className="card-title">Top sessions</span>
        </div>
        {stats && stats.top_sessions.length > 0 ? (
          <div className="table-container" style={{ marginTop: '0.5rem' }}>
            <table>
              <thead>
                <tr>
                  <th>Session</th>
                  <th>Commits</th>
                  <th>Last commit</th>
                </tr>
              </thead>
              <tbody>
                {stats.top_sessions.map((s) => (
                  <tr key={s.agent_session_id}>
                    <td>
                      <SessionChip
                        sessionId={s.agent_session_id}
                        sessionName={s.session_name}
                        jwt={jwt}
                      />
                    </td>
                    <td>{s.commits}</td>
                    <td>{formatTs(s.last_commit_at)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : (
          <div className="text-muted" style={{ marginTop: '0.5rem' }}>
            No session attribution yet.
          </div>
        )}
      </div>

      {/* Recent commits */}
      <div className="card">
        <div className="card-header">
          <span className="card-title">Recent commits</span>
          <span className={`badge ${rows.length > 0 ? 'badge-success' : 'badge-warning'}`}>
            {rows.length}
          </span>
        </div>
        {rows.length > 0 ? (
          <div className="table-container" style={{ marginTop: '0.5rem' }}>
            <table>
              <thead>
                <tr>
                  <th>Commit</th>
                  <th>Repo</th>
                  <th>Branch</th>
                  <th>PR</th>
                  <th>Session</th>
                  <th>Source</th>
                  <th>When</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((r) => (
                  <tr key={r.commit_sha}>
                    <td className="text-mono">
                      <a
                        href={`${GITHUB}/${r.repo}/commit/${r.commit_sha}`}
                        target="_blank"
                        rel="noreferrer"
                      >
                        {shortSha(r.commit_sha)}
                      </a>
                    </td>
                    <td className="text-mono">{r.repo}</td>
                    <td className="text-mono">{r.branch ?? '—'}</td>
                    <td>{r.pr_number != null ? `#${r.pr_number}` : '—'}</td>
                    <td>
                      <SessionChip
                        sessionId={r.agent_session_id}
                        sessionName={r.session_name}
                        jwt={jwt}
                      />
                    </td>
                    <td>
                      <span
                        className="badge"
                        style={{ fontSize: '0.68rem', background: 'var(--bg-tertiary)' }}
                      >
                        {r.source}
                      </span>
                    </td>
                    <td>{formatTs(r.recorded_at)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : (
          <div className="text-muted" style={{ marginTop: '0.5rem' }}>
            {loading
              ? 'Loading…'
              : error
                ? 'Could not load lineage — see the banner above.'
                : hasFetched
                  ? 'No lineage rows. Coord is reachable but has no recorded commits yet.'
                  : 'Press Refresh to load lineage.'}
          </div>
        )}
      </div>
        </>
      )}
    </div>
  );
}
