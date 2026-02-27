import { useState } from 'react';
import { useSearchParams } from 'react-router-dom';
import { api, TraceSpan } from '../lib/api';

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

export default function VelocityTrace() {
  const [searchParams] = useSearchParams();
  const initialId = searchParams.get('id') || '';

  const [requestId, setRequestId] = useState(initialId);
  const [inputValue, setInputValue] = useState(initialId);
  const [spans, setSpans] = useState<TraceSpan[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [searched, setSearched] = useState(false);

  const handleSearch = async (id?: string) => {
    const searchId = id ?? inputValue;
    if (!searchId.trim()) return;
    setRequestId(searchId);
    setLoading(true);
    setError(null);
    setSearched(true);
    try {
      const data = await api.trace(searchId.trim());
      setSpans(data);
    } catch (err) {
      setError(`${err}`);
    } finally {
      setLoading(false);
    }
  };

  // Auto-search if we have an ID from URL params
  useState(() => {
    if (initialId) handleSearch(initialId);
  });

  // Compute waterfall layout
  const traceStart =
    spans.length > 0 ? Math.min(...spans.map((s) => new Date(s.start_ts).getTime())) : 0;
  const traceEnd =
    spans.length > 0
      ? Math.max(
          ...spans.map((s) => {
            if (s.end_ts) return new Date(s.end_ts).getTime();
            if (s.duration_ms != null) return new Date(s.start_ts).getTime() + s.duration_ms;
            return new Date(s.start_ts).getTime();
          }),
        )
      : 0;
  const traceTotal = traceEnd - traceStart || 1;

  return (
    <div>
      <div className="page-header">
        <h1 className="page-title">Request Trace</h1>
      </div>
      <p className="page-desc">Distributed trace waterfall for a single request — see all spans across services.</p>

      {/* Search */}
      <div className="card mb-2">
        <div className="flex gap-2 items-center">
          <input
            type="text"
            value={inputValue}
            onChange={(e) => setInputValue(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && handleSearch()}
            placeholder="Enter Request ID..."
            style={{
              flex: 1,
              padding: '0.5rem 0.75rem',
              borderRadius: 6,
              border: '1px solid var(--border)',
              background: 'var(--bg-tertiary)',
              color: 'var(--text-primary)',
              fontFamily: 'var(--font-mono)',
              fontSize: '0.875rem',
            }}
          />
          <button className="btn btn-primary" onClick={() => handleSearch()} disabled={loading}>
            {loading ? 'Searching...' : 'Search'}
          </button>
        </div>
      </div>

      {error && (
        <div className="card mb-2">
          <p className="text-danger">{error}</p>
        </div>
      )}

      {/* Trace summary */}
      {spans.length > 0 && (
        <div className="card mb-2">
          <div className="card-header">
            <span className="card-title">Trace: {requestId}</span>
            <span className="text-mono">
              {spans.length} span{spans.length !== 1 ? 's' : ''} | {formatMs(traceTotal)}
            </span>
          </div>
          <div className="stat-row">
            {[...new Set(spans.map((s) => s.service))].map((svc) => (
              <div key={svc} className="stat-item">
                <div className="stat-label">{svc}</div>
                <div className="text-mono" style={{ color: SERVICE_COLORS[svc] || '#888' }}>
                  {formatMs(
                    spans
                      .filter((s) => s.service === svc)
                      .reduce((sum, s) => sum + (s.duration_ms || 0), 0),
                  )}
                </div>
              </div>
            ))}
          </div>
        </div>
      )}

      {/* Waterfall */}
      {spans.length > 0 && (
        <div className="card">
          <div className="card-header">
            <span className="card-title">Waterfall</span>
          </div>
          <div style={{ position: 'relative' }}>
            {spans.map((span) => {
              const start = new Date(span.start_ts).getTime();
              const duration = span.duration_ms || 0;
              const leftPct = ((start - traceStart) / traceTotal) * 100;
              const widthPct = Math.max((duration / traceTotal) * 100, 0.5);
              const color = SERVICE_COLORS[span.service] || '#888';

              return (
                <div
                  key={span.id}
                  style={{
                    display: 'flex',
                    alignItems: 'center',
                    padding: '0.375rem 0',
                    borderBottom: '1px solid var(--border)',
                  }}
                >
                  {/* Label */}
                  <div style={{ width: 200, flexShrink: 0, paddingRight: '0.75rem' }}>
                    <div style={{ fontSize: '0.8rem', color }}>{span.service}</div>
                    <div
                      style={{
                        fontSize: '0.75rem',
                        color: 'var(--text-muted)',
                        whiteSpace: 'nowrap',
                        overflow: 'hidden',
                        textOverflow: 'ellipsis',
                      }}
                    >
                      {span.http_method && span.http_route
                        ? `${span.http_method} ${span.http_route}`
                        : span.name}
                    </div>
                  </div>

                  {/* Bar */}
                  <div style={{ flex: 1, position: 'relative', height: 24 }}>
                    <div
                      style={{
                        position: 'absolute',
                        left: `${leftPct}%`,
                        width: `${widthPct}%`,
                        minWidth: 3,
                        height: '100%',
                        background: span.success ? color : 'var(--danger)',
                        borderRadius: 3,
                        opacity: 0.85,
                      }}
                    />
                  </div>

                  {/* Duration + Status */}
                  <div
                    style={{
                      width: 120,
                      flexShrink: 0,
                      textAlign: 'right',
                      fontFamily: 'var(--font-mono)',
                      fontSize: '0.75rem',
                    }}
                  >
                    <span className={!span.success ? 'text-danger' : ''}>
                      {span.duration_ms != null ? formatMs(span.duration_ms) : '-'}
                    </span>
                    {span.http_status_code != null && (
                      <span
                        className={span.http_status_code >= 500 ? 'text-danger' : 'text-muted'}
                        style={{ marginLeft: '0.5rem' }}
                      >
                        {span.http_status_code}
                      </span>
                    )}
                  </div>
                </div>
              );
            })}
          </div>

          {/* Time scale */}
          <div style={{ display: 'flex', paddingTop: '0.5rem', marginLeft: 200 }}>
            <div
              style={{
                flex: 1,
                display: 'flex',
                justifyContent: 'space-between',
                fontSize: '0.7rem',
                color: 'var(--text-muted)',
                fontFamily: 'var(--font-mono)',
              }}
            >
              <span>0ms</span>
              <span>{formatMs(traceTotal * 0.25)}</span>
              <span>{formatMs(traceTotal * 0.5)}</span>
              <span>{formatMs(traceTotal * 0.75)}</span>
              <span>{formatMs(traceTotal)}</span>
            </div>
          </div>
        </div>
      )}

      {/* Span details table */}
      {spans.length > 0 && (
        <div className="card mt-2">
          <div className="card-header">
            <span className="card-title">Span Details</span>
          </div>
          <div className="table-container">
            <table>
              <thead>
                <tr>
                  <th>Service</th>
                  <th>Name</th>
                  <th>Method</th>
                  <th>Route</th>
                  <th>Status</th>
                  <th>Duration</th>
                  <th>Start</th>
                  <th>Error</th>
                </tr>
              </thead>
              <tbody>
                {spans.map((span) => (
                  <tr key={span.id}>
                    <td style={{ color: SERVICE_COLORS[span.service] || '#888' }}>
                      {span.service}
                    </td>
                    <td>{span.name}</td>
                    <td>{span.http_method || '-'}</td>
                    <td>{span.http_route || '-'}</td>
                    <td
                      className={
                        span.http_status_code != null && span.http_status_code >= 500
                          ? 'text-danger'
                          : ''
                      }
                    >
                      {span.http_status_code ?? '-'}
                    </td>
                    <td>{span.duration_ms != null ? formatMs(span.duration_ms) : '-'}</td>
                    <td>{new Date(span.start_ts).toLocaleTimeString()}</td>
                    <td
                      className="text-danger"
                      style={{ maxWidth: 200, overflow: 'hidden', textOverflow: 'ellipsis' }}
                    >
                      {span.error || '-'}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}

      {searched && spans.length === 0 && !loading && !error && (
        <div className="card">
          <p className="text-muted" style={{ textAlign: 'center' }}>
            No spans found for request ID "{requestId}".
          </p>
        </div>
      )}
    </div>
  );
}
