import { useState, useEffect, useCallback } from 'react';
import { useSearchParams, Link } from 'react-router-dom';
import { BarChart, Bar, LineChart, Line, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer } from 'recharts';
import { api, TimelineBucket, SlowRequest } from '../lib/api';

const SERVICE_COLORS: Record<string, string> = {
  backend: '#6366f1',
  runner: '#22c55e',
  supervisor: '#f59e0b',
};

function formatMs(ms: number): string {
  if (ms < 1) return '<1ms';
  if (ms < 1000) return `${Math.round(ms)}ms`;
  return `${(ms / 1000).toFixed(2)}s`;
}

export default function VelocityEndpoint() {
  const [params] = useSearchParams();
  const route = params.get('route') || '';
  const method = params.get('method') || '';
  const service = params.get('service') || '';

  const [timeline, setTimeline] = useState<TimelineBucket[]>([]);
  const [slow, setSlow] = useState<SlowRequest[]>([]);
  const [loading, setLoading] = useState(true);

  const loadData = useCallback(async () => {
    if (!route || !service) return;
    setLoading(true);
    try {
      const filterParams = `service=${encodeURIComponent(service)}`;
      const [t, s] = await Promise.all([
        api.timeline(filterParams),
        api.slow(`${filterParams}&threshold_ms=0&limit=100`),
      ]);
      // Filter timeline to only this endpoint's service
      setTimeline(t.filter(b => b.service === service));
      // Filter slow requests to this specific endpoint
      setSlow(s.filter(r => r.http_method === method && r.http_route === route));
    } catch (err) {
      console.error('Failed to load endpoint data:', err);
    } finally {
      setLoading(false);
    }
  }, [route, method, service]);

  useEffect(() => { loadData(); }, [loadData]);

  // Build latency distribution buckets from slow request durations
  const distributionBuckets = (() => {
    if (slow.length === 0) return [];
    const ranges = [
      { label: '<50ms', min: 0, max: 50 },
      { label: '50-100ms', min: 50, max: 100 },
      { label: '100-250ms', min: 100, max: 250 },
      { label: '250-500ms', min: 250, max: 500 },
      { label: '500ms-1s', min: 500, max: 1000 },
      { label: '1-2s', min: 1000, max: 2000 },
      { label: '2-5s', min: 2000, max: 5000 },
      { label: '>5s', min: 5000, max: Infinity },
    ];
    return ranges.map(r => ({
      range: r.label,
      count: slow.filter(s => s.duration_ms >= r.min && s.duration_ms < r.max).length,
    })).filter(b => b.count > 0);
  })();

  // Timeline chart data
  const chartData = timeline.map(t => ({
    bucket: t.bucket.slice(11, 16), // HH:MM
    avg: Math.round(t.avg_duration_ms),
    p95: Math.round(t.p95_duration_ms),
    requests: t.request_count,
  }));

  // Summary stats from slow requests (which has threshold_ms=0 so it's all requests)
  const durations = slow.map(s => s.duration_ms).sort((a, b) => a - b);
  const stats = durations.length > 0 ? {
    count: durations.length,
    avg: Math.round(durations.reduce((a, b) => a + b, 0) / durations.length),
    p50: durations[Math.floor(durations.length * 0.5)],
    p95: durations[Math.floor(durations.length * 0.95)],
    p99: durations[Math.floor(durations.length * 0.99)],
    min: durations[0],
    max: durations[durations.length - 1],
    errors: slow.filter(s => s.http_status_code != null && s.http_status_code >= 500).length,
  } : null;

  const color = SERVICE_COLORS[service] || '#888';

  return (
    <div>
      <div className="page-header">
        <h1 className="page-title">
          <Link to="/velocity" style={{ color: 'var(--text-muted)', marginRight: '0.5rem' }}>Velocity</Link>
          /
          <span style={{ color, marginLeft: '0.5rem' }}>{service}</span>
          <span className="text-muted" style={{ margin: '0 0.5rem' }}>{method}</span>
          {route}
        </h1>
      </div>

      {loading && <div className="card"><p className="text-muted">Loading...</p></div>}

      {!loading && stats && (
        <>
          {/* Summary stats */}
          <div className="card-grid">
            <div className="card">
              <div className="card-header">
                <span className="card-title">Overview</span>
                <span className="text-mono">{stats.count} requests</span>
              </div>
              <div className="stat-row">
                <div className="stat-item">
                  <div className="stat-label">P50</div>
                  <div className="text-mono">{formatMs(stats.p50)}</div>
                </div>
                <div className="stat-item">
                  <div className="stat-label">P95</div>
                  <div className="text-mono">{formatMs(stats.p95)}</div>
                </div>
                <div className="stat-item">
                  <div className="stat-label">P99</div>
                  <div className="text-mono">{formatMs(stats.p99)}</div>
                </div>
                <div className="stat-item">
                  <div className="stat-label">Avg</div>
                  <div className="text-mono">{formatMs(stats.avg)}</div>
                </div>
              </div>
            </div>
            <div className="card">
              <div className="card-header">
                <span className="card-title">Range</span>
              </div>
              <div className="stat-row">
                <div className="stat-item">
                  <div className="stat-label">Min</div>
                  <div className="text-mono">{formatMs(stats.min)}</div>
                </div>
                <div className="stat-item">
                  <div className="stat-label">Max</div>
                  <div className="text-mono">{formatMs(stats.max)}</div>
                </div>
                <div className="stat-item">
                  <div className="stat-label">Errors</div>
                  <div className={`text-mono ${stats.errors > 0 ? 'text-danger' : 'text-success'}`}>
                    {stats.errors}
                  </div>
                </div>
              </div>
            </div>
          </div>

          {/* Latency distribution histogram */}
          {distributionBuckets.length > 0 && (
            <div className="chart-container">
              <div className="card-title mb-2">Latency Distribution</div>
              <ResponsiveContainer width="100%" height={250}>
                <BarChart data={distributionBuckets}>
                  <CartesianGrid strokeDasharray="3 3" stroke="var(--border)" />
                  <XAxis dataKey="range" tick={{ fill: 'var(--text-muted)', fontSize: 11 }} />
                  <YAxis tick={{ fill: 'var(--text-muted)', fontSize: 11 }} />
                  <Tooltip contentStyle={{ background: 'var(--bg-tertiary)', border: '1px solid var(--border)', borderRadius: 6 }} />
                  <Bar dataKey="count" fill={color} radius={[4, 4, 0, 0]} />
                </BarChart>
              </ResponsiveContainer>
            </div>
          )}

          {/* Latency over time */}
          {chartData.length > 0 && (
            <div className="chart-container">
              <div className="card-title mb-2">Latency Over Time</div>
              <ResponsiveContainer width="100%" height={250}>
                <LineChart data={chartData}>
                  <CartesianGrid strokeDasharray="3 3" stroke="var(--border)" />
                  <XAxis dataKey="bucket" tick={{ fill: 'var(--text-muted)', fontSize: 11 }} />
                  <YAxis tick={{ fill: 'var(--text-muted)', fontSize: 11 }} label={{ value: 'ms', position: 'insideLeft', fill: 'var(--text-muted)' }} />
                  <Tooltip contentStyle={{ background: 'var(--bg-tertiary)', border: '1px solid var(--border)', borderRadius: 6 }} />
                  <Line type="monotone" dataKey="p95" name="P95" stroke={color} strokeWidth={2} dot={false} />
                  <Line type="monotone" dataKey="avg" name="Avg" stroke="var(--text-muted)" strokeWidth={1} strokeDasharray="5 5" dot={false} />
                </LineChart>
              </ResponsiveContainer>
            </div>
          )}

          {/* Recent requests table */}
          <div className="card">
            <div className="card-header">
              <span className="card-title">Recent Requests</span>
            </div>
            <div className="table-container">
              <table>
                <thead>
                  <tr>
                    <th>Duration</th>
                    <th>Status</th>
                    <th>Time</th>
                    <th>Request ID</th>
                  </tr>
                </thead>
                <tbody>
                  {slow.slice(0, 50).map(s => (
                    <tr key={s.id}>
                      <td className={s.duration_ms > 1000 ? 'text-danger' : s.duration_ms > 500 ? 'text-warning' : ''}>
                        {formatMs(s.duration_ms)}
                      </td>
                      <td className={s.http_status_code != null && s.http_status_code >= 500 ? 'text-danger' : ''}>
                        {s.http_status_code ?? '-'}
                      </td>
                      <td>{new Date(s.start_ts).toLocaleTimeString()}</td>
                      <td style={{ fontSize: '0.7rem' }}>
                        {s.request_id ? (
                          <Link to={`/velocity/trace?id=${encodeURIComponent(s.request_id)}`}>
                            {s.request_id.slice(0, 12)}...
                          </Link>
                        ) : '-'}
                      </td>
                    </tr>
                  ))}
                  {slow.length === 0 && (
                    <tr><td colSpan={4} style={{ textAlign: 'center', color: 'var(--text-muted)' }}>No requests found.</td></tr>
                  )}
                </tbody>
              </table>
            </div>
          </div>
        </>
      )}

      {!loading && !stats && (
        <div className="card">
          <p className="text-muted">No data found for this endpoint. Try ingesting data first.</p>
        </div>
      )}
    </div>
  );
}
