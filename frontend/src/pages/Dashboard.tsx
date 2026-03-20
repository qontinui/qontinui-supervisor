import React, { useState, useEffect, useCallback, useRef } from 'react';
import { api, HealthResponse, DevStartResponse, AiModelInfo, WorkflowLoopStatus } from '../lib/api';
import { ErrorBoundary } from '../components/ErrorBoundary';
import { ToastContainer, addToast } from '../components/Toast';
import { ConfirmDialog, confirm } from '../components/ConfirmDialog';
import { SmallBtn } from '../components/SmallBtn';
import { StatusDot } from '../components/StatusDot';
import { useSSE } from '../hooks/useSSE';

// ─── AI Session Panel ────────────────────────────────────────────────────────

type AiPanelState = 'idle' | 'running' | 'completed';

function AiSessionPanel({
  provider,
  model,
  onStop,
  onDone,
}: {
  provider: string;
  model: string;
  onStop: () => Promise<void>;
  onDone: () => void;
}) {
  const [lines, setLines] = useState<string[]>([]);
  const [phase, setPhase] = useState<AiPanelState>('running');
  const [stopping, setStopping] = useState(false);
  const [expanded, setExpanded] = useState(true);
  const outputRef = useRef<HTMLDivElement>(null);
  const lastEventRef = useRef(Date.now());

  // SSE for AI output with auto-reconnect
  useSSE<{ stream: string; line: string }[]>(
    '/ai/output/stream',
    'ai_output',
    (entries) => {
      lastEventRef.current = Date.now();
      const newLines = entries.map((e) => e.line);
      if (newLines.length > 0) {
        setLines((prev) => {
          const combined = [...prev, ...newLines];
          return combined.length > 200 ? combined.slice(-200) : combined;
        });
      }
    },
    phase === 'running',
  );

  // Check for completion: if no ai_output events for 5s, do a single health check
  useEffect(() => {
    if (phase !== 'running') return;
    const checkDone = setInterval(async () => {
      if (Date.now() - lastEventRef.current > 5000) {
        try {
          const h = await api.health();
          if (!h.ai.ai_running) {
            setPhase('completed');
          }
        } catch {
          /* ignore */
        }
      }
    }, 5000);
    return () => clearInterval(checkDone);
  }, [phase]);

  // Auto-scroll output
  useEffect(() => {
    if (outputRef.current && expanded) {
      outputRef.current.scrollTop = outputRef.current.scrollHeight;
    }
  }, [lines, expanded]);

  const handleStop = async () => {
    setStopping(true);
    await onStop();
    setStopping(false);
    setPhase('completed');
  };

  const borderColor = phase === 'completed' ? 'rgba(34,197,94,0.3)' : 'rgba(99,102,241,0.3)';
  const bgColor = phase === 'completed' ? 'rgba(34,197,94,0.06)' : 'rgba(99,102,241,0.06)';

  return (
    <div
      style={{
        marginBottom: '1rem',
        border: `1px solid ${borderColor}`,
        borderRadius: 6,
        background: bgColor,
        overflow: 'hidden',
      }}
    >
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          gap: '0.75rem',
          padding: '0.5rem 1rem',
          fontSize: '0.8rem',
        }}
      >
        {phase === 'running' && (
          <span
            style={{
              display: 'inline-block',
              width: 8,
              height: 8,
              borderRadius: '50%',
              background: 'var(--accent)',
              animation: 'pulse 1.5s ease-in-out infinite',
              flexShrink: 0,
            }}
          />
        )}
        {phase === 'completed' && (
          <span
            style={{
              display: 'inline-block',
              width: 8,
              height: 8,
              borderRadius: '50%',
              background: 'var(--success)',
              flexShrink: 0,
            }}
          />
        )}
        <span style={{ flex: 1 }}>
          {phase === 'running' ? `AI debug running (${provider}/${model})` : 'AI debug completed'}
          {lines.length > 0 && <span className="text-muted"> — {lines.length} lines</span>}
        </span>
        <button
          onClick={() => setExpanded((v) => !v)}
          style={{
            background: 'none',
            border: 'none',
            color: 'var(--text-muted)',
            cursor: 'pointer',
            fontSize: '0.75rem',
            padding: '0 4px',
          }}
        >
          {expanded ? 'collapse' : 'expand'}
        </button>
        {phase === 'running' && (
          <SmallBtn
            label="Stop"
            activeLabel="Stopping…"
            onClick={handleStop}
            busy={stopping ? 'stop' : null}
            busyKey="stop"
            variant="danger"
          />
        )}
        {phase === 'completed' && (
          <SmallBtn label="Dismiss" activeLabel="" onClick={onDone} busy={null} />
        )}
      </div>

      {expanded && lines.length > 0 && (
        <div
          ref={outputRef}
          style={{
            maxHeight: 200,
            overflowY: 'auto',
            padding: '0.4rem 1rem',
            borderTop: `1px solid ${borderColor}`,
            fontFamily: 'var(--font-mono)',
            fontSize: '0.7rem',
            lineHeight: 1.5,
            whiteSpace: 'pre-wrap',
            wordBreak: 'break-word',
          }}
        >
          {lines.map((line, i) => (
            <div key={i}>{line}</div>
          ))}
        </div>
      )}
    </div>
  );
}

// ─── AI Provider Selector ────────────────────────────────────────────────────

function AiProviderSelector({ current }: { current: HealthResponse['ai'] }) {
  const [models, setModels] = useState<AiModelInfo[]>([]);
  const [changing, setChanging] = useState(false);

  useEffect(() => {
    api
      .aiModels()
      .then((r) => setModels(r.models))
      .catch(() => {});
  }, []);

  const handleChange = async (e: React.ChangeEvent<HTMLSelectElement>) => {
    const [provider, model] = e.target.value.split('/');
    if (!provider || !model) return;
    setChanging(true);
    try {
      await api.aiSetProvider(provider, model);
      addToast(`Switched to ${provider}/${model}`, 'success');
    } catch {
      addToast('Failed to change AI provider', 'error');
    }
    setChanging(false);
  };

  const currentValue = `${current.ai_provider}/${current.ai_model}`;

  return (
    <select
      className="provider-select"
      value={currentValue}
      onChange={handleChange}
      disabled={changing || current.ai_running}
      title={current.ai_running ? 'Cannot change while AI is running' : 'Select AI provider/model'}
    >
      {models.length === 0 && (
        <option value={currentValue}>
          {current.ai_provider}/{current.ai_model}
        </option>
      )}
      {models.map((m) => (
        <option key={`${m.provider}/${m.key}`} value={`${m.provider}/${m.key}`}>
          {m.display_name}
        </option>
      ))}
    </select>
  );
}

// ─── Log Viewer ──────────────────────────────────────────────────────────────

interface LogLine {
  timestamp: string;
  level: string;
  source: string;
  message: string;
}

function LogViewer() {
  const [lines, setLines] = useState<LogLine[]>([]);
  const [expanded, setExpanded] = useState(false);
  const [paused, setPaused] = useState(false);
  const [filter, setFilter] = useState('all');
  const viewerRef = useRef<HTMLDivElement>(null);
  const pausedRef = useRef(false);
  pausedRef.current = paused;

  useSSE<LogLine>(
    '/logs/stream',
    'log',
    (entry) => {
      if (!pausedRef.current) {
        setLines((prev) => {
          const next = [...prev, entry];
          return next.length > 300 ? next.slice(-300) : next;
        });
      }
    },
    expanded,
  );

  useEffect(() => {
    if (viewerRef.current && !paused) {
      viewerRef.current.scrollTop = viewerRef.current.scrollHeight;
    }
  }, [lines, paused]);

  const filtered =
    filter === 'all' ? lines : lines.filter((l) => l.source === filter || l.level === filter);
  const sources = [...new Set(lines.map((l) => l.source))].sort();

  const levelClass = (level: string) => {
    switch (level.toLowerCase()) {
      case 'error':
        return 'log-line-error';
      case 'warn':
      case 'warning':
        return 'log-line-warn';
      case 'debug':
      case 'trace':
        return 'log-line-debug';
      default:
        return 'log-line-info';
    }
  };

  return (
    <div className="card" style={{ marginBottom: '1rem' }}>
      <div className="card-header" style={{ marginBottom: expanded ? '0.5rem' : 0 }}>
        <span className="card-title">Logs</span>
        <div className="flex gap-2 items-center">
          {expanded && (
            <>
              <select
                className="log-filter"
                value={filter}
                onChange={(e) => setFilter(e.target.value)}
              >
                <option value="all">All sources</option>
                {sources.map((s) => (
                  <option key={s} value={s}>
                    {s}
                  </option>
                ))}
                <option value="error">Errors only</option>
                <option value="warn">Warnings only</option>
              </select>
              <SmallBtn
                label={paused ? 'Resume' : 'Pause'}
                activeLabel=""
                onClick={() => setPaused((v) => !v)}
                busy={null}
              />
              <SmallBtn label="Clear" activeLabel="" onClick={() => setLines([])} busy={null} />
            </>
          )}
          <button
            onClick={() => setExpanded((v) => !v)}
            style={{
              background: 'none',
              border: 'none',
              color: 'var(--text-muted)',
              cursor: 'pointer',
              fontSize: '0.75rem',
              padding: '0 4px',
            }}
          >
            {expanded ? 'collapse' : 'expand'}
          </button>
        </div>
      </div>
      {expanded && (
        <div ref={viewerRef} className="log-viewer">
          {filtered.length === 0 && (
            <div className="text-muted" style={{ padding: '1rem', textAlign: 'center' }}>
              Waiting for log events...
            </div>
          )}
          {filtered.map((l, i) => (
            <div key={i} className={`log-line ${levelClass(l.level)}`}>
              <span className="text-muted">{new Date(l.timestamp).toLocaleTimeString()} </span>[
              {l.source}] {l.message}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

// ─── Workflow Loop Panel ─────────────────────────────────────────────────────

function WorkflowLoopPanel() {
  const [status, setStatus] = useState<WorkflowLoopStatus | null>(null);
  const [expanded, setExpanded] = useState(false);
  const [busy, setBusy] = useState(false);

  // Fetch status on mount and when expanded
  useEffect(() => {
    api
      .wlStatus()
      .then(setStatus)
      .catch(() => {});
  }, []);

  // Subscribe to SSE when loop is running
  useSSE<WorkflowLoopStatus>(
    '/workflow-loop/stream',
    'status',
    (s) => setStatus(s),
    !!status?.running,
  );

  const handleStop = async () => {
    setBusy(true);
    try {
      await api.wlStop();
      addToast('Workflow loop stopping...', 'info');
      setTimeout(
        () =>
          api
            .wlStatus()
            .then(setStatus)
            .catch(() => {}),
        1500,
      );
    } catch {
      addToast('Failed to stop workflow loop', 'error');
    }
    setBusy(false);
  };

  if (!status) return null;

  const running = status.running;
  const phase = status.phase || 'idle';

  return (
    <div className="card" style={{ marginBottom: '1rem' }}>
      <div className="card-header" style={{ marginBottom: expanded ? '0.5rem' : 0 }}>
        <span className="card-title">
          Workflow Loop
          {running && (
            <span
              style={{
                display: 'inline-block',
                width: 6,
                height: 6,
                borderRadius: '50%',
                background: 'var(--success)',
                marginLeft: 8,
                animation: 'pulse 1.5s ease-in-out infinite',
              }}
            />
          )}
        </span>
        <div className="flex gap-2 items-center">
          {running && (
            <>
              <span className="wl-phase">{phase}</span>
              <span className="text-muted" style={{ fontSize: '0.75rem' }}>
                iter {status.current_iteration}/{status.config?.max_iterations ?? '?'}
              </span>
              <SmallBtn
                label="Stop"
                activeLabel="Stopping…"
                onClick={handleStop}
                busy={busy ? 'stop' : null}
                busyKey="stop"
                variant="danger"
              />
            </>
          )}
          {!running && (
            <span className="text-muted" style={{ fontSize: '0.75rem' }}>
              idle
            </span>
          )}
          <button
            onClick={() => setExpanded((v) => !v)}
            style={{
              background: 'none',
              border: 'none',
              color: 'var(--text-muted)',
              cursor: 'pointer',
              fontSize: '0.75rem',
              padding: '0 4px',
            }}
          >
            {expanded ? 'collapse' : 'expand'}
          </button>
        </div>
      </div>
      {expanded && (
        <div style={{ fontSize: '0.8rem' }}>
          <table style={{ width: '100%' }}>
            <tbody>
              <tr>
                <td className="text-muted" style={{ width: 140 }}>
                  Status
                </td>
                <td>{running ? <span className="text-success">Running</span> : 'Idle'}</td>
              </tr>
              <tr>
                <td className="text-muted">Phase</td>
                <td>{phase}</td>
              </tr>
              <tr>
                <td className="text-muted">Iteration</td>
                <td>
                  {status.current_iteration} / {status.config?.max_iterations ?? '—'}
                </td>
              </tr>
              {status.error && (
                <tr>
                  <td className="text-muted">Error</td>
                  <td className="text-danger">{status.error}</td>
                </tr>
              )}
              {status.config?.workflow_id && (
                <tr>
                  <td className="text-muted">Workflow</td>
                  <td className="text-mono" style={{ fontSize: '0.7rem' }}>
                    {status.config.workflow_id}
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

// ─── Constants ───────────────────────────────────────────────────────────────

const SERVICE_LOG_MAP: Record<string, string> = {
  Runner: 'runner-tauri',
  Backend: 'backend-err',
  Frontend: 'frontend-err',
};

// Actions that require user confirmation before executing
const DESTRUCTIVE_ACTIONS = new Set([
  'Stop All',
  'Fresh',
  'Clean',
  'backend-stop',
  'frontend-stop',
  'runner-stop',
  'expo-stop',
  'Stop Docker',
]);

interface ActionDef {
  key: string;
  display: string;
  activeLabel: string;
  service: string;
  fn: () => Promise<unknown>;
}

interface RowDef {
  name: string;
  port: string;
  up: boolean;
  actions?: ActionDef[];
}

interface ServiceError {
  service: string;
  stderr: string;
  stdout: string;
  action: string;
}

type ActionState = string | null;

interface StatusData {
  health: HealthResponse | null;
  services: { name: string; port: number; available: boolean }[];
  expo: Record<string, unknown> | null;
}

// ─── Runner Instances Panel ──────────────────────────────────────────────────

interface RunnerInstance {
  id: string;
  name: string;
  port: number;
  is_primary: boolean;
  protected: boolean;
  running: boolean;
  pid: number | null;
  api_responding: boolean;
}

function RunnerInstancesPanel() {
  const [runners, setRunners] = useState<RunnerInstance[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [showAdd, setShowAdd] = useState(false);
  const [newName, setNewName] = useState('');
  const [newPort, setNewPort] = useState('');

  const refresh = useCallback(async () => {
    try {
      const list = await api.listRunners();
      setRunners(list as RunnerInstance[]);
    } catch {
      /* may not be available */
    }
  }, []);

  useEffect(() => {
    refresh();
    const interval = setInterval(refresh, 5000);
    return () => clearInterval(interval);
  }, [refresh]);

  const doAction = async (key: string, fn: () => Promise<unknown>) => {
    setBusy(key);
    try {
      await fn();
      addToast(`${key} succeeded`, 'info');
    } catch (e) {
      addToast(`${key} failed: ${e instanceof Error ? e.message : 'unknown'}`, 'error');
    }
    setBusy(null);
    refresh();
  };

  const handleSpawn = async () => {
    const name = newName.trim();
    const port = parseInt(newPort);
    if (!name || !port || port < 1024) {
      addToast('Enter a valid name and port (>= 1024)', 'error');
      return;
    }
    setBusy('spawn');
    try {
      // Spawn via runner API (creates process), then register with supervisor
      await api.spawnInstance(name, port);
      addToast(`Instance "${name}" spawned on port ${port}`, 'info');
      setNewName('');
      setNewPort('');
      setShowAdd(false);
    } catch (e) {
      // Fallback: just add to supervisor config
      try {
        await api.addRunner(name, port);
        addToast(`Runner "${name}" added (start it manually)`, 'info');
        setNewName('');
        setNewPort('');
        setShowAdd(false);
      } catch (e2) {
        addToast(`Failed: ${e2 instanceof Error ? e2.message : 'unknown'}`, 'error');
      }
    }
    setBusy(null);
    refresh();
  };

  const nonPrimary = runners.filter((r) => !r.is_primary);

  return (
    <div className="card" style={{ marginBottom: '1rem' }}>
      <div className="card-header" style={{ marginBottom: '0.5rem', display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
        <span className="card-title">Runner Instances</span>
        <button
          className="btn"
          style={{ padding: '0.15rem 0.5rem', fontSize: '0.7rem' }}
          onClick={() => setShowAdd(!showAdd)}
        >
          {showAdd ? 'Cancel' : '+ New'}
        </button>
      </div>

      {showAdd && (
        <div style={{ display: 'flex', gap: '0.4rem', marginBottom: '0.5rem', alignItems: 'center' }}>
          <input
            type="text"
            value={newName}
            onChange={(e) => setNewName(e.target.value)}
            placeholder="Name"
            style={{
              padding: '0.2rem 0.4rem',
              fontSize: '0.75rem',
              background: 'var(--bg-secondary)',
              border: '1px solid var(--border)',
              borderRadius: 3,
              color: 'var(--text)',
              width: 120,
            }}
          />
          <input
            type="number"
            value={newPort}
            onChange={(e) => setNewPort(e.target.value)}
            placeholder="Port"
            style={{
              padding: '0.2rem 0.4rem',
              fontSize: '0.75rem',
              background: 'var(--bg-secondary)',
              border: '1px solid var(--border)',
              borderRadius: 3,
              color: 'var(--text)',
              width: 70,
            }}
          />
          <button
            className="btn"
            style={{ padding: '0.2rem 0.5rem', fontSize: '0.7rem' }}
            disabled={busy !== null}
            onClick={handleSpawn}
          >
            {busy === 'spawn' ? 'Spawning...' : 'Spawn'}
          </button>
        </div>
      )}

      {nonPrimary.length === 0 && !showAdd && (
        <div style={{ fontSize: '0.75rem', color: 'var(--text-muted)', padding: '0.25rem 0' }}>
          No secondary instances. Click "+ New" to spawn one.
        </div>
      )}

      {nonPrimary.length > 0 && (
        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th>Name</th>
                <th style={{ width: 60 }}>Port</th>
                <th style={{ width: 70 }}>Status</th>
                <th>Actions</th>
              </tr>
            </thead>
            <tbody>
              {nonPrimary.map((r) => (
                <tr key={r.id}>
                  <td style={{ fontWeight: 500, fontSize: '0.8rem' }}>
                    {r.name}
                    {r.protected && (
                      <span
                        style={{
                          marginLeft: 6,
                          padding: '1px 4px',
                          background: 'rgba(34,197,94,0.15)',
                          border: '1px solid rgba(34,197,94,0.4)',
                          borderRadius: 3,
                          fontSize: '0.6rem',
                          fontWeight: 600,
                          color: '#22c55e',
                        }}
                      >
                        PROTECTED
                      </span>
                    )}
                  </td>
                  <td style={{ fontSize: '0.75rem', fontFamily: 'var(--font-mono)' }}>{r.port}</td>
                  <td>
                    <StatusDot up={r.running} />
                    <span
                      className={r.running ? 'text-success' : 'text-danger'}
                      style={{ fontSize: '0.7rem' }}
                    >
                      {r.running ? 'UP' : 'DOWN'}
                    </span>
                  </td>
                  <td>
                    <div className="flex gap-2">
                      {!r.running && (
                        <button
                          className="btn"
                          style={{ padding: '0.15rem 0.4rem', fontSize: '0.7rem' }}
                          disabled={busy !== null}
                          onClick={() => doAction(`Start ${r.name}`, () => api.startRunner(r.id))}
                        >
                          {busy === `Start ${r.name}` ? 'Starting...' : 'Start'}
                        </button>
                      )}
                      {r.running && (
                        <button
                          className="btn"
                          style={{ padding: '0.15rem 0.4rem', fontSize: '0.7rem' }}
                          disabled={busy !== null}
                          onClick={() => doAction(`Stop ${r.name}`, () => api.stopRunner(r.id))}
                        >
                          {busy === `Stop ${r.name}` ? 'Stopping...' : 'Stop'}
                        </button>
                      )}
                      <button
                        className="btn"
                        style={{
                          padding: '0.15rem 0.4rem',
                          fontSize: '0.7rem',
                          color: r.protected ? '#22c55e' : undefined,
                          borderColor: r.protected ? 'rgba(34,197,94,0.4)' : undefined,
                        }}
                        disabled={busy !== null}
                        onClick={() =>
                          doAction(
                            `${r.protected ? 'Unprotect' : 'Protect'} ${r.name}`,
                            () => api.protectRunner(r.id, !r.protected),
                          )
                        }
                      >
                        {r.protected ? 'Unprotect' : 'Protect'}
                      </button>
                      {!r.running && (
                        <button
                          className="btn"
                          style={{
                            padding: '0.15rem 0.4rem',
                            fontSize: '0.7rem',
                            color: 'var(--danger)',
                            borderColor: 'var(--danger)',
                          }}
                          disabled={busy !== null}
                          onClick={() =>
                            doAction(`Remove ${r.name}`, async () => {
                              await confirm('Remove runner', `Remove "${r.name}" from the supervisor?`);
                              await api.removeRunner(r.id);
                            })
                          }
                        >
                          Remove
                        </button>
                      )}
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

// ─── Main Dashboard ──────────────────────────────────────────────────────────

function DashboardInner() {
  const [data, setData] = useState<StatusData>({
    health: null,
    services: [],
    expo: null,
  });
  const [busy, setBusy] = useState<ActionState>(null);
  const [lastRefresh, setLastRefresh] = useState<Date | null>(null);
  const [errors, setErrors] = useState<Map<string, ServiceError>>(new Map());
  const [aiFixBusy, setAiFixBusy] = useState<string | null>(null);
  const [fixAndRebuildBusy, setFixAndRebuildBusy] = useState(false);
  const [showAiPanel, setShowAiPanel] = useState(false);
  const [runnerProtection, setRunnerProtection] = useState<Map<string, boolean>>(new Map());
  const mountedRef = useRef(true);
  const actionGuardRef = useRef(false); // Race condition guard

  // One-time fetch for service port status and expo
  const refreshPorts = useCallback(async () => {
    const [devStatus, expo] = await Promise.allSettled([api.devStartStatus(), api.expoStatus()]);
    if (!mountedRef.current) return;
    setData((prev) => ({
      ...prev,
      services: devStatus.status === 'fulfilled' ? (devStatus.value.services ?? []) : prev.services,
      expo: expo.status === 'fulfilled' ? expo.value : prev.expo,
    }));
  }, []);

  // Full refresh (health + ports)
  const refresh = useCallback(async () => {
    const [health, devStatus, expo] = await Promise.allSettled([
      api.health(),
      api.devStartStatus(),
      api.expoStatus(),
    ]);
    if (!mountedRef.current) return;
    setData({
      health: health.status === 'fulfilled' ? health.value : null,
      services: devStatus.status === 'fulfilled' ? (devStatus.value.services ?? []) : [],
      expo: expo.status === 'fulfilled' ? expo.value : null,
    });
    setLastRefresh(new Date());
  }, []);

  // Fetch runner protection status
  const refreshRunners = useCallback(async () => {
    try {
      const runners = await api.listRunners();
      if (!mountedRef.current) return;
      setRunnerProtection(new Map(runners.map((r) => [r.id, r.protected])));
    } catch {
      /* runners endpoint may not be available */
    }
  }, []);

  // Initial fetch
  useEffect(() => {
    mountedRef.current = true;
    refresh();
    refreshRunners();
    return () => {
      mountedRef.current = false;
    };
  }, [refresh, refreshRunners]);

  // Subscribe to health SSE — replaces polling
  useSSE<HealthResponse>('/health/stream', 'health', (health) => {
    if (!mountedRef.current) return;
    setData((prev) => ({ ...prev, health }));
    setLastRefresh(new Date());
  });

  // Show AI panel when a session starts
  const ai = data.health?.ai;
  useEffect(() => {
    if (ai?.ai_running) {
      setShowAiPanel(true);
    }
  }, [ai?.ai_running]);

  // Run an action, detect failures, record errors, show toasts
  const doAction = useCallback(
    (key: string, service: string, fn: () => Promise<unknown>) => {
      return async () => {
        // Race condition guard
        if (actionGuardRef.current) return;
        actionGuardRef.current = true;

        // Confirm destructive actions
        if (DESTRUCTIVE_ACTIONS.has(key)) {
          const ok = await confirm(
            `Confirm: ${key}`,
            `Are you sure you want to run "${key}"? This may disrupt running services.`,
          );
          if (!ok) {
            actionGuardRef.current = false;
            return;
          }
        }

        setBusy(key);
        try {
          const result = await fn();
          const resp = result as DevStartResponse | undefined;
          if (
            resp &&
            typeof resp.status === 'string' &&
            (resp.status === 'error' || resp.status === 'timeout')
          ) {
            setErrors((prev) => {
              const next = new Map(prev);
              next.set(service, {
                service,
                stderr: resp.stderr || '',
                stdout: resp.stdout || '',
                action: key,
              });
              return next;
            });
            addToast(`${service}: action failed`, 'error');
          } else {
            setErrors((prev) => {
              if (!prev.has(service)) return prev;
              const next = new Map(prev);
              next.delete(service);
              return next;
            });
            addToast(`${service}: ${key} completed`, 'success');
          }
        } catch {
          setErrors((prev) => {
            const next = new Map(prev);
            next.set(service, {
              service,
              stderr: 'Request failed',
              stdout: '',
              action: key,
            });
            return next;
          });
          addToast(`${service}: request failed`, 'error');
        }
        setBusy(null);
        actionGuardRef.current = false;
        setTimeout(refreshPorts, 1500);
      };
    },
    [refreshPorts],
  );

  // Trigger AI debug with service-specific context
  const triggerAiFix = useCallback(
    async (service: string) => {
      if (ai?.ai_running) return;
      setAiFixBusy(service);
      try {
        const err = errors.get(service);
        const parts: string[] = [
          err
            ? `Service "${service}" failed to start/load.`
            : `Service "${service}" is down and not responding on its expected port.`,
        ];

        if (err?.stderr) parts.push(`\nStderr:\n${err.stderr}`);
        if (err?.stdout) parts.push(`\nStdout:\n${err.stdout}`);

        const logType = SERVICE_LOG_MAP[service];
        if (logType) {
          try {
            const log = await api.logFile(logType, 80);
            if (log.content?.trim()) {
              parts.push(`\nRecent ${logType} log (last 80 lines):\n${log.content}`);
            }
          } catch {
            /* log may not exist */
          }
        }

        parts.push('\nPlease diagnose the root cause and fix the issue.');

        await api.aiDebug(parts.join('\n'));
        setShowAiPanel(true);
        addToast('AI debug session started', 'info');
      } catch {
        addToast('Failed to start AI debug (cooldown or already running)', 'error');
      }
      setAiFixBusy(null);
    },
    [errors, ai?.ai_running],
  );

  // Trigger AI fix then rebuild the runner
  const triggerFixAndRebuild = useCallback(async () => {
    if (ai?.ai_running || fixAndRebuildBusy) return;
    setFixAndRebuildBusy(true);
    setShowAiPanel(true);
    try {
      const err = errors.get('Runner');
      const parts: string[] = [
        err
          ? 'Service "Runner" failed to start/load.'
          : 'Service "Runner" is down and not responding on its expected port.',
      ];

      if (err?.stderr) parts.push(`\nStderr:\n${err.stderr}`);
      if (err?.stdout) parts.push(`\nStdout:\n${err.stdout}`);

      const logType = SERVICE_LOG_MAP['Runner'];
      if (logType) {
        try {
          const log = await api.logFile(logType, 80);
          if (log.content?.trim()) {
            parts.push(`\nRecent ${logType} log (last 80 lines):\n${log.content}`);
          }
        } catch {
          /* log may not exist */
        }
      }

      parts.push('\nPlease diagnose the root cause and fix the issue.');

      await api.runnerFixAndRebuild(parts.join('\n'));
      addToast('Fix & Rebuild completed', 'info');
    } catch (e) {
      addToast(`Fix & Rebuild failed: ${e instanceof Error ? e.message : 'unknown error'}`, 'error');
    }
    setFixAndRebuildBusy(false);
    refreshPorts();
  }, [errors, ai?.ai_running, fixAndRebuildBusy, refreshPorts]);

  const clearError = useCallback((service: string) => {
    setErrors((prev) => {
      if (!prev.has(service)) return prev;
      const next = new Map(prev);
      next.delete(service);
      return next;
    });
  }, []);

  const runner = data.health?.runner;
  const watchdog = data.health?.watchdog;
  const build = data.health?.build;
  const expo = data.health?.expo;

  // Surface build errors on the Runner row
  useEffect(() => {
    if (build?.error_detected && build.last_error) {
      setErrors((prev) => {
        if (prev.has('Runner') && prev.get('Runner')!.action === 'build-error') return prev;
        const next = new Map(prev);
        next.set('Runner', {
          service: 'Runner',
          stderr: build.last_error!,
          stdout: '',
          action: 'build-error',
        });
        return next;
      });
    } else {
      setErrors((prev) => {
        if (!prev.has('Runner') || prev.get('Runner')!.action !== 'build-error') return prev;
        const next = new Map(prev);
        next.delete('Runner');
        return next;
      });
    }
  }, [build?.error_detected, build?.last_error]);

  // Toggle runner protection
  const primaryProtected = runnerProtection.get('primary') ?? false;
  const toggleProtection = useCallback(async () => {
    const newVal = !primaryProtected;
    try {
      await api.protectRunner('primary', newVal);
      setRunnerProtection((prev) => new Map(prev).set('primary', newVal));
      addToast(newVal ? 'Runner protected — safe from rebuilds' : 'Runner unprotected', 'info');
    } catch (e) {
      addToast(`Failed to toggle protection: ${e instanceof Error ? e.message : 'unknown'}`, 'error');
    }
  }, [primaryProtected]);

  // Build service rows
  const svcMap = new Map(data.services.map((s) => [s.name, s]));
  const backendUp = svcMap.get('backend')?.available ?? false;
  const frontendUp = svcMap.get('frontend')?.available ?? false;
  const expoUp = !!expo?.running;

  const rows: RowDef[] = [
    {
      name: 'Runner',
      port: '9876',
      up: !!runner?.api_responding || !!runner?.running,
      actions: [
        {
          key: 'runner-restart',
          display: 'Restart',
          activeLabel: 'Restarting…',
          service: 'Runner',
          fn: () => api.runnerRestart(false),
        },
        {
          key: 'runner-rebuild',
          display: 'Rebuild',
          activeLabel: 'Rebuilding…',
          service: 'Runner',
          fn: () => api.runnerRestart(true),
        },
        {
          key: 'runner-stop',
          display: 'Stop',
          activeLabel: 'Stopping…',
          service: 'Runner',
          fn: () => api.runnerStop(),
        },
      ],
    },
    {
      name: 'Backend',
      port: '8000',
      up: backendUp,
      actions: [
        {
          key: 'backend-start',
          display: backendUp ? 'Restart' : 'Start',
          activeLabel: backendUp ? 'Restarting…' : 'Starting…',
          service: 'Backend',
          fn: () => api.devStartAction('backend'),
        },
        {
          key: 'backend-stop',
          display: 'Stop',
          activeLabel: 'Stopping…',
          service: 'Backend',
          fn: () => api.devStartAction('backend/stop'),
        },
      ],
    },
    {
      name: 'Frontend',
      port: '3001',
      up: frontendUp,
      actions: [
        {
          key: 'frontend-start',
          display: frontendUp ? 'Restart' : 'Start',
          activeLabel: frontendUp ? 'Restarting…' : 'Starting…',
          service: 'Frontend',
          fn: () => api.devStartAction('frontend'),
        },
        {
          key: 'frontend-stop',
          display: 'Stop',
          activeLabel: 'Stopping…',
          service: 'Frontend',
          fn: () => api.devStartAction('frontend/stop'),
        },
        {
          key: 'frontend-clear-cache',
          display: 'Clear Cache',
          activeLabel: 'Clearing…',
          service: 'Frontend',
          fn: () => api.devStartAction('frontend/clear-cache'),
        },
      ],
    },
    {
      name: 'PostgreSQL',
      port: '5432',
      up: svcMap.get('postgresql')?.available ?? false,
    },
    {
      name: 'Redis',
      port: '6379',
      up: svcMap.get('redis')?.available ?? false,
    },
    {
      name: 'MinIO',
      port: '9000',
      up: svcMap.get('minio')?.available ?? false,
    },
    {
      name: 'Vite',
      port: '1420',
      up: svcMap.get('vite')?.available ?? false,
    },
    {
      name: 'Expo',
      port: '8081',
      up: expoUp,
      actions: [
        {
          key: 'expo-start',
          display: expoUp ? 'Restart' : 'Start',
          activeLabel: expoUp ? 'Restarting…' : 'Starting…',
          service: 'Expo',
          fn: () => api.expoStart(),
        },
        {
          key: 'expo-stop',
          display: 'Stop',
          activeLabel: 'Stopping…',
          service: 'Expo',
          fn: () => api.expoStop(),
        },
      ],
    },
    {
      name: 'Watchdog',
      port: '—',
      up: !!watchdog?.enabled,
    },
  ];

  return (
    <div>
      <div className="page-header">
        <h1 className="page-title">Dashboard</h1>
        <div className="flex gap-4 items-center">
          {ai && <AiProviderSelector current={ai} />}
          {lastRefresh && (
            <span className="text-muted" style={{ fontSize: '0.75rem' }}>
              Updated {lastRefresh.toLocaleTimeString()}
            </span>
          )}
        </div>
      </div>
      <p className="page-desc">Monitor service health, control dev-start services, and run AI debug sessions.</p>

      {showAiPanel && ai && (
        <AiSessionPanel
          provider={ai.ai_provider}
          model={ai.ai_model}
          onStop={async () => {
            try {
              await api.aiStop();
            } catch {}
          }}
          onDone={() => {
            setShowAiPanel(false);
            refreshPorts();
          }}
        />
      )}

      <div className="card" style={{ marginBottom: '1rem' }}>
        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th>Service</th>
                <th style={{ width: 70 }}>Port</th>
                <th style={{ width: 80 }}>Status</th>
                <th>Actions</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => {
                const err = errors.get(row.name);
                return (
                  <React.Fragment key={row.name}>
                    <tr>
                      <td style={{ fontFamily: 'inherit', fontWeight: 500 }}>
                        {row.name}
                        {row.name === 'Runner' && (
                          <button
                            onClick={toggleProtection}
                            title={primaryProtected ? 'Protected from rebuilds — click to unprotect' : 'Click to protect from rebuilds'}
                            style={{
                              marginLeft: 6,
                              padding: '1px 5px',
                              background: primaryProtected ? 'rgba(34,197,94,0.15)' : 'transparent',
                              border: `1px solid ${primaryProtected ? 'rgba(34,197,94,0.4)' : 'rgba(255,255,255,0.15)'}`,
                              borderRadius: 3,
                              cursor: 'pointer',
                              fontSize: '0.65rem',
                              fontWeight: 600,
                              color: primaryProtected ? '#22c55e' : 'rgba(255,255,255,0.35)',
                              verticalAlign: 'middle',
                              letterSpacing: '0.02em',
                            }}
                          >
                            {primaryProtected ? 'PROTECTED' : 'PROTECT'}
                          </button>
                        )}
                      </td>
                      <td>{row.port}</td>
                      <td>
                        <StatusDot up={row.up} error={!!err} />
                        <span
                          className={err ? 'text-warning' : row.up ? 'text-success' : 'text-danger'}
                          style={{ fontSize: '0.75rem' }}
                        >
                          {err
                            ? 'ERR'
                            : row.name === 'Watchdog'
                              ? row.up
                                ? 'ON'
                                : 'OFF'
                              : row.up
                                ? 'UP'
                                : 'DOWN'}
                        </span>
                      </td>
                      <td>
                        <div className="flex gap-2">
                          {row.actions?.map((a) => (
                            <SmallBtn
                              key={a.key}
                              label={a.display}
                              activeLabel={a.activeLabel}
                              onClick={doAction(a.key, a.service, a.fn)}
                              busy={busy}
                              busyKey={a.key}
                            />
                          ))}
                          {(err || (!row.up && row.actions)) && (
                            <>
                              <SmallBtn
                                label={ai?.ai_running ? 'AI busy' : 'AI Fix'}
                                activeLabel="Sending…"
                                onClick={() => triggerAiFix(row.name)}
                                busy={aiFixBusy}
                                busyKey={row.name}
                                variant="warning"
                                disabled={!!ai?.ai_running}
                              />
                              {row.name === 'Runner' && (
                                <SmallBtn
                                  label={fixAndRebuildBusy ? 'Fixing…' : 'Fix & Rebuild'}
                                  activeLabel="Fixing & Rebuilding…"
                                  onClick={triggerFixAndRebuild}
                                  busy={fixAndRebuildBusy ? 'Runner' : null}
                                  busyKey="Runner"
                                  variant="warning"
                                  disabled={!!ai?.ai_running || fixAndRebuildBusy}
                                />
                              )}
                            </>
                          )}
                        </div>
                      </td>
                    </tr>
                    {err && (
                      <tr>
                        <td
                          colSpan={4}
                          style={{
                            padding: '0 0.75rem 0.5rem',
                            borderBottom: '1px solid var(--border)',
                          }}
                        >
                          <div
                            style={{
                              background: 'rgba(239,68,68,0.08)',
                              border: '1px solid rgba(239,68,68,0.2)',
                              borderRadius: 4,
                              padding: '0.4rem 0.6rem',
                              fontSize: '0.75rem',
                              fontFamily: 'var(--font-mono)',
                              whiteSpace: 'pre-wrap',
                              maxHeight: 120,
                              overflowY: 'auto',
                              position: 'relative',
                            }}
                          >
                            <button
                              onClick={() => clearError(row.name)}
                              style={{
                                position: 'absolute',
                                top: 2,
                                right: 6,
                                background: 'none',
                                border: 'none',
                                color: 'var(--text-muted)',
                                cursor: 'pointer',
                                fontSize: '0.8rem',
                                padding: '0 4px',
                              }}
                              title="Dismiss"
                            >
                              x
                            </button>
                            {(err.stderr || err.stdout).trim() || 'Action failed (no output)'}
                          </div>
                        </td>
                      </tr>
                    )}
                  </React.Fragment>
                );
              })}
            </tbody>
          </table>
        </div>
      </div>

      <div className="card" style={{ marginBottom: '1rem' }}>
        <div className="card-header" style={{ marginBottom: '0.5rem' }}>
          <span className="card-title">Bulk Actions</span>
        </div>
        <div className="flex gap-2" style={{ flexWrap: 'wrap' }}>
          <SmallBtn
            label="Docker"
            activeLabel="Starting…"
            onClick={doAction('Docker', 'Docker', () => api.devStartAction('docker'))}
            busy={busy}
          />
          <SmallBtn
            label="Stop Docker"
            activeLabel="Stopping…"
            onClick={doAction('Stop Docker', 'Docker', () => api.devStartAction('docker/stop'))}
            busy={busy}
          />
          <span
            style={{
              borderLeft: '1px solid var(--border)',
              margin: '0 0.25rem',
            }}
          />
          <SmallBtn
            label="Start All"
            activeLabel="Starting…"
            onClick={doAction('Start All', 'All', () => api.devStartAction('all'))}
            busy={busy}
          />
          <SmallBtn
            label="Stop All"
            activeLabel="Stopping…"
            onClick={doAction('Stop All', 'All', () => api.devStartAction('stop'))}
            busy={busy}
          />
          <span
            style={{
              borderLeft: '1px solid var(--border)',
              margin: '0 0.25rem',
            }}
          />
          <SmallBtn
            label="Clean"
            activeLabel="Cleaning…"
            onClick={doAction('Clean', 'Clean', () => api.devStartAction('clean'))}
            busy={busy}
          />
          <SmallBtn
            label="Fresh"
            activeLabel="Starting…"
            onClick={doAction('Fresh', 'Fresh', () => api.devStartAction('fresh'))}
            busy={busy}
          />
          <SmallBtn
            label="Migrate"
            activeLabel="Migrating…"
            onClick={doAction('Migrate', 'Migrate', () => api.devStartAction('migrate'))}
            busy={busy}
          />
        </div>
      </div>

      <RunnerInstancesPanel />
      <WorkflowLoopPanel />
      <LogViewer />
    </div>
  );
}

// ─── Exported with Error Boundary ────────────────────────────────────────────

export default function Dashboard() {
  return (
    <ErrorBoundary>
      <ToastContainer />
      <ConfirmDialog />
      <DashboardInner />
    </ErrorBoundary>
  );
}
