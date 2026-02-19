import { useState, useEffect, useCallback } from 'react';
import { api, HealthResponse } from '../lib/api';

type ActionState = string | null;

function ActionButton({ label, activeLabel, action, busy }: {
  label: string;
  activeLabel: string;
  action: () => Promise<unknown>;
  busy: ActionState;
}) {
  const isActive = busy === label;
  const handleClick = async () => action();
  return (
    <button className="btn" disabled={busy !== null} onClick={handleClick}>
      {isActive ? activeLabel : label}
    </button>
  );
}

export default function Dashboard() {
  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [ports, setPorts] = useState<Record<string, unknown> | null>(null);
  const [expo, setExpo] = useState<Record<string, unknown> | null>(null);
  const [runnerAction, setRunnerAction] = useState<ActionState>(null);
  const [devStartAction, setDevStartAction] = useState<ActionState>(null);
  const [expoAction, setExpoAction] = useState<ActionState>(null);

  const wrapAction = useCallback((setter: (v: ActionState) => void, label: string, fn: () => Promise<unknown>) => {
    return async () => {
      setter(label);
      try { await fn(); } catch { /* status polling shows result */ } finally { setter(null); }
    };
  }, []);

  useEffect(() => {
    const fetchHealth = () => api.health().then(setHealth).catch(() => {});
    fetchHealth();
    const id = setInterval(fetchHealth, 3000);
    return () => clearInterval(id);
  }, []);

  useEffect(() => {
    const fetchPorts = () => api.devStartStatus().then(setPorts).catch(() => {});
    fetchPorts();
    const id = setInterval(fetchPorts, 5000);
    return () => clearInterval(id);
  }, []);

  useEffect(() => {
    const fetchExpo = () => api.expoStatus().then(setExpo).catch(() => {});
    fetchExpo();
    const id = setInterval(fetchExpo, 5000);
    return () => clearInterval(id);
  }, []);

  const runner = health?.runner as Record<string, unknown> | undefined;

  return (
    <div>
      <div className="page-header">
        <h1 className="page-title">Dashboard</h1>
      </div>

      <div className="card-grid">
        <div className="card">
          <div className="card-header">
            <span className="card-title">Runner</span>
            <span className={`badge ${runner?.running ? 'badge-success' : 'badge-danger'}`}>
              {runner?.running ? 'Running' : 'Stopped'}
            </span>
          </div>
          {runner?.pid != null && <div className="text-mono text-muted">PID: {String(runner.pid)}</div>}
          <div className="mt-1 flex gap-2">
            <ActionButton label="Restart" activeLabel="Restarting…" busy={runnerAction}
              action={wrapAction(setRunnerAction, 'Restart', () => api.runnerRestart(false))} />
            <ActionButton label="Rebuild" activeLabel="Rebuilding…" busy={runnerAction}
              action={wrapAction(setRunnerAction, 'Rebuild', () => api.runnerRestart(true))} />
            <ActionButton label="Stop" activeLabel="Stopping…" busy={runnerAction}
              action={wrapAction(setRunnerAction, 'Stop', () => api.runnerStop())} />
          </div>
        </div>

        <div className="card">
          <div className="card-header">
            <span className="card-title">Watchdog</span>
            <span className={`badge ${(health?.watchdog as Record<string, unknown>)?.enabled ? 'badge-success' : 'badge-warning'}`}>
              {(health?.watchdog as Record<string, unknown>)?.enabled ? 'Enabled' : 'Disabled'}
            </span>
          </div>
        </div>

        <div className="card">
          <div className="card-header">
            <span className="card-title">Services</span>
          </div>
          {ports && (
            <div className="text-mono" style={{ fontSize: '0.8rem' }}>
              {Object.entries(ports).filter(([k]) => k !== 'timestamp').map(([name, info]) => (
                <div key={name} className="flex justify-between" style={{ padding: '2px 0' }}>
                  <span>{name}</span>
                  <span className={(info as Record<string, unknown>)?.listening ? 'text-success' : 'text-danger'}>
                    {(info as Record<string, unknown>)?.listening ? 'UP' : 'DOWN'}
                  </span>
                </div>
              ))}
            </div>
          )}
        </div>

        <div className="card">
          <div className="card-header">
            <span className="card-title">Dev-Start Controls</span>
          </div>
          <div className="mt-1 flex gap-2" style={{ flexWrap: 'wrap' }}>
            <ActionButton label="Backend" activeLabel="Starting…" busy={devStartAction}
              action={wrapAction(setDevStartAction, 'Backend', () => api.devStartAction('backend'))} />
            <ActionButton label="Frontend" activeLabel="Starting…" busy={devStartAction}
              action={wrapAction(setDevStartAction, 'Frontend', () => api.devStartAction('frontend'))} />
            <ActionButton label="Docker" activeLabel="Starting…" busy={devStartAction}
              action={wrapAction(setDevStartAction, 'Docker', () => api.devStartAction('docker'))} />
            <ActionButton label="All" activeLabel="Starting…" busy={devStartAction}
              action={wrapAction(setDevStartAction, 'All', () => api.devStartAction('all'))} />
          </div>
          <div className="mt-1 flex gap-2" style={{ flexWrap: 'wrap' }}>
            <ActionButton label="Stop All" activeLabel="Stopping…" busy={devStartAction}
              action={wrapAction(setDevStartAction, 'Stop All', () => api.devStartAction('stop'))} />
            <ActionButton label="Stop Backend" activeLabel="Stopping…" busy={devStartAction}
              action={wrapAction(setDevStartAction, 'Stop Backend', () => api.devStartAction('backend/stop'))} />
            <ActionButton label="Stop Frontend" activeLabel="Stopping…" busy={devStartAction}
              action={wrapAction(setDevStartAction, 'Stop Frontend', () => api.devStartAction('frontend/stop'))} />
            <ActionButton label="Stop Docker" activeLabel="Stopping…" busy={devStartAction}
              action={wrapAction(setDevStartAction, 'Stop Docker', () => api.devStartAction('docker/stop'))} />
          </div>
          <div className="mt-1 flex gap-2">
            <ActionButton label="Clean" activeLabel="Cleaning…" busy={devStartAction}
              action={wrapAction(setDevStartAction, 'Clean', () => api.devStartAction('clean'))} />
            <ActionButton label="Fresh" activeLabel="Starting…" busy={devStartAction}
              action={wrapAction(setDevStartAction, 'Fresh', () => api.devStartAction('fresh'))} />
            <ActionButton label="Migrate" activeLabel="Migrating…" busy={devStartAction}
              action={wrapAction(setDevStartAction, 'Migrate', () => api.devStartAction('migrate'))} />
          </div>
        </div>

        <div className="card">
          <div className="card-header">
            <span className="card-title">Expo</span>
            <span className={`badge ${expo?.running ? 'badge-success' : 'badge-danger'}`}>
              {expo?.running ? 'Running' : 'Stopped'}
            </span>
          </div>
          {expo?.pid != null && <div className="text-mono text-muted">PID: {String(expo.pid)}</div>}
          <div className="mt-1 flex gap-2">
            <ActionButton label="Start" activeLabel="Starting…" busy={expoAction}
              action={wrapAction(setExpoAction, 'Start', () => api.expoStart())} />
            <ActionButton label="Stop" activeLabel="Stopping…" busy={expoAction}
              action={wrapAction(setExpoAction, 'Stop', () => api.expoStop())} />
          </div>
        </div>
      </div>
    </div>
  );
}
