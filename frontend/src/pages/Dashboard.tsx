import React, { useState, useEffect, useCallback, useRef } from 'react';
import {
  api,
  HealthResponse,
  DevStartResponse,
  ExpoStatus,
  RunnerDerivedStatus,
  UiErrorSummary,
} from '../lib/api';
import { ErrorBoundary } from '../components/ErrorBoundary';
import { ToastContainer, addToast } from '../components/Toast';
import { ConfirmDialog, confirm } from '../components/ConfirmDialog';
import { SmallBtn } from '../components/SmallBtn';
import { StatusDot } from '../components/StatusDot';
import { RunnerStatusBadge } from '../components/RunnerStatusBadge';
import { useSSE } from '../hooks/useSSE';

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
            <div key={`${l.timestamp}-${l.source}-${i}`} className={`log-line ${levelClass(l.level)}`}>
              <span className="text-muted">{new Date(l.timestamp).toLocaleTimeString()} </span>[
              {l.source}] {l.message}
            </div>
          ))}
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
  expo: ExpoStatus | null;
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
  // Phase 3J.3: supervisor-derived status + runner-reported ui_error.
  // Optional so the panel keeps rendering during lock-contention gaps in the
  // health cache (the supervisor returns `null` in that case).
  derived_status?: RunnerDerivedStatus;
  ui_error?: UiErrorSummary | null;
}

function RunnerInstancesPanel() {
  const [runners, setRunners] = useState<RunnerInstance[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [showAdd, setShowAdd] = useState(false);
  const [newName, setNewName] = useState('');
  const [newPort, setNewPort] = useState('');
  const [rebuild, setRebuild] = useState(true);
  const [isProtected, setIsProtected] = useState(false);

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
    if (!name) {
      addToast('Enter a valid instance name', 'error');
      return;
    }
    const portNum = newPort.trim() ? parseInt(newPort) : undefined;
    if (portNum !== undefined && (isNaN(portNum) || portNum < 1024)) {
      addToast('Port must be >= 1024', 'error');
      return;
    }
    setBusy('spawn');
    try {
      const result = await api.spawnNamedRunner({
        name,
        port: portNum,
        rebuild,
        wait: true,
        protected: isProtected,
        requester_id: 'dashboard',
      });
      addToast(`Runner "${result.name}" spawned on port ${result.port}`, 'info');
      setNewName('');
      setNewPort('');
      setRebuild(true);
      setIsProtected(false);
      setShowAdd(false);
    } catch (e) {
      addToast(`Spawn failed: ${e instanceof Error ? e.message : 'unknown'}`, 'error');
    }
    setBusy(null);
    refresh();
  };

  // Primary is rendered alongside secondary runners so users can Restart/Rebuild
  // it from the dashboard. Stop/Remove remain hidden for primary (user-managed).
  const visibleRunners = runners;

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
        <div style={{ display: 'flex', gap: '0.4rem', marginBottom: '0.5rem', alignItems: 'center', flexWrap: 'wrap' }}>
          <input
            type="text"
            value={newName}
            onChange={(e) => setNewName(e.target.value)}
            placeholder="Instance Name"
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
            placeholder="Port (auto)"
            style={{
              padding: '0.2rem 0.4rem',
              fontSize: '0.75rem',
              background: 'var(--bg-secondary)',
              border: '1px solid var(--border)',
              borderRadius: 3,
              color: 'var(--text)',
              width: 80,
            }}
          />
          <label style={{ display: 'flex', alignItems: 'center', gap: '0.2rem', fontSize: '0.7rem', cursor: 'pointer' }}>
            <input
              type="checkbox"
              checked={rebuild}
              onChange={(e) => setRebuild(e.target.checked)}
              style={{ margin: 0 }}
            />
            Rebuild
          </label>
          <label style={{ display: 'flex', alignItems: 'center', gap: '0.2rem', fontSize: '0.7rem', cursor: 'pointer' }}>
            <input
              type="checkbox"
              checked={isProtected}
              onChange={(e) => setIsProtected(e.target.checked)}
              style={{ margin: 0 }}
            />
            Protected
          </label>
          <button
            className="btn"
            style={{ padding: '0.2rem 0.5rem', fontSize: '0.7rem' }}
            disabled={busy !== null}
            onClick={handleSpawn}
          >
            {busy === 'spawn' ? (rebuild ? 'Building & Spawning...' : 'Spawning...') : 'Spawn'}
          </button>
        </div>
      )}

      {visibleRunners.length === 0 && !showAdd && (
        <div style={{ fontSize: '0.75rem', color: 'var(--text-muted)', padding: '0.25rem 0' }}>
          No runners registered. Click "+ New" to spawn one.
        </div>
      )}

      {visibleRunners.length > 0 && (
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
              {visibleRunners.map((r) => {
                const isUp = r.running || r.api_responding;
                const isPrimary = r.is_primary;
                // Primary uses the legacy single-runner endpoint; secondary
                // runners use the per-id endpoint.
                const doRestart = (rebuild: boolean) =>
                  isPrimary ? api.runnerRestart(rebuild) : api.restartRunnerById(r.id, rebuild);
                return (
                <tr key={r.id}>
                  <td style={{ fontWeight: 500, fontSize: '0.8rem' }}>
                    <span>{r.name}</span>
                    {isPrimary && (
                      <span
                        style={{
                          marginLeft: '0.4rem',
                          padding: '0 0.3rem',
                          fontSize: '0.6rem',
                          background: 'var(--bg-secondary)',
                          border: '1px solid var(--border)',
                          borderRadius: 3,
                          color: 'var(--text-muted)',
                          textTransform: 'uppercase',
                        }}
                      >
                        Primary
                      </span>
                    )}
                  </td>
                  <td style={{ fontSize: '0.75rem', fontFamily: 'var(--font-mono)' }}>{r.port}</td>
                  <td>
                    <div className="flex gap-2" style={{ alignItems: 'center', flexWrap: 'wrap' }}>
                      <StatusDot up={isUp} />
                      <RunnerStatusBadge
                        derivedStatus={r.derived_status}
                        uiError={r.ui_error}
                        fallbackUp={isUp}
                        style={{ fontSize: '0.7rem' }}
                      />
                    </div>
                  </td>
                  <td>
                    <div className="flex gap-2">
                      {!isUp && !isPrimary && (
                        <button
                          className="btn"
                          style={{ padding: '0.15rem 0.4rem', fontSize: '0.7rem' }}
                          disabled={busy !== null}
                          onClick={() => doAction(`Start ${r.name}`, () => api.startRunner(r.id))}
                        >
                          {busy === `Start ${r.name}` ? 'Starting...' : 'Start'}
                        </button>
                      )}
                      {isUp && !isPrimary && (
                        <button
                          className="btn"
                          style={{ padding: '0.15rem 0.4rem', fontSize: '0.7rem' }}
                          disabled={busy !== null}
                          onClick={() => doAction(`Stop ${r.name}`, () => api.stopRunner(r.id))}
                        >
                          {busy === `Stop ${r.name}` ? 'Stopping...' : 'Stop'}
                        </button>
                      )}
                      {isUp && (
                        <button
                          className="btn"
                          style={{ padding: '0.15rem 0.4rem', fontSize: '0.7rem' }}
                          disabled={busy !== null}
                          onClick={() =>
                            doAction(`Restart ${r.name}`, async () => {
                              if (isPrimary) {
                                await confirm(
                                  'Restart primary runner',
                                  `Restart "${r.name}"? Any active work in the runner window will be lost.`,
                                );
                              }
                              await doRestart(false);
                            })
                          }
                          title="Stop and start the runner using the existing binary"
                        >
                          {busy === `Restart ${r.name}` ? 'Restarting...' : 'Restart'}
                        </button>
                      )}
                      <button
                        className="btn"
                        style={{ padding: '0.15rem 0.4rem', fontSize: '0.7rem' }}
                        disabled={busy !== null}
                        onClick={() =>
                          doAction(`Rebuild ${r.name}`, async () => {
                            if (isPrimary) {
                              await confirm(
                                'Rebuild primary runner',
                                `Rebuild and restart "${r.name}"? This takes 1-3 minutes and any active work in the runner window will be lost.`,
                              );
                            }
                            await doRestart(true);
                          })
                        }
                        title="Rebuild the runner binary, then restart (blocks until build finishes)"
                      >
                        {busy === `Rebuild ${r.name}` ? 'Rebuilding...' : 'Rebuild'}
                      </button>
                      {!isUp && !isPrimary && (
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
                );
              })}
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
  const mountedRef = useRef(true);
  const actionGuardRef = useRef(false);

  const refreshPorts = useCallback(async () => {
    const [devStatus, expo] = await Promise.allSettled([api.devStartStatus(), api.expoStatus()]);
    if (!mountedRef.current) return;
    setData((prev) => ({
      ...prev,
      services: devStatus.status === 'fulfilled' ? (devStatus.value.services ?? []) : prev.services,
      expo: expo.status === 'fulfilled' ? expo.value : prev.expo,
    }));
  }, []);

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

  useEffect(() => {
    mountedRef.current = true;
    refresh();
    return () => {
      mountedRef.current = false;
    };
  }, [refresh]);

  useSSE<HealthResponse>('/health/stream', 'health', (health) => {
    if (!mountedRef.current) return;
    setData((prev) => ({ ...prev, health }));
    setLastRefresh(new Date());
  });

  const doAction = useCallback(
    (key: string, service: string, fn: () => Promise<unknown>) => {
      return async () => {
        if (actionGuardRef.current) return;
        actionGuardRef.current = true;

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

  const h = data.health;
  const statusColor =
    h?.status === 'healthy'
      ? 'var(--success)'
      : h?.status === 'external'
        ? 'var(--success)'
        : h?.status === 'degraded'
          ? 'var(--warning, orange)'
          : h?.status === 'building'
            ? 'var(--accent)'
            : 'var(--danger, red)';

  return (
    <div>
      {/* Overall status bar */}
      {h && (
        <div
          className="card"
          style={{
            marginBottom: '1rem',
            display: 'flex',
            alignItems: 'center',
            gap: '1rem',
            padding: '0.6rem 1rem',
          }}
        >
          <StatusDot up={h.status === 'healthy' || h.status === 'external'} />
          <span style={{ fontWeight: 600, textTransform: 'capitalize', color: statusColor }}>
            {h.status}
          </span>
          <span className="text-muted" style={{ fontSize: '0.75rem' }}>
            Runner: {h.runner.running
              ? 'running'
              : h.runner.api_responding
                ? 'external (not supervised)'
                : 'stopped'}
            {h.runner.api_responding && h.runner.running ? ' (API ok)' : ''}
            {' | '}Build: {h.build.in_progress ? 'in progress' : 'idle'}
          </span>
          {lastRefresh && (
            <span className="text-muted" style={{ fontSize: '0.7rem', marginLeft: 'auto' }}>
              updated {lastRefresh.toLocaleTimeString()}
            </span>
          )}
        </div>
      )}

      {/* Runner Instances — primary user action */}
      <RunnerInstancesPanel />

      {/* Service health table */}
      {data.services.length > 0 && (
        <div className="card" style={{ marginBottom: '1rem' }}>
          <div className="card-header" style={{ marginBottom: '0.5rem' }}>
            <span className="card-title">Services</span>
          </div>
          <div className="table-container">
            <table>
              <thead>
                <tr>
                  <th>Service</th>
                  <th style={{ width: 60 }}>Port</th>
                  <th style={{ width: 70 }}>Status</th>
                  <th>Actions</th>
                </tr>
              </thead>
              <tbody>
                {data.services.map((svc) => (
                  <tr key={svc.name}>
                    <td style={{ fontWeight: 500, fontSize: '0.8rem' }}>{svc.name}</td>
                    <td style={{ fontSize: '0.75rem', fontFamily: 'var(--font-mono)' }}>{svc.port}</td>
                    <td>
                      <StatusDot up={svc.available} />
                      <span
                        className={svc.available ? 'text-success' : 'text-danger'}
                        style={{ fontSize: '0.7rem' }}
                      >
                        {svc.available ? 'UP' : 'DOWN'}
                      </span>
                    </td>
                    <td>
                      <div className="flex gap-2">
                        {SERVICE_LOG_MAP[svc.name] && (
                          <SmallBtn
                            label="Logs"
                            activeLabel="Loading..."
                            onClick={async () => {
                              try {
                                const log = await api.logFile(SERVICE_LOG_MAP[svc.name], 100);
                                addToast(`${svc.name}: ${log.lines} lines`, 'info');
                              } catch {
                                addToast(`${svc.name}: failed to fetch logs`, 'error');
                              }
                            }}
                            busy={null}
                          />
                        )}
                        <SmallBtn
                          label="Start"
                          activeLabel="Starting..."
                          onClick={doAction(
                            `${svc.name.toLowerCase()}-start`,
                            svc.name,
                            () => api.devStartAction(`${svc.name.toLowerCase()}-start`),
                          )}
                          busy={busy}
                          busyKey={`${svc.name.toLowerCase()}-start`}
                        />
                        <SmallBtn
                          label="Stop"
                          activeLabel="Stopping..."
                          onClick={doAction(
                            `${svc.name.toLowerCase()}-stop`,
                            svc.name,
                            () => api.devStartAction(`${svc.name.toLowerCase()}-stop`),
                          )}
                          busy={busy}
                          busyKey={`${svc.name.toLowerCase()}-stop`}
                          variant="danger"
                        />
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          {errors.size > 0 && (
            <div style={{ marginTop: '0.5rem' }}>
              {Array.from(errors.values()).map((err) => (
                <div
                  key={err.service}
                  style={{
                    padding: '0.4rem 0.6rem',
                    fontSize: '0.75rem',
                    background: 'rgba(239,68,68,0.08)',
                    borderRadius: 4,
                    marginBottom: '0.25rem',
                  }}
                >
                  <strong className="text-danger">{err.service} — {err.action} failed</strong>
                  {err.stderr && (
                    <pre
                      style={{
                        margin: '0.25rem 0 0',
                        fontSize: '0.7rem',
                        whiteSpace: 'pre-wrap',
                        color: 'var(--text-muted)',
                      }}
                    >
                      {err.stderr}
                    </pre>
                  )}
                </div>
              ))}
            </div>
          )}
        </div>
      )}

      {/* Expo status */}
      {data.expo?.configured && (
        <div className="card" style={{ marginBottom: '1rem' }}>
          <div className="card-header" style={{ marginBottom: '0.5rem' }}>
            <span className="card-title">Expo</span>
          </div>
          <div style={{ display: 'flex', alignItems: 'center', gap: '0.75rem', fontSize: '0.8rem' }}>
            <StatusDot up={data.expo.running} />
            <span>{data.expo.running ? 'Running' : 'Stopped'}</span>
            {data.expo.port > 0 && (
              <span className="text-muted" style={{ fontSize: '0.75rem' }}>
                port {data.expo.port}
              </span>
            )}
            <div className="flex gap-2" style={{ marginLeft: 'auto' }}>
              <SmallBtn
                label="Start"
                activeLabel="Starting..."
                onClick={doAction('expo-start', 'Expo', () => api.expoStart())}
                busy={busy}
                busyKey="expo-start"
              />
              <SmallBtn
                label="Stop"
                activeLabel="Stopping..."
                onClick={doAction('expo-stop', 'Expo', () => api.expoStop())}
                busy={busy}
                busyKey="expo-stop"
                variant="danger"
              />
            </div>
          </div>
        </div>
      )}

      {/* Build info */}
      {h && h.build.error_detected && (
        <div
          className="card"
          style={{
            marginBottom: '1rem',
            border: '1px solid rgba(239,68,68,0.3)',
            background: 'rgba(239,68,68,0.06)',
          }}
        >
          <div className="card-header" style={{ marginBottom: '0.25rem' }}>
            <span className="card-title text-danger">Build Error</span>
          </div>
          <pre
            style={{
              fontSize: '0.7rem',
              whiteSpace: 'pre-wrap',
              wordBreak: 'break-word',
              margin: 0,
              color: 'var(--text-muted)',
            }}
          >
            {h.build.last_error || 'Unknown build error'}
          </pre>
        </div>
      )}

      {/* Logs */}
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
