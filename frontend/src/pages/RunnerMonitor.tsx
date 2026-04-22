import { useState, useEffect, useCallback, useRef } from 'react';
import { api, RecentCrashSummary, RunnerTaskRun, UiErrorSummary, RunnerDerivedStatus } from '../lib/api';
import { RunnerStatusBadge } from '../components/RunnerStatusBadge';

type ActionState = string | null;

/// Best-effort extraction of the runner's reported ui_error from an arbitrary
/// /health response. Tolerates older runners (no field) and mistyped payloads
/// by returning `null` whenever any required field is missing.
function extractUiError(health: Record<string, unknown> | null): UiErrorSummary | null {
  if (!health || typeof health !== 'object') return null;
  const raw = (health as { ui_error?: unknown }).ui_error;
  if (!raw || typeof raw !== 'object') return null;
  const obj = raw as Record<string, unknown>;
  const message = typeof obj.message === 'string' ? obj.message : null;
  const firstSeen = typeof obj.first_seen === 'string' ? obj.first_seen : null;
  const reportedAt = typeof obj.reported_at === 'string' ? obj.reported_at : null;
  const count = typeof obj.count === 'number' ? obj.count : null;
  if (message === null || firstSeen === null || reportedAt === null || count === null) {
    return null;
  }
  return {
    message,
    first_seen: firstSeen,
    reported_at: reportedAt,
    count,
    digest: typeof obj.digest === 'string' ? obj.digest : null,
    stack: typeof obj.stack === 'string' ? obj.stack : null,
    component_stack: typeof obj.component_stack === 'string' ? obj.component_stack : null,
  };
}

/// Best-effort extraction of the runner's `recent_crash` object. The runner
/// serializes it with camelCase keys (via `serde(rename_all="camelCase")`), so
/// the field names on the wire differ from `ui_error`. Returns null when the
/// runner hasn't seen a crash dump or when required fields are missing.
function extractRecentCrash(health: Record<string, unknown> | null): RecentCrashSummary | null {
  if (!health || typeof health !== 'object') return null;
  const raw = (health as { recent_crash?: unknown }).recent_crash;
  if (!raw || typeof raw !== 'object') return null;
  const obj = raw as Record<string, unknown>;
  const filePath = typeof obj.filePath === 'string' ? obj.filePath : null;
  const reportedAt = typeof obj.reportedAt === 'string' ? obj.reportedAt : null;
  if (filePath === null || reportedAt === null) return null;
  return {
    filePath,
    reportedAt,
    panicLocation: typeof obj.panicLocation === 'string' ? obj.panicLocation : null,
    panicMessage: typeof obj.panicMessage === 'string' ? obj.panicMessage : null,
    thread: typeof obj.thread === 'string' ? obj.thread : null,
  };
}

/// Derive a RunnerDerivedStatus for display from the runner's own /health
/// response. Prefers the runner's `derived_status` string if present
/// (Phase 3J.1); otherwise infers from `ui_error` / `recent_crash` /
/// top-level `status`.
function deriveStatus(
  health: Record<string, unknown> | null,
  fetchErrored: boolean,
): RunnerDerivedStatus {
  if (fetchErrored) return { kind: 'offline' };
  if (!health) return { kind: 'offline' };
  const uiError = extractUiError(health);
  if (uiError) return { kind: 'errored', reason: uiError.message };
  const crash = extractRecentCrash(health);
  if (crash) {
    return {
      kind: 'errored',
      reason: crash.panicMessage ?? 'runner restarted after Rust panic',
    };
  }
  const derived = (health as { derived_status?: unknown }).derived_status;
  if (typeof derived === 'string') {
    const lower = derived.toLowerCase();
    if (lower === 'healthy') return { kind: 'healthy' };
    if (lower === 'errored') return { kind: 'errored', reason: 'runner reported errored' };
  }
  const status = (health as { status?: unknown }).status;
  if (status === 'starting') return { kind: 'starting' };
  if (status === 'ok') return { kind: 'healthy' };
  return { kind: 'healthy' };
}

export default function RunnerMonitor() {
  const [runnerHealth, setRunnerHealth] = useState<Record<string, unknown> | null>(null);
  const [healthError, setHealthError] = useState<string | null>(null);
  const [taskRuns, setTaskRuns] = useState<RunnerTaskRun[]>([]);
  const [taskRunsError, setTaskRunsError] = useState<string | null>(null);
  const [resultTitle, setResultTitle] = useState<string>('');
  const [resultContent, setResultContent] = useState<string>('');
  const [busy, setBusy] = useState<ActionState>(null);
  const resultRef = useRef<HTMLPreElement>(null);

  const wrapAction = useCallback((label: string, fn: () => Promise<unknown>) => {
    return async () => {
      setBusy(label);
      try {
        const result = await fn();
        if (typeof result === 'string') {
          setResultContent(result);
        } else {
          setResultContent(JSON.stringify(result, null, 2));
        }
        setResultTitle(label);
      } catch (e) {
        setResultContent(String(e));
        setResultTitle(`${label} (error)`);
      } finally {
        setBusy(null);
      }
    };
  }, []);

  const refreshHealth = useCallback(() => {
    setBusy('Refresh Health');
    api
      .runnerHealth()
      .then((h) => {
        setRunnerHealth(h);
        setHealthError(null);
      })
      .catch((e) => {
        setRunnerHealth(null);
        setHealthError(String(e));
      })
      .finally(() => setBusy(null));
  }, []);

  const refreshTaskRuns = useCallback(() => {
    setBusy('Refresh Tasks');
    api
      .runnerTaskRunsRunning()
      .then((runs) => {
        setTaskRuns(Array.isArray(runs) ? runs : []);
        setTaskRunsError(null);
      })
      .catch((e) => {
        setTaskRuns([]);
        setTaskRunsError(String(e));
      })
      .finally(() => setBusy(null));
  }, []);

  // Auto-scroll result panel
  useEffect(() => {
    if (resultRef.current) {
      resultRef.current.scrollTop = resultRef.current.scrollHeight;
    }
  }, [resultContent]);

  const isHealthy = runnerHealth !== null && !healthError;

  return (
    <div>
      <div className="page-header">
        <h1 className="page-title">Runner Monitor</h1>
      </div>
      <p className="page-desc">Runner process health and active task runs — view workflow state, AI output, or stop tasks.</p>

      <div className="card-grid">
        {/* Runner Health */}
        <div className="card">
          <div className="card-header">
            <span className="card-title">Runner Health</span>
            <div
              className="flex gap-2"
              style={{ alignItems: 'center', flexWrap: 'wrap' }}
            >
              {runnerHealth === null && !healthError ? (
                <span className="badge badge-warning">Unknown</span>
              ) : (
                <RunnerStatusBadge
                  derivedStatus={deriveStatus(runnerHealth, !!healthError)}
                  uiError={extractUiError(runnerHealth)}
                  recentCrash={extractRecentCrash(runnerHealth)}
                  fallbackUp={isHealthy}
                />
              )}
              <button
                className="btn"
                style={{ fontSize: '0.75rem', padding: '2px 8px' }}
                disabled={busy !== null}
                onClick={refreshHealth}
              >
                {busy === 'Refresh Health' ? 'Checking...' : 'Check'}
              </button>
            </div>
          </div>
          {healthError && (
            <div
              className="text-mono text-danger"
              style={{ fontSize: '0.8rem', marginTop: '0.5rem' }}
            >
              {healthError}
            </div>
          )}
          {runnerHealth && (
            <div className="text-mono" style={{ fontSize: '0.8rem', marginTop: '0.5rem' }}>
              {Object.entries(runnerHealth).map(([key, val]) => (
                <div key={key} style={{ padding: '1px 0' }}>
                  <span className="text-muted">{key}:</span>{' '}
                  {typeof val === 'object' ? JSON.stringify(val) : String(val)}
                </div>
              ))}
            </div>
          )}
        </div>

        {/* Running Task Runs */}
        <div className="card" style={{ gridColumn: '1 / -1' }}>
          <div className="card-header">
            <span className="card-title">Running Task Runs</span>
            <div className="flex gap-2" style={{ alignItems: 'center' }}>
              <span className={`badge ${taskRuns.length > 0 ? 'badge-success' : 'badge-warning'}`}>
                {taskRuns.length} active
              </span>
              <button
                className="btn"
                style={{ fontSize: '0.75rem', padding: '2px 8px' }}
                disabled={busy !== null}
                onClick={refreshTaskRuns}
              >
                {busy === 'Refresh Tasks' ? 'Refreshing...' : 'Refresh'}
              </button>
            </div>
          </div>
          {taskRunsError && (
            <div
              className="text-mono text-danger"
              style={{ fontSize: '0.8rem', marginTop: '0.5rem' }}
            >
              {taskRunsError}
            </div>
          )}
          {taskRuns.length === 0 && !taskRunsError && (
            <div className="text-muted" style={{ marginTop: '0.5rem' }}>
              No running task runs
            </div>
          )}
          {taskRuns.map((run) => (
            <div
              key={run.id}
              style={{
                marginTop: '0.75rem',
                padding: '0.75rem',
                background: 'var(--bg-tertiary, #1a1a2e)',
                borderRadius: '6px',
              }}
            >
              <div
                className="flex justify-between"
                style={{ alignItems: 'center', marginBottom: '0.5rem' }}
              >
                <span className="text-mono" style={{ fontSize: '0.85rem' }}>
                  <strong>{run.id}</strong>
                  <span className="text-muted" style={{ marginLeft: '0.75rem' }}>
                    {run.status}
                  </span>
                </span>
              </div>
              {run.prompt && (
                <div
                  className="text-muted"
                  style={{
                    fontSize: '0.8rem',
                    marginBottom: '0.5rem',
                    maxHeight: '3em',
                    overflow: 'hidden',
                    textOverflow: 'ellipsis',
                  }}
                >
                  {String(run.prompt).slice(0, 200)}
                </div>
              )}
              <div className="flex gap-2" style={{ flexWrap: 'wrap' }}>
                <button
                  className="btn"
                  disabled={busy !== null}
                  onClick={wrapAction(`Workflow State (${run.id})`, () =>
                    api.runnerWorkflowState(run.id),
                  )}
                >
                  {busy === `Workflow State (${run.id})` ? 'Loading...' : 'Workflow State'}
                </button>
                <button
                  className="btn"
                  disabled={busy !== null}
                  onClick={wrapAction(`AI Output (${run.id})`, () => api.runnerTaskOutput(run.id))}
                >
                  {busy === `AI Output (${run.id})` ? 'Loading...' : 'AI Output'}
                </button>
                <button
                  className="btn"
                  disabled={busy !== null}
                  style={{ color: 'var(--danger, #ef4444)' }}
                  onClick={wrapAction(`Stop (${run.id})`, () => api.runnerStopTask(run.id))}
                >
                  {busy === `Stop (${run.id})` ? 'Stopping...' : 'Stop'}
                </button>
              </div>
            </div>
          ))}
        </div>
      </div>

      {/* Result Panel */}
      {resultContent && (
        <div className="card" style={{ marginTop: '1rem' }}>
          <div className="card-header">
            <span className="card-title">{resultTitle || 'Result'}</span>
            <button
              className="btn"
              style={{ fontSize: '0.75rem', padding: '2px 8px' }}
              onClick={() => {
                setResultContent('');
                setResultTitle('');
              }}
            >
              Clear
            </button>
          </div>
          <pre
            ref={resultRef}
            className="text-mono"
            style={{
              marginTop: '0.5rem',
              padding: '0.75rem',
              background: 'var(--bg-tertiary, #1a1a2e)',
              borderRadius: '6px',
              maxHeight: '500px',
              overflow: 'auto',
              whiteSpace: 'pre-wrap',
              wordBreak: 'break-word',
              fontSize: '0.8rem',
              lineHeight: '1.4',
            }}
          >
            {resultContent}
          </pre>
        </div>
      )}
    </div>
  );
}
