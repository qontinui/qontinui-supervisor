import { useState, useEffect, useCallback } from 'react';
import { Link } from 'react-router-dom';
import {
  LineChart,
  Line,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  Legend,
  ResponsiveContainer,
} from 'recharts';
import { api, ServiceSummary, EndpointSummary, SlowRequest, TimelineBucket } from '../lib/api';

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

export default function Velocity() {
  const [summary, setSummary] = useState<ServiceSummary[]>([]);
  const [endpoints, setEndpoints] = useState<EndpointSummary[]>([]);
  const [slow, setSlow] = useState<SlowRequest[]>([]);
  const [timeline, setTimeline] = useState<TimelineBucket[]>([]);
  const [ingesting, setIngesting] = useState(false);
  const [lastIngest, setLastIngest] = useState<string | null>(null);
  const [sortKey, setSortKey] = useState<keyof EndpointSummary>('p95_duration_ms');
  const [sortDir, setSortDir] = useState<'asc' | 'desc'>('desc');

  const loadData = useCallback(async () => {
    try {
      const [s, e, sl, t] = await Promise.all([
        api.summary(),
        api.endpoints(),
        api.slow(),
        api.timeline(),
      ]);
      setSummary(s);
      setEndpoints(e);
      setSlow(sl);
      setTimeline(t);
    } catch (err) {
      console.error('Failed to load velocity data:', err);
    }
  }, []);

  useEffect(() => {
    loadData();
  }, [loadData]);

  const handleIngest = async () => {
    setIngesting(true);
    try {
      const result = await api.ingest();
      setLastIngest(`Ingested ${result.total_new_spans} new spans`);
      await loadData();
    } catch (err) {
      setLastIngest(`Error: ${err}`);
    } finally {
      setIngesting(false);
    }
  };

  const handleSort = (key: keyof EndpointSummary) => {
    if (sortKey === key) {
      setSortDir((d) => (d === 'asc' ? 'desc' : 'asc'));
    } else {
      setSortKey(key);
      setSortDir('desc');
    }
  };

  const sortedEndpoints = [...endpoints].sort((a, b) => {
    const av = a[sortKey];
    const bv = b[sortKey];
    const cmp =
      typeof av === 'number' && typeof bv === 'number'
        ? av - bv
        : String(av).localeCompare(String(bv));
    return sortDir === 'asc' ? cmp : -cmp;
  });

  // Transform timeline data for recharts - pivot so each service is a line
  const timelineByBucket = new Map<string, Record<string, number>>();
  for (const t of timeline) {
    const existing = timelineByBucket.get(t.bucket) || { bucket: t.bucket as unknown as number };
    existing[`${t.service}_avg`] = t.avg_duration_ms;
    existing[`${t.service}_p95`] = t.p95_duration_ms;
    timelineByBucket.set(t.bucket, existing);
  }
  const chartData = Array.from(timelineByBucket.values());

  const services = [...new Set(timeline.map((t) => t.service))];

  return (
    <div>
      <div className="page-header">
        <h1 className="page-title">Response Velocity</h1>
        <div className="flex gap-2 items-center">
          {lastIngest && (
            <span className="text-muted" style={{ fontSize: '0.8rem' }}>
              {lastIngest}
            </span>
          )}
          <button className="btn btn-primary" onClick={handleIngest} disabled={ingesting}>
            {ingesting ? 'Ingesting...' : 'Ingest Latest Data'}
          </button>
        </div>
      </div>
      <p className="page-desc">API response latency across services — P50/P95/P99 percentiles, timeline, and slow request detection.</p>

      {/* Service summary cards */}
      <div className="card-grid">
        {summary.map((s) => (
          <div key={s.service} className="card">
            <div className="card-header">
              <span className="card-title" style={{ color: SERVICE_COLORS[s.service] }}>
                {s.service}
              </span>
              <span className="text-mono">{s.total_requests} requests</span>
            </div>
            <div className="stat-row">
              <div className="stat-item">
                <div className="stat-label">P50</div>
                <div className="text-mono">{formatMs(s.p50_duration_ms)}</div>
              </div>
              <div className="stat-item">
                <div className="stat-label">P95</div>
                <div className="text-mono">{formatMs(s.p95_duration_ms)}</div>
              </div>
              <div className="stat-item">
                <div className="stat-label">P99</div>
                <div className="text-mono">{formatMs(s.p99_duration_ms)}</div>
              </div>
              <div className="stat-item">
                <div className="stat-label">Errors</div>
                <div
                  className={`text-mono ${s.error_rate > 0.05 ? 'text-danger' : 'text-success'}`}
                >
                  {(s.error_rate * 100).toFixed(1)}%
                </div>
              </div>
            </div>
          </div>
        ))}
      </div>

      {/* Timeline chart */}
      {chartData.length > 0 && (
        <div className="chart-container">
          <div className="card-title mb-2">Latency Over Time (P95)</div>
          <ResponsiveContainer width="100%" height={300}>
            <LineChart data={chartData}>
              <CartesianGrid strokeDasharray="3 3" stroke="var(--border)" />
              <XAxis dataKey="bucket" tick={{ fill: 'var(--text-muted)', fontSize: 11 }} />
              <YAxis
                tick={{ fill: 'var(--text-muted)', fontSize: 11 }}
                label={{ value: 'ms', position: 'insideLeft', fill: 'var(--text-muted)' }}
              />
              <Tooltip
                contentStyle={{
                  background: 'var(--bg-tertiary)',
                  border: '1px solid var(--border)',
                  borderRadius: 6,
                }}
              />
              <Legend />
              {services.map((s) => (
                <Line
                  key={s}
                  type="monotone"
                  dataKey={`${s}_p95`}
                  name={`${s} P95`}
                  stroke={SERVICE_COLORS[s] || '#888'}
                  strokeWidth={2}
                  dot={false}
                />
              ))}
            </LineChart>
          </ResponsiveContainer>
        </div>
      )}

      {/* Endpoint table */}
      <div className="card mb-2">
        <div className="card-header">
          <span className="card-title">Endpoints</span>
        </div>
        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th onClick={() => handleSort('service')}>Service</th>
                <th onClick={() => handleSort('http_method')}>Method</th>
                <th onClick={() => handleSort('http_route')}>Route</th>
                <th onClick={() => handleSort('request_count')}>Count</th>
                <th onClick={() => handleSort('avg_duration_ms')}>Avg</th>
                <th onClick={() => handleSort('p50_duration_ms')}>P50</th>
                <th onClick={() => handleSort('p95_duration_ms')}>P95</th>
                <th onClick={() => handleSort('p99_duration_ms')}>P99</th>
                <th onClick={() => handleSort('error_count')}>Errors</th>
              </tr>
            </thead>
            <tbody>
              {sortedEndpoints.map((e, i) => (
                <tr key={i}>
                  <td style={{ color: SERVICE_COLORS[e.service] }}>{e.service}</td>
                  <td>{e.http_method}</td>
                  <td>
                    <Link
                      to={`/velocity/endpoint?service=${encodeURIComponent(e.service)}&method=${encodeURIComponent(e.http_method)}&route=${encodeURIComponent(e.http_route)}`}
                    >
                      {e.http_route}
                    </Link>
                  </td>
                  <td>{e.request_count}</td>
                  <td>{formatMs(e.avg_duration_ms)}</td>
                  <td>{formatMs(e.p50_duration_ms)}</td>
                  <td className={e.p95_duration_ms > 1000 ? 'text-warning' : ''}>
                    {formatMs(e.p95_duration_ms)}
                  </td>
                  <td className={e.p99_duration_ms > 2000 ? 'text-danger' : ''}>
                    {formatMs(e.p99_duration_ms)}
                  </td>
                  <td className={e.error_count > 0 ? 'text-danger' : ''}>{e.error_count}</td>
                </tr>
              ))}
              {sortedEndpoints.length === 0 && (
                <tr>
                  <td colSpan={9} style={{ textAlign: 'center', color: 'var(--text-muted)' }}>
                    No data. Click "Ingest Latest Data" to load.
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      </div>

      {/* Slow requests */}
      <div className="card">
        <div className="card-header">
          <span className="card-title">Slow Requests (&gt;1s)</span>
        </div>
        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th>Service</th>
                <th>Method</th>
                <th>Route</th>
                <th>Duration</th>
                <th>Status</th>
                <th>Time</th>
                <th>Request ID</th>
              </tr>
            </thead>
            <tbody>
              {slow.map((s) => (
                <tr key={s.id}>
                  <td style={{ color: SERVICE_COLORS[s.service] }}>{s.service}</td>
                  <td>{s.http_method}</td>
                  <td>{s.http_route}</td>
                  <td className="text-danger">{formatMs(s.duration_ms)}</td>
                  <td>{s.http_status_code ?? '-'}</td>
                  <td>{new Date(s.start_ts).toLocaleTimeString()}</td>
                  <td
                    style={{
                      fontSize: '0.7rem',
                      maxWidth: '120px',
                      overflow: 'hidden',
                      textOverflow: 'ellipsis',
                    }}
                  >
                    {s.request_id ? (
                      <Link to={`/velocity/trace?id=${encodeURIComponent(s.request_id)}`}>
                        {s.request_id.slice(0, 12)}...
                      </Link>
                    ) : (
                      '-'
                    )}
                  </td>
                </tr>
              ))}
              {slow.length === 0 && (
                <tr>
                  <td colSpan={7} style={{ textAlign: 'center', color: 'var(--text-muted)' }}>
                    No slow requests found.
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
}
