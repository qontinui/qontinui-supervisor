import { useState, useEffect, useCallback, useRef } from 'react';
import { api, RunnerTaskRun } from '../lib/api';

type ActionState = string | null;

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

      <div className="card-grid">
        {/* Runner Health */}
        <div className="card">
          <div className="card-header">
            <span className="card-title">Runner Health</span>
            <div className="flex gap-2" style={{ alignItems: 'center' }}>
              <span
                className={`badge ${isHealthy ? 'badge-success' : runnerHealth === null && !healthError ? 'badge-warning' : 'badge-danger'}`}
              >
                {isHealthy
                  ? 'Healthy'
                  : runnerHealth === null && !healthError
                    ? 'Unknown'
                    : 'Unreachable'}
              </span>
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
