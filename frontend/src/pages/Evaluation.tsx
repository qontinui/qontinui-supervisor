import { useState, useEffect, useCallback, useMemo } from 'react';
import { Link } from 'react-router-dom';
import { BarChart, Bar, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer } from 'recharts';
import { api, EvalStatus, EvalRunSummary, TestPrompt, UnifiedWorkflow } from '../lib/api';

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
  const [categoryFilter, setCategoryFilter] = useState<string>('all');
  const [complexityFilter, setComplexityFilter] = useState<string>('all');
  const [selectedIds, setSelectedIds] = useState<Set<string>>(new Set());
  const [gtPickerPromptId, setGtPickerPromptId] = useState<string | null>(null);
  const [workflows, setWorkflows] = useState<UnifiedWorkflow[]>([]);
  const [wfSearch, setWfSearch] = useState('');

  const loadData = useCallback(async () => {
    try {
      const [s, r, ts] = await Promise.all([api.evalStatus(), api.evalRuns(), api.evalTestSuite()]);
      setStatus(s);
      setRuns(r);
      setTestSuite(ts);
    } catch (err) {
      console.error('Failed to load evaluation data:', err);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    loadData();
  }, [loadData]);

  // Poll status while running
  useEffect(() => {
    if (!status?.running) return;
    const interval = setInterval(async () => {
      try {
        const s = await api.evalStatus();
        setStatus(s);
        if (!s.running) {
          const r = await api.evalRuns();
          setRuns(r);
        }
      } catch {
        /* ignore */
      }
    }, 3000);
    return () => clearInterval(interval);
  }, [status?.running]);

  // Derived data
  const categories = useMemo(() => {
    const cats = new Set(testSuite.map((p) => p.category));
    return ['all', ...Array.from(cats).sort()];
  }, [testSuite]);

  const complexities = useMemo(() => {
    const cmps = new Set(testSuite.map((p) => p.complexity));
    return ['all', ...Array.from(cmps).sort()];
  }, [testSuite]);

  const filteredSuite = useMemo(() => {
    return testSuite.filter((p) => {
      if (categoryFilter !== 'all' && p.category !== categoryFilter) return false;
      if (complexityFilter !== 'all' && p.complexity !== complexityFilter) return false;
      return true;
    });
  }, [testSuite, categoryFilter, complexityFilter]);

  const enabledFiltered = filteredSuite.filter((p) => p.enabled);

  const handleStart = async (promptIds?: string[]) => {
    try {
      await api.evalStart(promptIds);
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

  const openGtPicker = async (promptId: string) => {
    setGtPickerPromptId(promptId);
    setWfSearch('');
    try {
      const wfs = await api.wlWorkflows();
      // Filter out meta workflows
      setWorkflows(
        wfs.filter((w: UnifiedWorkflow) => {
          const name = w.name || '';
          return !name.startsWith('AI Generate:') && !name.startsWith('Meta:');
        }),
      );
    } catch (err) {
      console.error('Failed to load workflows:', err);
    }
  };

  const handleSetGroundTruth = async (workflowId: string) => {
    if (!gtPickerPromptId) return;
    try {
      const result = await api.evalSetGroundTruth(gtPickerPromptId, workflowId);
      if (result.ok) {
        setGtPickerPromptId(null);
        await loadData();
      } else {
        console.error('Failed to set ground truth:', result.message);
      }
    } catch (err) {
      console.error('Failed to set ground truth:', err);
    }
  };

  const handleClearGroundTruth = async (promptId: string) => {
    try {
      await api.evalClearGroundTruth(promptId);
      await loadData();
    } catch (err) {
      console.error('Failed to clear ground truth:', err);
    }
  };

  const handleSelectAll = () => {
    const allIds = new Set(filteredSuite.map((p) => p.id));
    setSelectedIds(allIds);
  };

  const handleSelectNone = () => {
    setSelectedIds(new Set());
  };

  const handleToggleSelect = (id: string) => {
    setSelectedIds((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const handleEnableFiltered = async (enabled: boolean) => {
    try {
      await Promise.all(
        filteredSuite
          .filter((p) => p.enabled !== enabled)
          .map((p) => api.evalTestSuiteUpdate(p.id, { ...p, enabled })),
      );
      await loadData();
    } catch (err) {
      console.error('Failed to bulk toggle:', err);
    }
  };

  // Build score chart data from latest completed run
  const latestCompleted = runs.find(
    (r) => r.status === 'completed' && r.avg_overall_score !== null,
  );

  type ScoreSet = 'combined' | 'gt' | 'generic';
  const [scoreView, setScoreView] = useState<ScoreSet>('combined');

  const buildChartData = (run: EvalRunSummary | undefined, view: ScoreSet) => {
    if (!run) return [];
    const prefix = view === 'gt' ? 'gt_avg_' : view === 'generic' ? 'gen_avg_' : 'avg_';
    const get = (field: string) =>
      (run as unknown as Record<string, unknown>)[`${prefix}${field}`] as number | null;
    return [
      {
        name: 'Structural',
        score: view === 'combined' ? run.avg_structural : get('structural'),
        fill: DIMENSION_COLORS.structural,
      },
      {
        name: 'Commands',
        score: view === 'combined' ? run.avg_command_accuracy : get('command_accuracy'),
        fill: DIMENSION_COLORS.command_accuracy,
      },
      {
        name: 'Phase Flow',
        score: view === 'combined' ? run.avg_phase_flow : get('phase_flow'),
        fill: DIMENSION_COLORS.phase_flow,
      },
      {
        name: 'Completeness',
        score: view === 'combined' ? run.avg_step_completeness : get('step_completeness'),
        fill: DIMENSION_COLORS.step_completeness,
      },
      {
        name: 'Prompts',
        score: view === 'combined' ? run.avg_prompt_quality : get('prompt_quality'),
        fill: DIMENSION_COLORS.prompt_quality,
      },
      {
        name: 'Determinism',
        score: view === 'combined' ? run.avg_determinism : get('determinism'),
        fill: DIMENSION_COLORS.determinism,
      },
    ];
  };

  const chartData = buildChartData(latestCompleted, scoreView);
  const overallScore = latestCompleted
    ? scoreView === 'gt'
      ? latestCompleted.gt_avg_overall
      : scoreView === 'generic'
        ? latestCompleted.gen_avg_overall
        : latestCompleted.avg_overall_score
    : null;
  const scoreCount = latestCompleted
    ? scoreView === 'gt'
      ? latestCompleted.gt_count
      : scoreView === 'generic'
        ? latestCompleted.gen_count
        : null
    : null;

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
              <button
                className="btn"
                onClick={handleStop}
                style={{ background: 'var(--danger)', color: '#fff' }}
              >
                Stop
              </button>
            </>
          ) : (
            <div style={{ display: 'flex', gap: '0.5rem' }}>
              <button className="btn btn-primary" onClick={() => handleStart()}>
                Run All Enabled ({testSuite.filter((p) => p.enabled).length})
              </button>
              {selectedIds.size > 0 && (
                <button
                  className="btn"
                  style={{ background: 'var(--accent)', color: '#fff' }}
                  onClick={() => handleStart(Array.from(selectedIds))}
                >
                  Run Selected ({selectedIds.size})
                </button>
              )}
              {categoryFilter !== 'all' && enabledFiltered.length > 0 && selectedIds.size === 0 && (
                <button
                  className="btn"
                  style={{ background: 'var(--accent)', color: '#fff' }}
                  onClick={() => handleStart(enabledFiltered.map((p) => p.id))}
                >
                  Run {categoryFilter} ({enabledFiltered.length})
                </button>
              )}
            </div>
          )}
        </div>
      </div>

      {/* Status bar */}
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
                  width: `${status.total_prompts > 0 ? ((status.current_prompt_index + 1) / status.total_prompts) * 100 : 0}%`,
                  background: 'var(--accent)',
                  height: '100%',
                  borderRadius: 4,
                  transition: 'width 0.3s ease',
                }}
              />
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
            <div style={{ display: 'flex', alignItems: 'center', gap: '0.75rem' }}>
              <span className="card-title">Latest Scores</span>
              <div
                style={{
                  display: 'flex',
                  gap: '2px',
                  background: 'var(--bg-tertiary)',
                  borderRadius: 4,
                  padding: 2,
                }}
              >
                {(['combined', 'gt', 'generic'] as ScoreSet[]).map((v) => (
                  <button
                    key={v}
                    onClick={() => setScoreView(v)}
                    style={{
                      padding: '2px 10px',
                      fontSize: '0.75rem',
                      border: 'none',
                      borderRadius: 3,
                      cursor: 'pointer',
                      background: scoreView === v ? 'var(--accent)' : 'transparent',
                      color: scoreView === v ? '#fff' : 'var(--text-muted)',
                      fontWeight: scoreView === v ? 600 : 400,
                    }}
                  >
                    {v === 'combined'
                      ? 'Combined'
                      : v === 'gt'
                        ? `Ground Truth (${latestCompleted?.gt_count ?? 0})`
                        : `Generic (${latestCompleted?.gen_count ?? 0})`}
                  </button>
                ))}
              </div>
            </div>
            <span className="text-mono" style={{ color: 'var(--accent)' }}>
              Overall: {formatScore(overallScore ?? null)}
              {scoreCount !== null && (
                <span style={{ color: 'var(--text-muted)', fontSize: '0.8rem' }}>
                  {' '}
                  (n={scoreCount})
                </span>
              )}
            </span>
          </div>
          <ResponsiveContainer width="100%" height={200}>
            <BarChart data={chartData} layout="vertical" margin={{ left: 90 }}>
              <CartesianGrid strokeDasharray="3 3" stroke="var(--border)" />
              <XAxis
                type="number"
                domain={[0, 5]}
                tick={{ fill: 'var(--text-muted)', fontSize: 11 }}
              />
              <YAxis
                type="category"
                dataKey="name"
                tick={{ fill: 'var(--text-secondary)', fontSize: 12 }}
                width={80}
              />
              <Tooltip
                contentStyle={{
                  background: 'var(--bg-tertiary)',
                  border: '1px solid var(--border)',
                  borderRadius: 6,
                }}
              />
              <Bar dataKey="score" radius={[0, 4, 4, 0]} />
            </BarChart>
          </ResponsiveContainer>
        </div>
      )}

      {/* Run history */}
      <div className="card mb-2">
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
                <th>Prompts</th>
                <th>Combined</th>
                <th>Ground Truth</th>
                <th>Generic</th>
                <th>Started</th>
              </tr>
            </thead>
            <tbody>
              {runs.map((r) => (
                <tr key={r.id}>
                  <td>
                    <Link
                      to={`/evaluation/run/${r.id}`}
                      style={{ fontFamily: 'var(--font-mono)', fontSize: '0.8rem' }}
                    >
                      {r.id.slice(0, 8)}...
                    </Link>
                  </td>
                  <td>
                    <span
                      className={
                        r.status === 'completed'
                          ? 'text-success'
                          : r.status === 'running'
                            ? 'text-warning'
                            : r.status === 'failed' || r.status === 'interrupted'
                              ? 'text-danger'
                              : ''
                      }
                    >
                      {r.status}
                    </span>
                  </td>
                  <td>
                    {r.prompts_completed}/{r.prompts_total}
                  </td>
                  <td className="text-mono">{formatScore(r.avg_overall_score)}</td>
                  <td className="text-mono">
                    {r.gt_avg_overall !== null ? (
                      <span>
                        {formatScore(r.gt_avg_overall)}{' '}
                        <span style={{ color: 'var(--text-muted)', fontSize: '0.75rem' }}>
                          ({r.gt_count})
                        </span>
                      </span>
                    ) : (
                      '-'
                    )}
                  </td>
                  <td className="text-mono">
                    {r.gen_avg_overall !== null ? (
                      <span>
                        {formatScore(r.gen_avg_overall)}{' '}
                        <span style={{ color: 'var(--text-muted)', fontSize: '0.75rem' }}>
                          ({r.gen_count})
                        </span>
                      </span>
                    ) : (
                      '-'
                    )}
                  </td>
                  <td>{new Date(r.started_at).toLocaleString()}</td>
                </tr>
              ))}
              {runs.length === 0 && (
                <tr>
                  <td colSpan={7} style={{ textAlign: 'center', color: 'var(--text-muted)' }}>
                    No eval runs yet. Click "Start Eval Run" to begin.
                  </td>
                </tr>
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
            {testSuite.filter((p) => p.enabled).length}/{testSuite.length} enabled
            {filteredSuite.length !== testSuite.length && ` (showing ${filteredSuite.length})`}
            {' | '}
            {testSuite.filter((p) => p.ground_truth_json).length} with ground truth
          </span>
        </div>

        {/* Filters */}
        <div
          style={{
            display: 'flex',
            gap: '0.75rem',
            padding: '0.75rem 1rem',
            borderBottom: '1px solid var(--border)',
            flexWrap: 'wrap',
            alignItems: 'center',
          }}
        >
          <div style={{ display: 'flex', gap: '0.35rem', alignItems: 'center' }}>
            <label style={{ fontSize: '0.8rem', color: 'var(--text-muted)' }}>Category:</label>
            <select
              value={categoryFilter}
              onChange={(e) => {
                setCategoryFilter(e.target.value);
                setSelectedIds(new Set());
              }}
              style={{
                fontSize: '0.8rem',
                padding: '2px 6px',
                background: 'var(--bg-tertiary)',
                border: '1px solid var(--border)',
                borderRadius: 4,
                color: 'var(--text-primary)',
              }}
            >
              {categories.map((c) => (
                <option key={c} value={c}>
                  {c === 'all' ? 'All Categories' : c} (
                  {c === 'all'
                    ? testSuite.length
                    : testSuite.filter((p) => p.category === c).length}
                  )
                </option>
              ))}
            </select>
          </div>
          <div style={{ display: 'flex', gap: '0.35rem', alignItems: 'center' }}>
            <label style={{ fontSize: '0.8rem', color: 'var(--text-muted)' }}>Complexity:</label>
            <select
              value={complexityFilter}
              onChange={(e) => {
                setComplexityFilter(e.target.value);
                setSelectedIds(new Set());
              }}
              style={{
                fontSize: '0.8rem',
                padding: '2px 6px',
                background: 'var(--bg-tertiary)',
                border: '1px solid var(--border)',
                borderRadius: 4,
                color: 'var(--text-primary)',
              }}
            >
              {complexities.map((c) => (
                <option key={c} value={c}>
                  {c === 'all' ? 'All' : c} (
                  {c === 'all'
                    ? testSuite.length
                    : testSuite.filter((p) => p.complexity === c).length}
                  )
                </option>
              ))}
            </select>
          </div>
          <div style={{ display: 'flex', gap: '0.35rem', marginLeft: 'auto' }}>
            <button
              className="btn"
              style={{ padding: '2px 8px', fontSize: '0.75rem' }}
              onClick={handleSelectAll}
            >
              Select All
            </button>
            <button
              className="btn"
              style={{ padding: '2px 8px', fontSize: '0.75rem' }}
              onClick={handleSelectNone}
            >
              Select None
            </button>
            <button
              className="btn"
              style={{ padding: '2px 8px', fontSize: '0.75rem' }}
              onClick={() => handleEnableFiltered(true)}
            >
              Enable Shown
            </button>
            <button
              className="btn"
              style={{ padding: '2px 8px', fontSize: '0.75rem' }}
              onClick={() => handleEnableFiltered(false)}
            >
              Disable Shown
            </button>
          </div>
        </div>

        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th style={{ width: 30 }}></th>
                <th>ID</th>
                <th>Prompt</th>
                <th>Category</th>
                <th>Complexity</th>
                <th>GT</th>
                <th>Enabled</th>
                <th>Actions</th>
              </tr>
            </thead>
            <tbody>
              {filteredSuite.map((p) => (
                <tr key={p.id} style={{ opacity: p.enabled ? 1 : 0.5 }}>
                  <td>
                    <input
                      type="checkbox"
                      checked={selectedIds.has(p.id)}
                      onChange={() => handleToggleSelect(p.id)}
                    />
                  </td>
                  <td className="text-mono" style={{ fontSize: '0.8rem' }}>
                    {p.id}
                  </td>
                  <td
                    style={{
                      maxWidth: 300,
                      overflow: 'hidden',
                      textOverflow: 'ellipsis',
                      whiteSpace: 'nowrap',
                    }}
                  >
                    {p.prompt}
                  </td>
                  <td>{p.category}</td>
                  <td>{p.complexity}</td>
                  <td style={{ textAlign: 'center' }}>
                    {p.ground_truth_json ? (
                      <span style={{ display: 'inline-flex', gap: '4px', alignItems: 'center' }}>
                        <span
                          title="Has ground truth reference"
                          style={{ color: 'var(--success)', fontWeight: 600, fontSize: '0.75rem' }}
                        >
                          REF
                        </span>
                        <button
                          className="btn"
                          style={{
                            padding: '1px 4px',
                            fontSize: '0.65rem',
                            color: 'var(--text-muted)',
                          }}
                          onClick={() => handleClearGroundTruth(p.id)}
                          title="Clear ground truth"
                        >
                          &times;
                        </button>
                      </span>
                    ) : (
                      <button
                        className="btn"
                        style={{ padding: '1px 6px', fontSize: '0.7rem' }}
                        onClick={() => openGtPicker(p.id)}
                        title="Set ground truth from workflow"
                      >
                        Set
                      </button>
                    )}
                  </td>
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

      {/* Ground Truth Workflow Picker Modal */}
      {gtPickerPromptId && (
        <div
          style={{
            position: 'fixed',
            top: 0,
            left: 0,
            right: 0,
            bottom: 0,
            background: 'rgba(0,0,0,0.6)',
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'center',
            zIndex: 1000,
          }}
          onClick={() => setGtPickerPromptId(null)}
        >
          <div
            style={{
              background: 'var(--bg-secondary)',
              borderRadius: 8,
              padding: '1.5rem',
              width: '90%',
              maxWidth: 700,
              maxHeight: '80vh',
              display: 'flex',
              flexDirection: 'column',
              border: '1px solid var(--border)',
            }}
            onClick={(e) => e.stopPropagation()}
          >
            <div
              style={{
                display: 'flex',
                justifyContent: 'space-between',
                alignItems: 'center',
                marginBottom: '1rem',
              }}
            >
              <h3 style={{ margin: 0 }}>
                Set Ground Truth for{' '}
                <span className="text-mono" style={{ color: 'var(--accent)' }}>
                  {gtPickerPromptId}
                </span>
              </h3>
              <button
                className="btn"
                onClick={() => setGtPickerPromptId(null)}
                style={{ padding: '2px 8px' }}
              >
                &times;
              </button>
            </div>
            <div style={{ marginBottom: '0.75rem' }}>
              <input
                type="text"
                placeholder="Search workflows by name..."
                value={wfSearch}
                onChange={(e) => setWfSearch(e.target.value)}
                style={{
                  width: '100%',
                  padding: '6px 10px',
                  background: 'var(--bg-tertiary)',
                  border: '1px solid var(--border)',
                  borderRadius: 4,
                  color: 'var(--text-primary)',
                  fontSize: '0.85rem',
                }}
              />
            </div>
            <div style={{ overflow: 'auto', flex: 1 }}>
              <table style={{ width: '100%' }}>
                <thead>
                  <tr>
                    <th style={{ textAlign: 'left', fontSize: '0.8rem' }}>Name</th>
                    <th style={{ width: 80, fontSize: '0.8rem' }}>Action</th>
                  </tr>
                </thead>
                <tbody>
                  {workflows
                    .filter(
                      (w) =>
                        !wfSearch || (w.name || '').toLowerCase().includes(wfSearch.toLowerCase()),
                    )
                    .slice(0, 50)
                    .map((w) => (
                      <tr key={w.id}>
                        <td style={{ fontSize: '0.8rem' }}>
                          <div>{w.name}</div>
                          <div
                            className="text-mono"
                            style={{ fontSize: '0.7rem', color: 'var(--text-muted)' }}
                          >
                            {w.id.slice(0, 12)}...
                          </div>
                        </td>
                        <td>
                          <button
                            className="btn btn-primary"
                            style={{ padding: '2px 10px', fontSize: '0.75rem' }}
                            onClick={() => handleSetGroundTruth(w.id)}
                          >
                            Use
                          </button>
                        </td>
                      </tr>
                    ))}
                  {workflows.filter(
                    (w) =>
                      !wfSearch || (w.name || '').toLowerCase().includes(wfSearch.toLowerCase()),
                  ).length === 0 && (
                    <tr>
                      <td
                        colSpan={2}
                        style={{ textAlign: 'center', color: 'var(--text-muted)', padding: '1rem' }}
                      >
                        No workflows found
                      </td>
                    </tr>
                  )}
                </tbody>
              </table>
            </div>
            <div style={{ marginTop: '0.75rem', fontSize: '0.8rem', color: 'var(--text-muted)' }}>
              Select a workflow created in qontinui-web to use as the ground truth reference.
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
