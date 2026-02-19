import { useState, useEffect, useCallback } from 'react';
import { api, VelocityImprovementStatus, VelocityImprovementIteration } from '../lib/api';

const PHASE_LABELS: Record<string, string> = {
  idle: 'Idle',
  running_tests: 'Running Tests',
  analyzing: 'Analyzing Results',
  fixing: 'Fixing Issues',
  restarting_frontend: 'Restarting Frontend',
  waiting_frontend: 'Waiting for Frontend',
  complete: 'Complete',
  stopped: 'Stopped',
  error: 'Error',
};

const PHASE_COLORS: Record<string, string> = {
  idle: 'var(--text-muted)',
  running_tests: 'var(--info)',
  analyzing: 'var(--info)',
  fixing: 'var(--warning)',
  restarting_frontend: 'var(--warning)',
  waiting_frontend: 'var(--warning)',
  complete: 'var(--success)',
  stopped: 'var(--text-muted)',
  error: 'var(--danger)',
};

const BOTTLENECK_COLORS: Record<string, string> = {
  'JS Blocking': '#e74c3c',
  'Bundle Heavy': '#e67e22',
  'Render Slow': '#f39c12',
  'Backend Slow': '#95a5a6',
  'TTFB Slow': '#95a5a6',
  'Network Slow': '#3498db',
  'Healthy': '#2ecc71',
};

function scoreColor(score: number): string {
  if (score >= 80) return 'var(--success)';
  if (score >= 60) return 'var(--warning)';
  return 'var(--danger)';
}

function formatDelta(current: number, previous: number | null): string {
  if (previous === null) return '';
  const delta = current - previous;
  if (delta > 0) return `+${delta.toFixed(1)}`;
  if (delta < 0) return delta.toFixed(1);
  return '0';
}

function PhaseBadge({ phase, iteration, maxIterations }: { phase: string; iteration: number; maxIterations: number }) {
  const label = PHASE_LABELS[phase] || phase;
  const color = PHASE_COLORS[phase] || 'var(--text-muted)';
  return (
    <span
      style={{
        display: 'inline-flex',
        alignItems: 'center',
        gap: '6px',
        padding: '4px 12px',
        borderRadius: '12px',
        fontSize: '0.85rem',
        fontWeight: 600,
        background: `color-mix(in srgb, ${color} 15%, transparent)`,
        color,
        border: `1px solid color-mix(in srgb, ${color} 30%, transparent)`,
      }}
    >
      {phase === 'running_tests' || phase === 'fixing' || phase === 'restarting_frontend' || phase === 'waiting_frontend' ? (
        <span className="spinner" style={{ width: 12, height: 12 }} />
      ) : null}
      {label}
      {iteration > 0 && ` (${iteration}/${maxIterations})`}
    </span>
  );
}

function BottleneckBadge({ bottleneck }: { bottleneck: string }) {
  const bg = BOTTLENECK_COLORS[bottleneck] || '#666';
  return (
    <span
      style={{
        display: 'inline-block',
        padding: '2px 8px',
        borderRadius: '8px',
        fontSize: '0.75rem',
        fontWeight: 600,
        background: `${bg}22`,
        color: bg,
        border: `1px solid ${bg}44`,
      }}
    >
      {bottleneck}
    </span>
  );
}

export default function VelocityImprovement() {
  const [status, setStatus] = useState<VelocityImprovementStatus | null>(null);
  const [iterations, setIterations] = useState<VelocityImprovementIteration[]>([]);
  const [loading, setLoading] = useState(true);
  const [expandedIteration, setExpandedIteration] = useState<number | null>(null);

  // Config inputs
  const [maxIterations, setMaxIterations] = useState(5);
  const [targetScore, setTargetScore] = useState(80);

  const loadData = useCallback(async () => {
    try {
      const [s, h] = await Promise.all([api.viStatus(), api.viHistory()]);
      setStatus(s);
      setIterations(h.iterations);
    } catch (err) {
      console.error('Failed to load velocity improvement data:', err);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    loadData();
  }, [loadData]);

  // Poll when running
  useEffect(() => {
    if (!status?.running) return;
    const interval = setInterval(async () => {
      try {
        const [s, h] = await Promise.all([api.viStatus(), api.viHistory()]);
        setStatus(s);
        setIterations(h.iterations);
      } catch { /* ignore */ }
    }, 3000);
    return () => clearInterval(interval);
  }, [status?.running]);

  const handleStart = async () => {
    try {
      await api.viStart({
        max_iterations: maxIterations,
        target_score: targetScore,
      });
      // Reload status
      const s = await api.viStatus();
      setStatus(s);
    } catch (err) {
      console.error('Failed to start velocity improvement:', err);
    }
  };

  const handleStop = async () => {
    try {
      await api.viStop();
    } catch (err) {
      console.error('Failed to stop velocity improvement:', err);
    }
  };

  if (loading) {
    return <div style={{ padding: 24 }}>Loading...</div>;
  }

  const isRunning = status?.running ?? false;
  const latestScore = iterations.length > 0
    ? iterations[iterations.length - 1].overall_score
    : null;
  const initialScore = iterations.length > 0
    ? iterations[0].overall_score
    : null;

  return (
    <div style={{ padding: 24, maxWidth: 1200 }}>
      {/* Header */}
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 24 }}>
        <h1 style={{ margin: 0, fontSize: '1.5rem' }}>Velocity Improvement</h1>
        {status && (
          <PhaseBadge
            phase={status.phase}
            iteration={status.current_iteration}
            maxIterations={status.max_iterations}
          />
        )}
      </div>

      {/* Controls */}
      <div className="card" style={{ padding: 20, marginBottom: 24 }}>
        <div style={{ display: 'flex', gap: 16, alignItems: 'flex-end', flexWrap: 'wrap' }}>
          <div>
            <label style={{ display: 'block', fontSize: '0.8rem', color: 'var(--text-muted)', marginBottom: 4 }}>
              Max Iterations
            </label>
            <input
              type="number"
              min={1}
              max={20}
              value={maxIterations}
              onChange={(e) => setMaxIterations(parseInt(e.target.value) || 5)}
              disabled={isRunning}
              style={{
                width: 80,
                padding: '6px 10px',
                borderRadius: 6,
                border: '1px solid var(--border)',
                background: 'var(--bg-secondary)',
                color: 'var(--text)',
                fontSize: '0.9rem',
              }}
            />
          </div>
          <div>
            <label style={{ display: 'block', fontSize: '0.8rem', color: 'var(--text-muted)', marginBottom: 4 }}>
              Target Score
            </label>
            <input
              type="number"
              min={0}
              max={100}
              value={targetScore}
              onChange={(e) => setTargetScore(parseInt(e.target.value) || 80)}
              disabled={isRunning}
              style={{
                width: 80,
                padding: '6px 10px',
                borderRadius: 6,
                border: '1px solid var(--border)',
                background: 'var(--bg-secondary)',
                color: 'var(--text)',
                fontSize: '0.9rem',
              }}
            />
          </div>
          <div style={{ display: 'flex', gap: 8 }}>
            {!isRunning ? (
              <button
                onClick={handleStart}
                style={{
                  padding: '8px 20px',
                  borderRadius: 6,
                  border: 'none',
                  background: 'var(--success)',
                  color: '#fff',
                  fontWeight: 600,
                  cursor: 'pointer',
                  fontSize: '0.9rem',
                }}
              >
                Start Improvement Loop
              </button>
            ) : (
              <button
                onClick={handleStop}
                style={{
                  padding: '8px 20px',
                  borderRadius: 6,
                  border: 'none',
                  background: 'var(--danger)',
                  color: '#fff',
                  fontWeight: 600,
                  cursor: 'pointer',
                  fontSize: '0.9rem',
                }}
              >
                Stop
              </button>
            )}
          </div>
        </div>

        {status?.error && (
          <div style={{ marginTop: 12, padding: '8px 12px', borderRadius: 6, background: 'var(--danger-bg, #fee)', color: 'var(--danger)', fontSize: '0.85rem' }}>
            {status.error}
          </div>
        )}
      </div>

      {/* Summary when complete */}
      {status?.phase === 'complete' && initialScore !== null && latestScore !== null && (
        <div className="card" style={{ padding: 20, marginBottom: 24, borderLeft: '4px solid var(--success)' }}>
          <div style={{ fontSize: '1.1rem', fontWeight: 600, marginBottom: 8 }}>Improvement Complete</div>
          <div style={{ display: 'flex', gap: 32, fontSize: '0.95rem' }}>
            <div>
              Initial Score: <strong style={{ color: scoreColor(initialScore) }}>{initialScore.toFixed(1)}</strong>
            </div>
            <div>
              Final Score: <strong style={{ color: scoreColor(latestScore) }}>{latestScore.toFixed(1)}</strong>
            </div>
            <div>
              Change: <strong style={{ color: latestScore > initialScore ? 'var(--success)' : 'var(--danger)' }}>
                {formatDelta(latestScore, initialScore)}
              </strong>
            </div>
            <div>
              Iterations: <strong>{iterations.length}</strong>
            </div>
          </div>
        </div>
      )}

      {/* Score Trend */}
      {iterations.length > 1 && (
        <div className="card" style={{ padding: 20, marginBottom: 24 }}>
          <h3 style={{ margin: '0 0 12px', fontSize: '1rem' }}>Score Trend</h3>
          <div style={{ display: 'flex', alignItems: 'flex-end', gap: 8, height: 80 }}>
            {iterations.map((iter, i) => {
              const score = iter.overall_score ?? 0;
              const height = Math.max(4, (score / 100) * 72);
              return (
                <div key={i} style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 4, flex: 1 }}>
                  <span style={{ fontSize: '0.75rem', fontWeight: 600, color: scoreColor(score) }}>
                    {score.toFixed(0)}
                  </span>
                  <div
                    style={{
                      width: '100%',
                      maxWidth: 60,
                      height,
                      borderRadius: 4,
                      background: scoreColor(score),
                      opacity: 0.8,
                    }}
                  />
                  <span style={{ fontSize: '0.7rem', color: 'var(--text-muted)' }}>#{iter.iteration}</span>
                </div>
              );
            })}
          </div>
        </div>
      )}

      {/* Iteration History */}
      {iterations.length > 0 && (
        <div className="card" style={{ padding: 20 }}>
          <h3 style={{ margin: '0 0 12px', fontSize: '1rem' }}>Iteration History</h3>
          <table style={{ width: '100%', borderCollapse: 'collapse', fontSize: '0.85rem' }}>
            <thead>
              <tr style={{ borderBottom: '1px solid var(--border)' }}>
                <th style={{ textAlign: 'left', padding: '8px 12px' }}>Iteration</th>
                <th style={{ textAlign: 'left', padding: '8px 12px' }}>Score</th>
                <th style={{ textAlign: 'left', padding: '8px 12px' }}>Delta</th>
                <th style={{ textAlign: 'left', padding: '8px 12px' }}>Pages</th>
                <th style={{ textAlign: 'left', padding: '8px 12px' }}>Fix Applied</th>
                <th style={{ textAlign: 'left', padding: '8px 12px' }}>Status</th>
              </tr>
            </thead>
            <tbody>
              {iterations.map((iter, i) => {
                const prevScore = i > 0 ? iterations[i - 1].overall_score : null;
                const score = iter.overall_score ?? 0;
                const isExpanded = expandedIteration === iter.iteration;
                return (
                  <tr key={iter.iteration} style={{ cursor: 'pointer' }}>
                    <td colSpan={6} style={{ padding: 0 }}>
                      {/* Main row */}
                      <div
                        onClick={() => setExpandedIteration(isExpanded ? null : iter.iteration)}
                        style={{
                          display: 'grid',
                          gridTemplateColumns: '1fr 1fr 1fr 1fr 1fr 1fr',
                          padding: '8px 12px',
                          borderBottom: '1px solid var(--border)',
                          background: isExpanded ? 'var(--bg-secondary)' : undefined,
                        }}
                      >
                        <span>#{iter.iteration}</span>
                        <span style={{ color: scoreColor(score), fontWeight: 600 }}>
                          {score.toFixed(1)}
                        </span>
                        <span style={{
                          color: prevScore !== null
                            ? (score > prevScore ? 'var(--success)' : score < prevScore ? 'var(--danger)' : 'var(--text-muted)')
                            : 'var(--text-muted)'
                        }}>
                          {formatDelta(score, prevScore) || '-'}
                        </span>
                        <span>{iter.per_page_scores.length} pages</span>
                        <span>{iter.fix_applied ? 'Yes' : 'No'}</span>
                        <span>{iter.exit_reason || (iter.completed_at ? 'Continued' : 'In progress')}</span>
                      </div>

                      {/* Expanded detail */}
                      {isExpanded && (
                        <div style={{ padding: '12px 24px', background: 'var(--bg-secondary)', borderBottom: '1px solid var(--border)' }}>
                          {/* Per-page scores */}
                          <div style={{ marginBottom: 12 }}>
                            <strong style={{ fontSize: '0.8rem', color: 'var(--text-muted)' }}>Per-Page Scores</strong>
                            <div style={{ display: 'flex', gap: 12, flexWrap: 'wrap', marginTop: 6 }}>
                              {iter.per_page_scores.map((page) => (
                                <div
                                  key={page.name}
                                  style={{
                                    padding: '6px 12px',
                                    borderRadius: 6,
                                    background: 'var(--bg)',
                                    border: '1px solid var(--border)',
                                    display: 'flex',
                                    alignItems: 'center',
                                    gap: 8,
                                  }}
                                >
                                  <span style={{ fontWeight: 600 }}>{page.name}</span>
                                  <span style={{ color: scoreColor(page.score), fontWeight: 600 }}>
                                    {page.score.toFixed(1)}
                                  </span>
                                  <BottleneckBadge bottleneck={page.bottleneck} />
                                </div>
                              ))}
                            </div>
                          </div>

                          {/* Fix summary */}
                          {iter.fix_summary && (
                            <div style={{ marginBottom: 8 }}>
                              <strong style={{ fontSize: '0.8rem', color: 'var(--text-muted)' }}>Fix Summary</strong>
                              <div style={{
                                marginTop: 4,
                                padding: '8px 12px',
                                borderRadius: 6,
                                background: 'var(--bg)',
                                border: '1px solid var(--border)',
                                fontSize: '0.8rem',
                                whiteSpace: 'pre-wrap',
                                maxHeight: 200,
                                overflow: 'auto',
                              }}>
                                {iter.fix_summary}
                              </div>
                            </div>
                          )}

                          {/* Exit reason */}
                          {iter.exit_reason && (
                            <div style={{ fontSize: '0.8rem', color: 'var(--text-muted)' }}>
                              Exit reason: {iter.exit_reason}
                            </div>
                          )}
                        </div>
                      )}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}

      {/* Empty state */}
      {iterations.length === 0 && !isRunning && (
        <div className="card" style={{ padding: 40, textAlign: 'center', color: 'var(--text-muted)' }}>
          No improvement iterations yet. Start the loop to begin optimizing frontend performance.
        </div>
      )}

      {/* Running state with no iterations yet */}
      {iterations.length === 0 && isRunning && (
        <div className="card" style={{ padding: 40, textAlign: 'center', color: 'var(--text-muted)' }}>
          <div className="spinner" style={{ width: 24, height: 24, margin: '0 auto 12px' }} />
          Running initial velocity tests...
        </div>
      )}
    </div>
  );
}
