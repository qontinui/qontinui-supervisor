import { useState, useEffect, useCallback } from 'react';
import { Link } from 'react-router-dom';
import { BarChart, Bar, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer } from 'recharts';
import { api, EvalStatus, EvalRunSummary, TestPrompt } from '../lib/api';

const DIMENSION_COLORS: Record<string, string> = {
  structural: '#6366f1',
  command_accuracy: '#22c55e',
  phase_flow: '#f59e0b',
  step_completeness: '#ec4899',
  prompt_quality: '#06b6d4',
  determinism: '#8b5cf6',
};

function formatScore(score: number | null): string {
  if (score === null) return '-';
  return score.toFixed(2);
}

export default function Evaluation() {
  const [status, setStatus] = useState<EvalStatus | null>(null);
  const [runs, setRuns] = useState<EvalRunSummary[]>([]);
  const [testSuite, setTestSuite] = useState<TestPrompt[]>([]);
  const [loading, setLoading] = useState(true);

  const loadData = useCallback(async () => {
    try {
      const [s, r, ts] = await Promise.all([
        api.evalStatus(),
        api.evalRuns(),
        api.evalTestSuite(),
      ]);
      setStatus(s);
      setRuns(r);
      setTestSuite(ts);
    } catch (err) {
      console.error('Failed to load evaluation data:', err);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { loadData(); }, [loadData]);

  // Poll status while running
  useEffect(() => {
    if (!status?.running) return;
    const interval = setInterval(async () => {
      try {
        const s = await api.evalStatus();
        setStatus(s);
        if (!s.running) {
          // Reload runs when done
          const r = await api.evalRuns();
          setRuns(r);
        }
      } catch { /* ignore */ }
    }, 3000);
    return () => clearInterval(interval);
  }, [status?.running]);

  const handleStart = async () => {
    try {
      await api.evalStart();
      const s = await api.evalStatus();
      setStatus(s);
    } catch (err) {
      console.error('Failed to start eval:', err);
    }
  };

  const handleStop = async () => {
    try {
      await api.evalStop();
      const s = await api.evalStatus();
      setStatus(s);
    } catch (err) {
      console.error('Failed to stop eval:', err);
    }
  };

  const handleTogglePrompt = async (prompt: TestPrompt) => {
    try {
      await api.evalTestSuiteUpdate(prompt.id, { ...prompt, enabled: !prompt.enabled });
      await loadData();
    } catch (err) {
      console.error('Failed to toggle prompt:', err);
    }
  };

  const handleDeletePrompt = async (id: string) => {
    try {
      await api.evalTestSuiteDelete(id);
      await loadData();
    } catch (err) {
      console.error('Failed to delete prompt:', err);
    }
  };

  // Build score chart data from latest completed run
  const latestCompleted = runs.find(r => r.status === 'completed' && r.avg_overall_score !== null);
  const chartData = latestCompleted ? [
    { name: 'Structural', score: latestCompleted.avg_structural, fill: DIMENSION_COLORS.structural },
    { name: 'Commands', score: latestCompleted.avg_command_accuracy, fill: DIMENSION_COLORS.command_accuracy },
    { name: 'Phase Flow', score: latestCompleted.avg_phase_flow, fill: DIMENSION_COLORS.phase_flow },
    { name: 'Completeness', score: latestCompleted.avg_step_completeness, fill: DIMENSION_COLORS.step_completeness },
    { name: 'Prompts', score: latestCompleted.avg_prompt_quality, fill: DIMENSION_COLORS.prompt_quality },
    { name: 'Determinism', score: latestCompleted.avg_determinism, fill: DIMENSION_COLORS.determinism },
  ] : [];

  if (loading) {
    return <div style={{ padding: '2rem', color: 'var(--text-muted)' }}>Loading...</div>;
  }

  return (
    <div>
      <div className="page-header">
        <h1 className="page-title">Evaluation Benchmark</h1>
        <div className="flex gap-2 items-center">
          {status?.running ? (
            <>
              <span className="text-mono" style={{ fontSize: '0.85rem', color: 'var(--warning)' }}>
                Running: {status.current_prompt_index + 1}/{status.total_prompts}
              </span>
              <button className="btn" onClick={handleStop} style={{ background: 'var(--danger)', color: '#fff' }}>
                Stop
              </button>
            </>
          ) : (
            <button className="btn btn-primary" onClick={handleStart}>
              Start Eval Run
            </button>
          )}
        </div>
      </div>

      {/* Status bar */}
      {status?.running && (
        <div className="card mb-2" style={{ borderLeft: '3px solid var(--warning)' }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: '1rem' }}>
            <div className="text-mono" style={{ fontSize: '0.85rem' }}>
              Run: <span style={{ color: 'var(--text-muted)' }}>{status.current_run_id?.slice(0, 8)}...</span>
            </div>
            <div style={{ flex: 1, background: 'var(--bg-tertiary)', borderRadius: 4, height: 8 }}>
              <div style={{
                width: `${status.total_prompts > 0 ? ((status.current_prompt_index + 1) / status.total_prompts * 100) : 0}%`,
                background: 'var(--accent)',
                height: '100%',
                borderRadius: 4,
                transition: 'width 0.3s ease',
              }} />
            </div>
            <span className="text-mono" style={{ fontSize: '0.8rem', color: 'var(--text-muted)' }}>
              {status.continuous_mode ? 'Continuous' : 'On-demand'}
            </span>
          </div>
        </div>
      )}

      {/* Latest scores chart */}
      {chartData.length > 0 && (
        <div className="card mb-2">
          <div className="card-header">
            <span className="card-title">Latest Scores (avg)</span>
            <span className="text-mono" style={{ color: 'var(--accent)' }}>
              Overall: {formatScore(latestCompleted?.avg_overall_score ?? null)}
            </span>
          </div>
          <ResponsiveContainer width="100%" height={200}>
            <BarChart data={chartData} layout="vertical" margin={{ left: 90 }}>
              <CartesianGrid strokeDasharray="3 3" stroke="var(--border)" />
              <XAxis type="number" domain={[0, 5]} tick={{ fill: 'var(--text-muted)', fontSize: 11 }} />
              <YAxis type="category" dataKey="name" tick={{ fill: 'var(--text-secondary)', fontSize: 12 }} width={80} />
              <Tooltip contentStyle={{ background: 'var(--bg-tertiary)', border: '1px solid var(--border)', borderRadius: 6 }} />
              <Bar dataKey="score" radius={[0, 4, 4, 0]} />
            </BarChart>
          </ResponsiveContainer>
        </div>
      )}

      {/* Run history */}
      <div className="card mb-2">
        <div className="card-header">
          <span className="card-title">Run History</span>
          <span className="text-muted" style={{ fontSize: '0.8rem' }}>{runs.length} runs</span>
        </div>
        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th>Run</th>
                <th>Mode</th>
                <th>Status</th>
                <th>Prompts</th>
                <th>Avg Score</th>
                <th>Started</th>
              </tr>
            </thead>
            <tbody>
              {runs.map(r => (
                <tr key={r.id}>
                  <td>
                    <Link to={`/evaluation/run/${r.id}`} style={{ fontFamily: 'var(--font-mono)', fontSize: '0.8rem' }}>
                      {r.id.slice(0, 8)}...
                    </Link>
                  </td>
                  <td>{r.mode}</td>
                  <td>
                    <span className={
                      r.status === 'completed' ? 'text-success' :
                      r.status === 'running' ? 'text-warning' :
                      r.status === 'failed' ? 'text-danger' : ''
                    }>
                      {r.status}
                    </span>
                  </td>
                  <td>{r.prompts_completed}/{r.prompts_total}</td>
                  <td className="text-mono">{formatScore(r.avg_overall_score)}</td>
                  <td>{new Date(r.started_at).toLocaleString()}</td>
                </tr>
              ))}
              {runs.length === 0 && (
                <tr><td colSpan={6} style={{ textAlign: 'center', color: 'var(--text-muted)' }}>No eval runs yet. Click "Start Eval Run" to begin.</td></tr>
              )}
            </tbody>
          </table>
        </div>
      </div>

      {/* Test suite */}
      <div className="card">
        <div className="card-header">
          <span className="card-title">Test Suite</span>
          <span className="text-muted" style={{ fontSize: '0.8rem' }}>
            {testSuite.filter(p => p.enabled).length}/{testSuite.length} enabled
          </span>
        </div>
        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th>ID</th>
                <th>Prompt</th>
                <th>Category</th>
                <th>Complexity</th>
                <th>Enabled</th>
                <th>Actions</th>
              </tr>
            </thead>
            <tbody>
              {testSuite.map(p => (
                <tr key={p.id} style={{ opacity: p.enabled ? 1 : 0.5 }}>
                  <td className="text-mono" style={{ fontSize: '0.8rem' }}>{p.id}</td>
                  <td style={{ maxWidth: 300, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                    {p.prompt}
                  </td>
                  <td>{p.category}</td>
                  <td>{p.complexity}</td>
                  <td>
                    <button
                      className="btn"
                      style={{ padding: '2px 8px', fontSize: '0.75rem' }}
                      onClick={() => handleTogglePrompt(p)}
                    >
                      {p.enabled ? 'Yes' : 'No'}
                    </button>
                  </td>
                  <td>
                    <button
                      className="btn"
                      style={{ padding: '2px 8px', fontSize: '0.75rem', color: 'var(--danger)' }}
                      onClick={() => handleDeletePrompt(p.id)}
                    >
                      Delete
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
}
