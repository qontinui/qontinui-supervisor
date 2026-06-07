import { useCallback, useEffect, useState } from 'react';
import { useUIElement } from '@qontinui/ui-bridge/react';
import { api, LineageRow, LineageStats } from '../lib/api';
import { SessionChip } from '../components/SessionChip';

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

export default function Lineage() {
  const [stats, setStats] = useState<LineageStats | null>(null);
  const [rows, setRows] = useState<LineageRow[]>(EMPTY_ROWS);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [lastFetchedAt, setLastFetchedAt] = useState<string | null>(null);

  const loadData = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [s, r] = await Promise.all([
        api.lineageStats().catch(() => null),
        api.lineageRecent(100).catch(() => EMPTY_ROWS),
      ]);
      setStats(s);
      setRows(Array.isArray(r) && r.length > 0 ? r : EMPTY_ROWS);
      setLastFetchedAt(new Date().toISOString());
      // Surface a hard error only when BOTH calls failed (coord unreachable).
      if (s === null && (!Array.isArray(r) || r.length === 0)) {
        // Probe once for a real error message rather than silently empty.
        await api.lineageStats();
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    loadData();
  }, [loadData]);

  const { ref: refreshBtnRef } = useUIElement({
    id: 'lineage-refresh',
    type: 'button',
    label: 'Refresh lineage data',
    actions: ['click'],
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
            disabled={loading}
          >
            {loading ? 'Loading…' : 'Refresh'}
          </button>
        </div>
      </div>
      <p className="page-desc">
        Which Claude Code session produced which commit — recorded server-side by coord (merge
        orchestrator + trailer backfill). Click a session chip to see all of its commits.
      </p>

      {error && (
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
                      <SessionChip sessionId={s.agent_session_id} sessionName={s.session_name} />
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
                      <SessionChip sessionId={r.agent_session_id} sessionName={r.session_name} />
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
            {loading ? 'Loading…' : 'No lineage rows. Coord may have no recorded commits yet.'}
          </div>
        )}
      </div>
    </div>
  );
}
