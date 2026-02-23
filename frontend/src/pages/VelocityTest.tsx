import React, { useState, useEffect, useCallback } from 'react';
import {
  LineChart,
  Line,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
} from 'recharts';
import {
  api,
  VtStatus,
  VtRun,
  VtRunWithResults,
  VtResult,
  VtTrendPoint,
  VtDiagnostics,
} from '../lib/api';

function scoreColor(score: number | null): string {
  if (score === null) return 'var(--text-muted)';
  if (score >= 80) return 'var(--success)';
  if (score >= 50) return 'var(--warning)';
  return 'var(--danger)';
}

function formatScore(score: number | null): string {
  if (score === null) return '-';
  return score.toFixed(1);
}

function formatMs(ms: number | null): string {
  if (ms === null) return '-';
  if (ms < 1000) return `${Math.round(ms)}ms`;
  return `${(ms / 1000).toFixed(2)}s`;
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes}B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)}KB`;
  return `${(bytes / (1024 * 1024)).toFixed(2)}MB`;
}

const BOTTLENECK_COLORS: Record<string, string> = {
  'Backend Slow': 'var(--danger)',
  'JS Blocking': '#a855f7',
  'Bundle Heavy': 'var(--warning)',
  'TTFB Slow': '#f97316',
  'Render Slow': '#3b82f6',
  'Network Slow': '#06b6d4',
  Healthy: 'var(--success)',
};

function BottleneckBadge({ type }: { type: string | null }) {
  if (!type) return <span style={{ color: 'var(--text-muted)' }}>-</span>;
  const color = BOTTLENECK_COLORS[type] || 'var(--text-muted)';
  return (
    <span
      style={{
        display: 'inline-block',
        padding: '2px 8px',
        borderRadius: 4,
        fontSize: '0.75rem',
        fontWeight: 600,
        color: '#fff',
        background: color,
      }}
    >
      {type}
    </span>
  );
}

function TimingBar({ result }: { result: VtResult }) {
  const total = result.load_time_ms ?? 0;
  if (total <= 0) return null;

  const ttfb = result.ttfb_ms ?? 0;
  const domInteractive = result.dom_interactive_ms ?? 0;
  const fcp = result.fcp_ms ?? 0;
  const domComplete = result.dom_complete_ms ?? 0;

  // Segments: [TTFB] [DOM Interactive - TTFB] [FCP - DOM Interactive] [DOM Complete - FCP] [Remaining]
  const segments = [
    { label: 'TTFB', ms: ttfb, color: '#f97316' },
    { label: 'DOM Interactive', ms: Math.max(0, domInteractive - ttfb), color: '#eab308' },
    { label: 'FCP', ms: Math.max(0, fcp - domInteractive), color: '#22c55e' },
    { label: 'DOM Complete', ms: Math.max(0, domComplete - fcp), color: '#3b82f6' },
    { label: 'Key Element', ms: Math.max(0, total - domComplete), color: '#a855f7' },
  ].filter((s) => s.ms > 0);

  return (
    <div
      style={{
        display: 'flex',
        height: 8,
        borderRadius: 4,
        overflow: 'hidden',
        background: 'var(--bg-tertiary)',
        marginTop: 4,
      }}
    >
      {segments.map((seg, i) => (
        <div
          key={i}
          title={`${seg.label}: ${formatMs(seg.ms)}`}
          style={{
            width: `${(seg.ms / total) * 100}%`,
            background: seg.color,
            minWidth: seg.ms > 0 ? 2 : 0,
          }}
        />
      ))}
    </div>
  );
}

function ResourceTable({ resources }: { resources: VtDiagnostics['resources'] }) {
  if (!resources || resources.length === 0)
    return (
      <div style={{ color: 'var(--text-muted)', fontSize: '0.8rem' }}>No resources recorded</div>
    );

  const sorted = [...resources].sort((a, b) => b.duration - a.duration).slice(0, 10);

  return (
    <div style={{ fontSize: '0.8rem' }}>
      <div style={{ fontWeight: 600, marginBottom: 4 }}>Top Slow Resources</div>
      <table style={{ width: '100%' }}>
        <thead>
          <tr>
            <th style={{ textAlign: 'left', fontSize: '0.75rem' }}>Resource</th>
            <th style={{ textAlign: 'left', fontSize: '0.75rem' }}>Type</th>
            <th style={{ textAlign: 'right', fontSize: '0.75rem' }}>Duration</th>
            <th style={{ textAlign: 'right', fontSize: '0.75rem' }}>Size</th>
          </tr>
        </thead>
        <tbody>
          {sorted.map((r, i) => {
            const shortName = r.name.split('/').pop()?.split('?')[0] || r.name;
            const isSlowResource = r.duration > 500;
            return (
              <tr key={i} style={{ color: isSlowResource ? 'var(--danger)' : 'inherit' }}>
                <td
                  title={r.name}
                  style={{
                    maxWidth: 200,
                    overflow: 'hidden',
                    textOverflow: 'ellipsis',
                    whiteSpace: 'nowrap',
                  }}
                >
                  {shortName}
                </td>
                <td>{r.initiatorType}</td>
                <td className="text-mono" style={{ textAlign: 'right' }}>
                  {formatMs(r.duration)}
                </td>
                <td className="text-mono" style={{ textAlign: 'right' }}>
                  {formatBytes(r.transferSize)}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

function LongTaskList({ tasks }: { tasks: VtDiagnostics['longTasks'] }) {
  if (!tasks || tasks.length === 0)
    return (
      <div style={{ color: 'var(--text-muted)', fontSize: '0.8rem' }}>No long tasks detected</div>
    );

  const totalBlocking = tasks.reduce((sum, t) => sum + (t.duration || 0), 0);

  return (
    <div style={{ fontSize: '0.8rem' }}>
      <div style={{ fontWeight: 600, marginBottom: 4 }}>
        Long Tasks ({tasks.length}) — Total blocking: {formatMs(totalBlocking)}
      </div>
      {tasks.map((t, i) => {
        const dur = t.duration || 0;
        const color = dur > 500 ? 'var(--danger)' : dur > 250 ? '#f97316' : 'var(--warning)';
        const maxBar = 1000;
        const barWidth = Math.min((dur / maxBar) * 100, 100);
        return (
          <div key={i} style={{ display: 'flex', alignItems: 'center', gap: 8, marginBottom: 2 }}>
            <span className="text-mono" style={{ width: 60, textAlign: 'right', color }}>
              {formatMs(dur)}
            </span>
            <div style={{ flex: 1, background: 'var(--bg-tertiary)', borderRadius: 2, height: 10 }}>
              <div
                style={{
                  width: `${barWidth}%`,
                  background: color,
                  height: '100%',
                  borderRadius: 2,
                }}
              />
            </div>
          </div>
        );
      })}
    </div>
  );
}

function ScriptAttributionTable({ scripts }: { scripts: VtDiagnostics['scriptAttribution'] }) {
  if (!scripts || scripts.length === 0)
    return (
      <div style={{ color: 'var(--text-muted)', fontSize: '0.8rem' }}>
        No LoAF data (requires Chrome 123+)
      </div>
    );

  return (
    <div style={{ fontSize: '0.8rem' }}>
      <div style={{ fontWeight: 600, marginBottom: 4 }}>Script Attribution (LoAF)</div>
      <table style={{ width: '100%' }}>
        <thead>
          <tr>
            <th style={{ textAlign: 'left', fontSize: '0.75rem' }}>Source File</th>
            <th style={{ textAlign: 'left', fontSize: '0.75rem' }}>Function</th>
            <th style={{ textAlign: 'right', fontSize: '0.75rem' }}>Duration</th>
            <th style={{ textAlign: 'left', fontSize: '0.75rem' }}>Invoker</th>
          </tr>
        </thead>
        <tbody>
          {scripts.map((s, i) => {
            const fileName =
              s.sourceURL.split('/').pop()?.split('?')[0] || s.sourceURL || '(unknown)';
            const color =
              s.duration > 500
                ? 'var(--danger)'
                : s.duration > 250
                  ? '#f97316'
                  : s.duration > 100
                    ? 'var(--warning)'
                    : 'inherit';
            return (
              <tr key={i} style={{ color }}>
                <td
                  title={s.sourceURL}
                  style={{
                    maxWidth: 200,
                    overflow: 'hidden',
                    textOverflow: 'ellipsis',
                    whiteSpace: 'nowrap',
                  }}
                >
                  {fileName}
                </td>
                <td
                  style={{
                    maxWidth: 150,
                    overflow: 'hidden',
                    textOverflow: 'ellipsis',
                    whiteSpace: 'nowrap',
                  }}
                >
                  {s.sourceFunctionName || '(anonymous)'}
                </td>
                <td className="text-mono" style={{ textAlign: 'right' }}>
                  {formatMs(s.duration)}
                </td>
                <td
                  style={{
                    maxWidth: 150,
                    overflow: 'hidden',
                    textOverflow: 'ellipsis',
                    whiteSpace: 'nowrap',
                  }}
                >
                  {s.invoker || '-'}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

function DiagnosticDetail({ result }: { result: VtResult }) {
  const diag: VtDiagnostics | null = result.diagnostics_json
    ? (() => {
        try {
          return JSON.parse(result.diagnostics_json!) as VtDiagnostics;
        } catch {
          return null;
        }
      })()
    : null;

  const bottleneckColor = result.bottleneck
    ? BOTTLENECK_COLORS[result.bottleneck] || 'var(--text-muted)'
    : 'var(--text-muted)';

  // Build explanation text for bottleneck
  let explanation = '';
  if (result.bottleneck === 'Backend Slow' && result.api_response_time_ms != null) {
    const pct = result.load_time_ms
      ? ((result.api_response_time_ms / result.load_time_ms) * 100).toFixed(0)
      : '?';
    explanation = `API response took ${formatMs(result.api_response_time_ms)} (${pct}% of total load time)`;
  } else if (result.bottleneck === 'JS Blocking') {
    explanation = `${result.long_task_count} long tasks totaling ${formatMs(result.long_task_total_ms)} of main thread blocking`;
  } else if (result.bottleneck === 'Bundle Heavy') {
    explanation = `${result.resource_count} resources, ${formatBytes(result.total_transfer_size_bytes)} total transfer`;
  } else if (result.bottleneck === 'TTFB Slow') {
    explanation = `Time to first byte: ${formatMs(result.ttfb_ms)}`;
  } else if (result.bottleneck === 'Render Slow') {
    const renderTime = (result.dom_complete_ms ?? 0) - (result.dom_interactive_ms ?? 0);
    explanation = `Render phase took ${formatMs(renderTime)} (DOM Interactive → DOM Complete)`;
  } else if (result.bottleneck === 'Network Slow') {
    explanation = `Slowest resource took ${formatMs(result.slowest_resource_ms)}`;
  } else if (result.bottleneck === 'Healthy') {
    explanation = 'All metrics within healthy thresholds';
  }

  return (
    <div
      style={{
        padding: '12px 16px',
        background: 'var(--bg-secondary)',
        borderTop: '1px solid var(--border)',
      }}
    >
      {/* Section A: Timing Phases */}
      <div style={{ marginBottom: 12 }}>
        <div style={{ fontWeight: 600, fontSize: '0.85rem', marginBottom: 6 }}>
          Timing Breakdown
        </div>
        <div
          style={{
            display: 'grid',
            gridTemplateColumns: 'repeat(5, 1fr)',
            gap: 8,
            fontSize: '0.8rem',
          }}
        >
          <div>
            <div style={{ color: 'var(--text-muted)' }}>TTFB</div>
            <div className="text-mono">{formatMs(result.ttfb_ms)}</div>
          </div>
          <div>
            <div style={{ color: 'var(--text-muted)' }}>DOM Interactive</div>
            <div className="text-mono">{formatMs(result.dom_interactive_ms)}</div>
          </div>
          <div>
            <div style={{ color: 'var(--text-muted)' }}>FCP</div>
            <div className="text-mono">{formatMs(result.fcp_ms)}</div>
          </div>
          <div>
            <div style={{ color: 'var(--text-muted)' }}>DOM Complete</div>
            <div className="text-mono">{formatMs(result.dom_complete_ms)}</div>
          </div>
          <div>
            <div style={{ color: 'var(--text-muted)' }}>API Time</div>
            <div
              className="text-mono"
              style={{
                color: (result.api_response_time_ms ?? 0) > 500 ? 'var(--danger)' : 'inherit',
              }}
            >
              {formatMs(result.api_response_time_ms)}
              {result.api_status_code != null && (
                <span style={{ color: 'var(--text-muted)', marginLeft: 4 }}>
                  ({result.api_status_code})
                </span>
              )}
            </div>
          </div>
        </div>
        <TimingBar result={result} />
      </div>

      {/* Section B + C: Resources and Long Tasks side by side */}
      <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 16, marginBottom: 12 }}>
        <ResourceTable resources={diag?.resources} />
        <LongTaskList tasks={diag?.longTasks} />
      </div>

      {/* Section C2: Script Attribution from LoAF */}
      {diag?.scriptAttribution && diag.scriptAttribution.length > 0 && (
        <div style={{ marginBottom: 12 }}>
          <ScriptAttributionTable scripts={diag.scriptAttribution} />
        </div>
      )}

      {/* Section D: Bottleneck Summary */}
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          gap: 12,
          padding: '8px 12px',
          borderRadius: 6,
          background: 'var(--bg-tertiary)',
        }}
      >
        <span
          style={{
            display: 'inline-block',
            padding: '4px 12px',
            borderRadius: 4,
            fontSize: '0.85rem',
            fontWeight: 700,
            color: '#fff',
            background: bottleneckColor,
          }}
        >
          {result.bottleneck || 'Unknown'}
        </span>
        <span style={{ fontSize: '0.85rem', color: 'var(--text-secondary)' }}>{explanation}</span>
      </div>
    </div>
  );
}

export default function VelocityTest() {
  const [status, setStatus] = useState<VtStatus | null>(null);
  const [runs, setRuns] = useState<VtRun[]>([]);
  const [trend, setTrend] = useState<VtTrendPoint[]>([]);
  const [latestResults, setLatestResults] = useState<VtRunWithResults | null>(null);
  const [expandedRunId, setExpandedRunId] = useState<string | null>(null);
  const [expandedResults, setExpandedResults] = useState<VtRunWithResults | null>(null);
  const [expandedResultId, setExpandedResultId] = useState<number | null>(null);
  const [loading, setLoading] = useState(true);

  const loadData = useCallback(async () => {
    try {
      const [s, r, t] = await Promise.all([api.vtStatus(), api.vtRuns(), api.vtTrend()]);
      setStatus(s);
      setRuns(r);
      setTrend(t);

      const latestCompleted = r.find((run) => run.status === 'completed');
      if (latestCompleted) {
        const detail = await api.vtRun(latestCompleted.id);
        if (detail) setLatestResults(detail);
      }
    } catch (err) {
      console.error('Failed to load velocity test data:', err);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    loadData();
  }, [loadData]);

  useEffect(() => {
    if (!status?.running) return;
    const interval = setInterval(async () => {
      try {
        const s = await api.vtStatus();
        setStatus(s);
        if (!s.running) {
          const [r, t] = await Promise.all([api.vtRuns(), api.vtTrend()]);
          setRuns(r);
          setTrend(t);
          const latestCompleted = r.find((run) => run.status === 'completed');
          if (latestCompleted) {
            const detail = await api.vtRun(latestCompleted.id);
            if (detail) setLatestResults(detail);
          }
        }
      } catch {
        /* ignore */
      }
    }, 2000);
    return () => clearInterval(interval);
  }, [status?.running]);

  const handleStart = async () => {
    try {
      await api.vtStart();
      const s = await api.vtStatus();
      setStatus(s);
    } catch (err) {
      console.error('Failed to start velocity tests:', err);
    }
  };

  const handleStop = async () => {
    try {
      await api.vtStop();
      const s = await api.vtStatus();
      setStatus(s);
    } catch (err) {
      console.error('Failed to stop velocity tests:', err);
    }
  };

  const handleExpandRun = async (runId: string) => {
    if (expandedRunId === runId) {
      setExpandedRunId(null);
      setExpandedResults(null);
      setExpandedResultId(null);
      return;
    }
    try {
      const detail = await api.vtRun(runId);
      setExpandedRunId(runId);
      setExpandedResults(detail);
      setExpandedResultId(null);
    } catch (err) {
      console.error('Failed to load run details:', err);
    }
  };

  const testNames = ['Dashboard', 'Build Workflows', 'Runs History', 'Runners', 'Build Tests'];

  if (loading) {
    return <div style={{ padding: '2rem', color: 'var(--text-muted)' }}>Loading...</div>;
  }

  return (
    <div>
      <div className="page-header">
        <h1 className="page-title">Velocity Tests</h1>
        <div className="flex gap-2 items-center">
          {status?.running ? (
            <>
              <span className="text-mono" style={{ fontSize: '0.85rem', color: 'var(--warning)' }}>
                Testing: {status.current_test_index + 1}/{status.total_tests}
                {status.current_test_index < testNames.length && (
                  <> — {testNames[status.current_test_index]}</>
                )}
              </span>
              <button
                className="btn"
                onClick={handleStop}
                style={{ background: 'var(--danger)', color: '#fff' }}
              >
                Stop
              </button>
            </>
          ) : (
            <button className="btn btn-primary" onClick={handleStart}>
              Run Tests
            </button>
          )}
        </div>
      </div>

      {/* Progress bar */}
      {status?.running && (
        <div className="card mb-2" style={{ borderLeft: '3px solid var(--warning)' }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: '1rem' }}>
            <div className="text-mono" style={{ fontSize: '0.85rem' }}>
              Run:{' '}
              <span style={{ color: 'var(--text-muted)' }}>
                {status.current_run_id?.slice(0, 8)}...
              </span>
            </div>
            <div style={{ flex: 1, background: 'var(--bg-tertiary)', borderRadius: 4, height: 8 }}>
              <div
                style={{
                  width: `${status.total_tests > 0 ? ((status.current_test_index + 1) / status.total_tests) * 100 : 0}%`,
                  background: 'var(--accent)',
                  height: '100%',
                  borderRadius: 4,
                  transition: 'width 0.3s ease',
                }}
              />
            </div>
          </div>
        </div>
      )}

      {/* Latest results table */}
      {latestResults && latestResults.results.length > 0 && (
        <div className="card mb-2">
          <div className="card-header">
            <span className="card-title">Latest Results</span>
            <span className="text-mono" style={{ color: 'var(--accent)' }}>
              Overall: {formatScore(latestResults.overall_score)}
            </span>
          </div>
          <div className="table-container">
            <table>
              <thead>
                <tr>
                  <th>Page</th>
                  <th>Load Time</th>
                  <th>API Time</th>
                  <th>Console Errors</th>
                  <th>Element Found</th>
                  <th>Bottleneck</th>
                  <th>Score</th>
                </tr>
              </thead>
              <tbody>
                {latestResults.results.map((r) => (
                  <React.Fragment key={r.id}>
                    <tr
                      onClick={() => setExpandedResultId(expandedResultId === r.id ? null : r.id)}
                      style={{ cursor: 'pointer' }}
                    >
                      <td>{r.test_name}</td>
                      <td className="text-mono">
                        {formatMs(r.load_time_ms)}
                        <TimingBar result={r} />
                      </td>
                      <td
                        className="text-mono"
                        style={{
                          color: (r.api_response_time_ms ?? 0) > 500 ? 'var(--danger)' : 'inherit',
                        }}
                      >
                        {formatMs(r.api_response_time_ms)}
                      </td>
                      <td
                        className="text-mono"
                        style={{ color: r.console_errors > 0 ? 'var(--danger)' : 'var(--success)' }}
                      >
                        {r.console_errors}
                      </td>
                      <td>
                        <span
                          style={{ color: r.element_found ? 'var(--success)' : 'var(--danger)' }}
                        >
                          {r.element_found ? 'Yes' : 'No'}
                        </span>
                      </td>
                      <td>
                        <BottleneckBadge type={r.bottleneck} />
                      </td>
                      <td
                        className="text-mono"
                        style={{ color: scoreColor(r.score), fontWeight: 600 }}
                      >
                        {formatScore(r.score)}
                      </td>
                    </tr>
                    {expandedResultId === r.id && (
                      <tr key={`${r.id}-diag`}>
                        <td colSpan={7} style={{ padding: 0 }}>
                          <DiagnosticDetail result={r} />
                        </td>
                      </tr>
                    )}
                  </React.Fragment>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}

      {/* Trend chart */}
      {trend.length > 1 && (
        <div className="card mb-2">
          <div className="card-header">
            <span className="card-title">Score Trend</span>
            <span className="text-muted" style={{ fontSize: '0.8rem' }}>
              {trend.length} runs
            </span>
          </div>
          <ResponsiveContainer width="100%" height={200}>
            <LineChart data={trend} margin={{ top: 5, right: 20, left: 10, bottom: 5 }}>
              <CartesianGrid strokeDasharray="3 3" stroke="var(--border)" />
              <XAxis
                dataKey="started_at"
                tickFormatter={(v: string) => new Date(v).toLocaleDateString()}
                tick={{ fill: 'var(--text-muted)', fontSize: 11 }}
              />
              <YAxis domain={[0, 100]} tick={{ fill: 'var(--text-muted)', fontSize: 11 }} />
              <Tooltip
                contentStyle={{
                  background: 'var(--bg-tertiary)',
                  border: '1px solid var(--border)',
                  borderRadius: 6,
                }}
                labelFormatter={(v: string) => new Date(v).toLocaleString()}
                formatter={(value: number) => [value.toFixed(1), 'Score']}
              />
              <Line
                type="monotone"
                dataKey="overall_score"
                stroke="var(--accent)"
                strokeWidth={2}
                dot={{ fill: 'var(--accent)', r: 4 }}
              />
            </LineChart>
          </ResponsiveContainer>
        </div>
      )}

      {/* History table */}
      <div className="card">
        <div className="card-header">
          <span className="card-title">Run History</span>
          <span className="text-muted" style={{ fontSize: '0.8rem' }}>
            {runs.length} runs
          </span>
        </div>
        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th>Run</th>
                <th>Status</th>
                <th>Tests</th>
                <th>Score</th>
                <th>Started</th>
              </tr>
            </thead>
            <tbody>
              {runs.map((r) => (
                <React.Fragment key={r.id}>
                  <tr onClick={() => handleExpandRun(r.id)} style={{ cursor: 'pointer' }}>
                    <td className="text-mono" style={{ fontSize: '0.8rem' }}>
                      {r.id.slice(0, 8)}...
                    </td>
                    <td>
                      <span
                        className={
                          r.status === 'completed'
                            ? 'text-success'
                            : r.status === 'running'
                              ? 'text-warning'
                              : r.status === 'failed'
                                ? 'text-danger'
                                : r.status === 'stopped'
                                  ? 'text-muted'
                                  : ''
                        }
                      >
                        {r.status}
                      </span>
                    </td>
                    <td>
                      {r.tests_completed}/{r.tests_total}
                    </td>
                    <td
                      className="text-mono"
                      style={{ color: scoreColor(r.overall_score), fontWeight: 600 }}
                    >
                      {formatScore(r.overall_score)}
                    </td>
                    <td>{new Date(r.started_at).toLocaleString()}</td>
                  </tr>
                  {expandedRunId === r.id && expandedResults && (
                    <tr key={`${r.id}-detail`}>
                      <td colSpan={5} style={{ padding: 0 }}>
                        <table style={{ margin: 0, borderTop: 'none' }}>
                          <thead>
                            <tr>
                              <th style={{ paddingLeft: '2rem' }}>Page</th>
                              <th>Load Time</th>
                              <th>API Time</th>
                              <th>Bottleneck</th>
                              <th>Score</th>
                            </tr>
                          </thead>
                          <tbody>
                            {expandedResults.results.map((er) => (
                              <tr key={er.id}>
                                <td style={{ paddingLeft: '2rem' }}>{er.test_name}</td>
                                <td className="text-mono">{formatMs(er.load_time_ms)}</td>
                                <td
                                  className="text-mono"
                                  style={{
                                    color:
                                      (er.api_response_time_ms ?? 0) > 500
                                        ? 'var(--danger)'
                                        : 'inherit',
                                  }}
                                >
                                  {formatMs(er.api_response_time_ms)}
                                </td>
                                <td>
                                  <BottleneckBadge type={er.bottleneck} />
                                </td>
                                <td
                                  className="text-mono"
                                  style={{ color: scoreColor(er.score), fontWeight: 600 }}
                                >
                                  {formatScore(er.score)}
                                </td>
                              </tr>
                            ))}
                          </tbody>
                        </table>
                      </td>
                    </tr>
                  )}
                </React.Fragment>
              ))}
              {runs.length === 0 && (
                <tr>
                  <td colSpan={5} style={{ textAlign: 'center', color: 'var(--text-muted)' }}>
                    No velocity test runs yet. Click "Run Tests" to begin.
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
