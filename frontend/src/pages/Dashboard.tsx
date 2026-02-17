import { useState, useEffect } from 'react';
import { api, HealthResponse } from '../lib/api';

export default function Dashboard() {
  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [ports, setPorts] = useState<Record<string, unknown> | null>(null);

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
            <button className="btn" onClick={() => api.runnerRestart(false)}>Restart</button>
            <button className="btn" onClick={() => api.runnerRestart(true)}>Rebuild</button>
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
      </div>
    </div>
  );
}
