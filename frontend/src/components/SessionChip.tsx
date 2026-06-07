import { useCallback, useEffect, useState } from 'react';
import { api, LineageRow } from '../lib/api';

// Stable module-level empty array. Runtime loaders / hooks that return arrays
// MUST hand back a stable reference for the empty case — a fresh `[]` literal
// per render breaks identity-memoing consumers and triggers infinite
// re-render loops (memory: stable-empty-refs).
const EMPTY_ROWS: LineageRow[] = [];

const GITHUB = 'https://github.com';

/** Short 7-char form of a commit SHA. */
function shortSha(sha: string): string {
  return sha.slice(0, 7);
}

/** Human label for a session: its name, else the first 8 of the uuid. */
function sessionLabel(name: string | null | undefined, id: string | null | undefined): string {
  if (name && name.trim()) return name.trim();
  if (id && id.trim()) return id.trim().slice(0, 8);
  return 'unattributed';
}

function formatTs(iso: string | null): string {
  if (!iso) return '—';
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString();
}

interface SessionDrawerProps {
  sessionId: string;
  sessionName: string | null;
  onClose: () => void;
}

/**
 * Slide-in drawer listing every commit a session produced. Fetches
 * `/lineage/sessions/{id}/commits` lazily on open.
 */
function SessionDrawer({ sessionId, sessionName, onClose }: SessionDrawerProps) {
  const [rows, setRows] = useState<LineageRow[]>(EMPTY_ROWS);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const result = await api.lineageSessionCommits(sessionId);
      setRows(Array.isArray(result) && result.length > 0 ? result : EMPTY_ROWS);
    } catch (e) {
      setRows(EMPTY_ROWS);
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [sessionId]);

  useEffect(() => {
    load();
  }, [load]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label={`Commits for session ${sessionLabel(sessionName, sessionId)}`}
      style={{
        position: 'fixed',
        inset: 0,
        background: 'rgba(0,0,0,0.45)',
        display: 'flex',
        justifyContent: 'flex-end',
        zIndex: 1000,
      }}
      onClick={onClose}
    >
      <div
        style={{
          width: 'min(560px, 92vw)',
          height: '100%',
          background: 'var(--bg-secondary, #1a1a1a)',
          borderLeft: '1px solid var(--border, #333)',
          padding: '1rem 1.25rem',
          overflowY: 'auto',
          boxShadow: '-4px 0 24px rgba(0,0,0,0.4)',
        }}
        onClick={(e) => e.stopPropagation()}
      >
        <div
          style={{
            display: 'flex',
            justifyContent: 'space-between',
            alignItems: 'flex-start',
            marginBottom: '0.75rem',
          }}
        >
          <div>
            <div className="card-title">{sessionLabel(sessionName, sessionId)}</div>
            <div
              className="text-muted text-mono"
              style={{ fontSize: '0.7rem', marginTop: '0.2rem' }}
            >
              {sessionId}
            </div>
          </div>
          <button className="btn" onClick={onClose} aria-label="Close drawer">
            Close
          </button>
        </div>

        {loading && <div className="text-muted">Loading commits…</div>}
        {error && (
          <div
            className="text-mono text-danger"
            style={{ fontSize: '0.78rem', whiteSpace: 'pre-wrap', wordBreak: 'break-word' }}
          >
            {error}
          </div>
        )}
        {!loading && !error && rows.length === 0 && (
          <div className="text-muted">No commits attributed to this session.</div>
        )}
        {rows.length > 0 && (
          <div className="table-container">
            <table>
              <thead>
                <tr>
                  <th>Commit</th>
                  <th>Repo</th>
                  <th>Branch</th>
                  <th>PR</th>
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
                    <td>{formatTs(r.recorded_at)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  );
}

export interface SessionChipProps {
  /** The Claude Code agent session UUID. */
  sessionId: string | null | undefined;
  /** Human session name; falls back to first 8 of the uuid. */
  sessionName?: string | null;
}

/**
 * Reusable chip rendering a session's name (or short uuid). Clicking opens a
 * drawer listing that session's commits. Exported so future pages can reuse it.
 * Renders an inert "unattributed" pill when no session id is present.
 */
export function SessionChip({ sessionId, sessionName }: SessionChipProps) {
  const [open, setOpen] = useState(false);

  if (!sessionId || !sessionId.trim()) {
    return (
      <span className="badge badge-warning" style={{ fontSize: '0.7rem' }}>
        unattributed
      </span>
    );
  }

  return (
    <>
      <span
        className="badge badge-clickable"
        role="button"
        tabIndex={0}
        title={`View commits for session ${sessionId}`}
        style={{
          fontSize: '0.7rem',
          background: 'var(--bg-tertiary, #2a2a2a)',
          color: 'var(--text-secondary, #ccc)',
          cursor: 'pointer',
        }}
        onClick={() => setOpen(true)}
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            setOpen(true);
          }
        }}
      >
        {sessionLabel(sessionName, sessionId)}
      </span>
      {open && (
        <SessionDrawer
          sessionId={sessionId}
          sessionName={sessionName ?? null}
          onClose={() => setOpen(false)}
        />
      )}
    </>
  );
}

export default SessionChip;
