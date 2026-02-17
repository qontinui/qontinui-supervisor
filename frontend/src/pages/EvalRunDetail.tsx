import { useState, useEffect, useCallback } from 'react';
import { useParams, Link } from 'react-router-dom';
import { api, EvalRunWithResults, EvalResultItem, EvalRunSummary, CompareReport } from '../lib/api';

function formatScore(score: number | null): string {
  if (score === null) return '-';
  return score.toFixed(2);
}

function formatDuration(ms: number | null): string {
  if (ms === null) return '-';
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

function ScoreCell({ score }: { score: number | null }) {
  if (score === null) return <td className="text-muted">-</td>;
  const color = score >= 4 ? 'var(--success)' : score >= 3 ? 'var(--warning)' : 'var(--danger)';
  return <td style={{ color }} className="text-mono">{score}</td>;
}

export default function EvalRunDetail() {
  const { id } = useParams<{ id: string }>();
  const [data, setData] = useState<EvalRunWithResults | null>(null);
  const [expandedResult, setExpandedResult] = useState<number | null>(null);
  const [loading, setLoading] = useState(true);

  // Compare state
  const [runs, setRuns] = useState<EvalRunSummary[]>([]);
  const [compareBaselineId, setCompareBaselineId] = useState<string>('');
  const [compareReport, setCompareReport] = useState<CompareReport | null>(null);

  const loadData = useCallback(async () => {
    if (!id) return;
    try {
      const [runData, allRuns] = await Promise.all([
        api.evalRun(id),
        api.evalRuns(),
      ]);
      setData(runData);
      setRuns(allRuns.filter(r => r.id !== id && r.status === 'completed'));
    } catch (err) {
      console.error('Failed to load eval run:', err);
    } finally {
      setLoading(false);
    }
  }, [id]);

  useEffect(() => { loadData(); }, [loadData]);

  const handleCompare = async () => {
    if (!id || !compareBaselineId) return;
    try {
      const report = await api.evalCompare(id, compareBaselineId);
      setCompareReport(report);
    } catch (err) {
      console.error('Failed to compare runs:', err);
    }
  };

  const toggleExpand = (resultId: number) => {
    setExpandedResult(prev => prev === resultId ? null : resultId);
  };

  if (loading) {
    return <div style={{ padding: '2rem', color: 'var(--text-muted)' }}>Loading...</div>;
  }

  if (!data) {
    return <div style={{ padding: '2rem', color: 'var(--text-muted)' }}>Run not found</div>;
  }

  const run = data;
  const results = data.results;

  return (
    <div>
      <div className="page-header">
        <h1 className="page-title">
          <Link to="/evaluation" style={{ color: 'var(--text-muted)', textDecoration: 'none' }}>Evaluation</Link>
          {' / '}
          <span className="text-mono" style={{ fontSize: '0.9rem' }}>{run.id.slice(0, 8)}</span>
        </h1>
      </div>

      {/* Summary card */}
      <div className="card mb-2">
        <div className="card-header">
          <span className="card-title">Run Summary</span>
          <span className={
            run.status === 'completed' ? 'text-success' :
            run.status === 'running' ? 'text-warning' : 'text-danger'
          }>
            {run.status}
          </span>
        </div>
        <div className="stat-row">
          <div className="stat-item">
            <div className="stat-label">Overall</div>
            <div className="text-mono" style={{ fontSize: '1.2rem', color: 'var(--accent)' }}>
              {formatScore(run.avg_overall_score)}
            </div>
          </div>
          <div className="stat-item">
            <div className="stat-label">Structural</div>
            <div className="text-mono">{formatScore(run.avg_structural)}</div>
          </div>
          <div className="stat-item">
            <div className="stat-label">Commands</div>
            <div className="text-mono">{formatScore(run.avg_command_accuracy)}</div>
          </div>
          <div className="stat-item">
            <div className="stat-label">Phase Flow</div>
            <div className="text-mono">{formatScore(run.avg_phase_flow)}</div>
          </div>
          <div className="stat-item">
            <div className="stat-label">Completeness</div>
            <div className="text-mono">{formatScore(run.avg_step_completeness)}</div>
          </div>
          <div className="stat-item">
            <div className="stat-label">Prompts</div>
            <div className="text-mono">{formatScore(run.avg_prompt_quality)}</div>
          </div>
          <div className="stat-item">
            <div className="stat-label">Determinism</div>
            <div className="text-mono">{formatScore(run.avg_determinism)}</div>
          </div>
        </div>
        <div style={{ marginTop: '0.5rem', fontSize: '0.8rem', color: 'var(--text-muted)' }}>
          Mode: {run.mode} | Prompts: {run.prompts_completed}/{run.prompts_total} | Started: {new Date(run.started_at).toLocaleString()}
          {run.completed_at && <> | Completed: {new Date(run.completed_at).toLocaleString()}</>}
        </div>
      </div>

      {/* Compare section */}
      {runs.length > 0 && (
        <div className="card mb-2">
          <div className="card-header">
            <span className="card-title">Compare with Baseline</span>
          </div>
          <div style={{ display: 'flex', gap: '0.5rem', alignItems: 'center', padding: '0.5rem 0' }}>
            <select
              value={compareBaselineId}
              onChange={e => setCompareBaselineId(e.target.value)}
              style={{
                background: 'var(--bg-tertiary)',
                color: 'var(--text-primary)',
                border: '1px solid var(--border)',
                borderRadius: 4,
                padding: '4px 8px',
                fontSize: '0.85rem',
              }}
            >
              <option value="">Select baseline run...</option>
              {runs.map(r => (
                <option key={r.id} value={r.id}>
                  {r.id.slice(0, 8)} — {formatScore(r.avg_overall_score)} avg — {new Date(r.started_at).toLocaleDateString()}
                </option>
              ))}
            </select>
            <button className="btn btn-primary" onClick={handleCompare} disabled={!compareBaselineId}>
              Compare
            </button>
          </div>

          {compareReport && (
            <div style={{ marginTop: '0.5rem' }}>
              <div className="stat-row">
                <div className="stat-item">
                  <div className="stat-label">Avg Delta</div>
                  <div className="text-mono" style={{
                    color: (compareReport.aggregate.avg_overall_delta ?? 0) > 0 ? 'var(--success)' :
                           (compareReport.aggregate.avg_overall_delta ?? 0) < 0 ? 'var(--danger)' : 'var(--text-secondary)',
                  }}>
                    {compareReport.aggregate.avg_overall_delta !== null
                      ? (compareReport.aggregate.avg_overall_delta > 0 ? '+' : '') + compareReport.aggregate.avg_overall_delta.toFixed(2)
                      : '-'}
                  </div>
                </div>
                <div className="stat-item">
                  <div className="stat-label">Regressions</div>
                  <div className={`text-mono ${compareReport.aggregate.regressions > 0 ? 'text-danger' : ''}`}>
                    {compareReport.aggregate.regressions}
                  </div>
                </div>
                <div className="stat-item">
                  <div className="stat-label">Improvements</div>
                  <div className={`text-mono ${compareReport.aggregate.improvements > 0 ? 'text-success' : ''}`}>
                    {compareReport.aggregate.improvements}
                  </div>
                </div>
                <div className="stat-item">
                  <div className="stat-label">Unchanged</div>
                  <div className="text-mono">{compareReport.aggregate.unchanged}</div>
                </div>
              </div>
              <div className="table-container" style={{ marginTop: '0.5rem' }}>
                <table>
                  <thead>
                    <tr>
                      <th>Prompt</th>
                      <th>Baseline</th>
                      <th>Current</th>
                      <th>Delta</th>
                      <th>Status</th>
                    </tr>
                  </thead>
                  <tbody>
                    {compareReport.per_prompt.map(p => (
                      <tr key={p.test_prompt_id}>
                        <td className="text-mono" style={{ fontSize: '0.8rem' }}>{p.test_prompt_id}</td>
                        <td className="text-mono">{formatScore(p.baseline_overall)}</td>
                        <td className="text-mono">{formatScore(p.current_overall)}</td>
                        <td className="text-mono" style={{
                          color: (p.delta ?? 0) > 0 ? 'var(--success)' : (p.delta ?? 0) < 0 ? 'var(--danger)' : '',
                        }}>
                          {p.delta !== null ? (p.delta > 0 ? '+' : '') + p.delta.toFixed(2) : '-'}
                        </td>
                        <td>
                          {p.regression && <span className="text-danger">Regression</span>}
                          {p.improvement && <span className="text-success">Improved</span>}
                          {!p.regression && !p.improvement && <span className="text-muted">Same</span>}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            </div>
          )}
        </div>
      )}

      {/* Per-prompt results */}
      <div className="card">
        <div className="card-header">
          <span className="card-title">Per-Prompt Results</span>
        </div>
        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th>Prompt</th>
                <th>Overall</th>
                <th>Struct</th>
                <th>Cmd</th>
                <th>Flow</th>
                <th>Complete</th>
                <th>Prompt</th>
                <th>Determ</th>
                <th>Gen Time</th>
                <th>Score Time</th>
              </tr>
            </thead>
            <tbody>
              {results.map((r: EvalResultItem) => (
                <>
                  <tr key={r.id} onClick={() => toggleExpand(r.id)} style={{ cursor: 'pointer' }}>
                    <td className="text-mono" style={{ fontSize: '0.8rem' }}>
                      {r.generation_error || r.scoring_error ? (
                        <span style={{ color: 'var(--danger)' }}>{r.test_prompt_id}</span>
                      ) : r.test_prompt_id}
                    </td>
                    <td className="text-mono" style={{
                      color: (r.overall_score ?? 0) >= 4 ? 'var(--success)' :
                             (r.overall_score ?? 0) >= 3 ? 'var(--warning)' : 'var(--danger)',
                      fontWeight: 600,
                    }}>
                      {formatScore(r.overall_score)}
                    </td>
                    <ScoreCell score={r.structural_correctness} />
                    <ScoreCell score={r.command_accuracy} />
                    <ScoreCell score={r.phase_flow_logic} />
                    <ScoreCell score={r.step_completeness} />
                    <ScoreCell score={r.prompt_quality} />
                    <ScoreCell score={r.determinism} />
                    <td className="text-mono" style={{ fontSize: '0.8rem' }}>{formatDuration(r.generation_duration_ms)}</td>
                    <td className="text-mono" style={{ fontSize: '0.8rem' }}>{formatDuration(r.scoring_duration_ms)}</td>
                  </tr>
                  {expandedResult === r.id && (
                    <tr key={`${r.id}-detail`}>
                      <td colSpan={10} style={{ background: 'var(--bg-tertiary)', padding: '0.75rem' }}>
                        {r.generation_error && (
                          <div style={{ marginBottom: '0.5rem' }}>
                            <strong className="text-danger">Generation Error:</strong>
                            <pre style={{ fontSize: '0.8rem', whiteSpace: 'pre-wrap', marginTop: '0.25rem' }}>{r.generation_error}</pre>
                          </div>
                        )}
                        {r.scoring_error && (
                          <div style={{ marginBottom: '0.5rem' }}>
                            <strong className="text-danger">Scoring Error:</strong>
                            <pre style={{ fontSize: '0.8rem', whiteSpace: 'pre-wrap', marginTop: '0.25rem' }}>{r.scoring_error}</pre>
                          </div>
                        )}
                        {r.score_rationales && (() => {
                          try {
                            const rationales = JSON.parse(r.score_rationales);
                            return (
                              <div>
                                <strong>Score Rationales:</strong>
                                <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: '0.5rem', marginTop: '0.25rem' }}>
                                  {Object.entries(rationales).map(([key, val]: [string, unknown]) => {
                                    const v = val as { score: number; rationale: string };
                                    return (
                                      <div key={key} style={{ fontSize: '0.8rem', padding: '0.25rem 0.5rem', background: 'var(--bg-secondary)', borderRadius: 4 }}>
                                        <strong>{key}:</strong> {v.score}/5 — {v.rationale}
                                      </div>
                                    );
                                  })}
                                </div>
                              </div>
                            );
                          } catch {
                            return <pre style={{ fontSize: '0.8rem' }}>{r.score_rationales}</pre>;
                          }
                        })()}
                        {r.task_run_id && (
                          <div style={{ marginTop: '0.5rem', fontSize: '0.8rem', color: 'var(--text-muted)' }}>
                            Task Run: {r.task_run_id} | Workflow: {r.workflow_id}
                          </div>
                        )}
                      </td>
                    </tr>
                  )}
                </>
              ))}
              {results.length === 0 && (
                <tr><td colSpan={10} style={{ textAlign: 'center', color: 'var(--text-muted)' }}>No results yet.</td></tr>
              )}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
}
