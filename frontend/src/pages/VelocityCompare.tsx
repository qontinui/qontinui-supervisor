import { useState } from 'react';
import {
  BarChart,
  Bar,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  Legend,
  ResponsiveContainer,
  Cell,
} from 'recharts';
import { api, CompareResult } from '../lib/api';

function formatMs(ms: number): string {
  if (ms < 1) return '<1ms';
  if (ms < 1000) return `${Math.round(ms)}ms`;
  return `${(ms / 1000).toFixed(2)}s`;
}

function getTimePreset(label: string): { start: string; end: string } {
  const now = new Date();
  const fmt = (d: Date) => d.toISOString();
  switch (label) {
    case '5m':
      return { start: fmt(new Date(now.getTime() - 5 * 60000)), end: fmt(now) };
    case '15m':
      return { start: fmt(new Date(now.getTime() - 15 * 60000)), end: fmt(now) };
    case '30m':
      return { start: fmt(new Date(now.getTime() - 30 * 60000)), end: fmt(now) };
    case '1h':
      return { start: fmt(new Date(now.getTime() - 3600000)), end: fmt(now) };
    case '2h':
      return { start: fmt(new Date(now.getTime() - 7200000)), end: fmt(now) };
    case '4h':
      return { start: fmt(new Date(now.getTime() - 14400000)), end: fmt(now) };
    default:
      return { start: fmt(new Date(now.getTime() - 3600000)), end: fmt(now) };
  }
}

type Preset = '5m' | '15m' | '30m' | '1h' | '2h' | '4h';

export default function VelocityCompare() {
  const [beforePreset, setBeforePreset] = useState<Preset>('2h');
  const [afterPreset, setAfterPreset] = useState<Preset>('1h');
  const [service, setService] = useState('');
  const [results, setResults] = useState<CompareResult[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const handleCompare = async () => {
    setLoading(true);
    setError(null);
    try {
      // "Before" window ends where "after" window starts
      const afterWindow = getTimePreset(afterPreset);
      const beforeWindow = getTimePreset(beforePreset);
      // Ensure before window ends before after window starts
      // Use "before" as the older window, "after" as the more recent
      const params = new URLSearchParams({
        before_start: beforeWindow.start,
        before_end: beforeWindow.end,
        after_start: afterWindow.start,
        after_end: afterWindow.end,
      });
      if (service) params.set('service', service);
      const data = await api.compare(params.toString());
      setResults(data);
    } catch (err) {
      setError(`${err}`);
    } finally {
      setLoading(false);
    }
  };

  // Chart data: show P95 comparison for top endpoints
  const chartData = results.slice(0, 15).map((r) => ({
    endpoint: `${r.http_method} ${r.http_route.length > 30 ? r.http_route.slice(0, 30) + '...' : r.http_route}`,
    before_p95: Math.round(r.before_p95),
    after_p95: Math.round(r.after_p95),
    change: r.p95_change_pct,
  }));

  return (
    <div>
      <div className="page-header">
        <h1 className="page-title">Compare Time Windows</h1>
      </div>

      {/* Controls */}
      <div className="card mb-2">
        <div className="flex gap-4 items-center" style={{ flexWrap: 'wrap' }}>
          <div>
            <label className="stat-label" style={{ display: 'block', marginBottom: '0.25rem' }}>
              Before (baseline)
            </label>
            <div className="flex gap-2">
              {(['5m', '15m', '30m', '1h', '2h', '4h'] as Preset[]).map((p) => (
                <button
                  key={p}
                  className={`btn ${beforePreset === p ? 'btn-primary' : ''}`}
                  onClick={() => setBeforePreset(p)}
                  style={{ padding: '0.25rem 0.5rem', fontSize: '0.8rem' }}
                >
                  {p} ago
                </button>
              ))}
            </div>
          </div>
          <div>
            <label className="stat-label" style={{ display: 'block', marginBottom: '0.25rem' }}>
              After (current)
            </label>
            <div className="flex gap-2">
              {(['5m', '15m', '30m', '1h', '2h', '4h'] as Preset[]).map((p) => (
                <button
                  key={p}
                  className={`btn ${afterPreset === p ? 'btn-primary' : ''}`}
                  onClick={() => setAfterPreset(p)}
                  style={{ padding: '0.25rem 0.5rem', fontSize: '0.8rem' }}
                >
                  {p} ago
                </button>
              ))}
            </div>
          </div>
          <div>
            <label className="stat-label" style={{ display: 'block', marginBottom: '0.25rem' }}>
              Service (optional)
            </label>
            <select
              className="btn"
              value={service}
              onChange={(e) => setService(e.target.value)}
              style={{ fontSize: '0.8rem' }}
            >
              <option value="">All</option>
              <option value="backend">backend</option>
              <option value="runner">runner</option>
              <option value="supervisor">supervisor</option>
            </select>
          </div>
          <div style={{ alignSelf: 'flex-end' }}>
            <button className="btn btn-primary" onClick={handleCompare} disabled={loading}>
              {loading ? 'Comparing...' : 'Compare'}
            </button>
          </div>
        </div>
      </div>

      {error && (
        <div className="card mb-2">
          <p className="text-danger">{error}</p>
        </div>
      )}

      {/* P95 comparison chart */}
      {chartData.length > 0 && (
        <div className="chart-container">
          <div className="card-title mb-2">P95 Latency: Before vs After</div>
          <ResponsiveContainer width="100%" height={Math.max(300, chartData.length * 35)}>
            <BarChart data={chartData} layout="vertical" margin={{ left: 180 }}>
              <CartesianGrid strokeDasharray="3 3" stroke="var(--border)" />
              <XAxis
                type="number"
                tick={{ fill: 'var(--text-muted)', fontSize: 11 }}
                label={{ value: 'ms', position: 'insideBottomRight', fill: 'var(--text-muted)' }}
              />
              <YAxis
                type="category"
                dataKey="endpoint"
                tick={{ fill: 'var(--text-muted)', fontSize: 11 }}
                width={170}
              />
              <Tooltip
                contentStyle={{
                  background: 'var(--bg-tertiary)',
                  border: '1px solid var(--border)',
                  borderRadius: 6,
                }}
              />
              <Legend />
              <Bar
                dataKey="before_p95"
                name="Before P95"
                fill="var(--text-muted)"
                radius={[0, 4, 4, 0]}
              />
              <Bar dataKey="after_p95" name="After P95" radius={[0, 4, 4, 0]}>
                {chartData.map((entry, i) => (
                  <Cell
                    key={i}
                    fill={
                      entry.change > 10
                        ? 'var(--danger)'
                        : entry.change < -10
                          ? 'var(--success)'
                          : 'var(--accent)'
                    }
                  />
                ))}
              </Bar>
            </BarChart>
          </ResponsiveContainer>
        </div>
      )}

      {/* Results table */}
      {results.length > 0 && (
        <div className="card">
          <div className="card-header">
            <span className="card-title">Endpoint Comparison ({results.length} endpoints)</span>
          </div>
          <div className="table-container">
            <table>
              <thead>
                <tr>
                  <th>Method</th>
                  <th>Route</th>
                  <th>Before Count</th>
                  <th>Before P50</th>
                  <th>Before P95</th>
                  <th>After Count</th>
                  <th>After P50</th>
                  <th>After P95</th>
                  <th>P50 Change</th>
                  <th>P95 Change</th>
                </tr>
              </thead>
              <tbody>
                {results.map((r, i) => (
                  <tr key={i}>
                    <td>{r.http_method}</td>
                    <td>{r.http_route}</td>
                    <td>{r.before_count}</td>
                    <td>{formatMs(r.before_p50)}</td>
                    <td>{formatMs(r.before_p95)}</td>
                    <td>{r.after_count}</td>
                    <td>{formatMs(r.after_p50)}</td>
                    <td>{formatMs(r.after_p95)}</td>
                    <td
                      className={
                        r.p50_change_pct > 10
                          ? 'text-danger'
                          : r.p50_change_pct < -10
                            ? 'text-success'
                            : ''
                      }
                    >
                      {r.p50_change_pct > 0 ? '+' : ''}
                      {r.p50_change_pct.toFixed(1)}%
                    </td>
                    <td
                      className={
                        r.p95_change_pct > 10
                          ? 'text-danger'
                          : r.p95_change_pct < -10
                            ? 'text-success'
                            : ''
                      }
                    >
                      {r.p95_change_pct > 0 ? '+' : ''}
                      {r.p95_change_pct.toFixed(1)}%
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}

      {results.length === 0 && !loading && !error && (
        <div className="card">
          <p className="text-muted" style={{ textAlign: 'center' }}>
            Select time windows and click Compare to analyze latency changes.
          </p>
        </div>
      )}
    </div>
  );
}
