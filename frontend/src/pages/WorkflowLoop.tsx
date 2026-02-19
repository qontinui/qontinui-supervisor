import { useState, useEffect, useCallback } from 'react';
import { api, WorkflowLoopStatus, IterationResult, UnifiedWorkflow } from '../lib/api';

type Mode = 'simple' | 'pipeline';

const PHASE_COLORS: Record<string, string> = {
  idle: 'var(--text-muted)',
  running_workflow: 'var(--accent)',
  building_workflow: 'var(--warning)',
  evaluating_exit: 'var(--warning)',
  reflecting: '#b39ddb',
  implementing_fixes: '#ff8a65',
  between_iterations: '#b39ddb',
  waiting_for_runner: 'var(--warning)',
  complete: 'var(--success)',
  stopped: 'var(--text-muted)',
  error: 'var(--danger)',
};

function phaseColor(phase: string): string {
  return PHASE_COLORS[phase] || 'var(--text-muted)';
}

function phaseBadgeStyle(phase: string): React.CSSProperties {
  const color = phaseColor(phase);
  return {
    display: 'inline-flex',
    padding: '2px 10px',
    borderRadius: 9999,
    fontSize: '0.75rem',
    fontWeight: 600,
    fontFamily: 'var(--font-mono)',
    background: `color-mix(in srgb, ${color} 15%, transparent)`,
    color,
  };
}

const selectStyle: React.CSSProperties = {
  fontSize: '0.85rem',
  padding: '4px 8px',
  background: 'var(--bg-tertiary)',
  border: '1px solid var(--border)',
  borderRadius: 4,
  color: 'var(--text-primary)',
};

const textareaStyle: React.CSSProperties = {
  width: '100%',
  minHeight: 60,
  padding: '6px 8px',
  fontSize: '0.85rem',
  fontFamily: '-apple-system, BlinkMacSystemFont, Segoe UI, Roboto, sans-serif',
  background: 'var(--bg-tertiary)',
  color: 'var(--text-primary)',
  border: '1px solid var(--border)',
  borderRadius: 4,
  resize: 'vertical' as const,
};

const inputStyle: React.CSSProperties = {
  fontSize: '0.85rem',
  padding: '4px 8px',
  background: 'var(--bg-tertiary)',
  border: '1px solid var(--border)',
  borderRadius: 4,
  color: 'var(--text-primary)',
  width: 70,
  textAlign: 'center' as const,
};

const labelStyle: React.CSSProperties = {
  fontSize: '0.8rem',
  color: 'var(--text-muted)',
  minWidth: 130,
};

const rowStyle: React.CSSProperties = {
  display: 'flex',
  alignItems: 'center',
  justifyContent: 'space-between',
  padding: '4px 0',
};

export default function WorkflowLoop() {
  const [status, setStatus] = useState<WorkflowLoopStatus | null>(null);
  const [history, setHistory] = useState<IterationResult[]>([]);
  const [workflows, setWorkflows] = useState<UnifiedWorkflow[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  // Form state
  const [mode, setMode] = useState<Mode>(() => (localStorage.getItem('wl-mode') as Mode) || 'simple');
  const [selectedWorkflowId, setSelectedWorkflowId] = useState('');
  const [exitStrategy, setExitStrategy] = useState('fixed_iterations');
  const [between, setBetween] = useState('restart_on_signal');
  const [maxIter, setMaxIter] = useState(5);

  // Pipeline form state
  const [buildDesc, setBuildDesc] = useState('');
  const [buildContext, setBuildContext] = useState('');
  const [pipelineExecId, setPipelineExecId] = useState('');
  const [enableFixes, setEnableFixes] = useState(false);
  const [fixContext, setFixContext] = useState('');
  const [fixTimeout, setFixTimeout] = useState(600);

  const loadData = useCallback(async () => {
    try {
      const [s, h] = await Promise.all([api.wlStatus(), api.wlHistory()]);
      setStatus(s);
      setHistory(h.iterations || []);
    } catch (err) {
      console.error('Failed to load workflow loop data:', err);
    } finally {
      setLoading(false);
    }
  }, []);

  const loadWorkflows = useCallback(async () => {
    try {
      const wfs = await api.wlWorkflows();
      setWorkflows(Array.isArray(wfs) ? wfs : []);
    } catch {
      setWorkflows([]);
    }
  }, []);

  useEffect(() => {
    loadData();
    loadWorkflows();
  }, [loadData, loadWorkflows]);

  // Poll while running
  useEffect(() => {
    if (!status?.running) return;
    const interval = setInterval(async () => {
      try {
        const [s, h] = await Promise.all([api.wlStatus(), api.wlHistory()]);
        setStatus(s);
        setHistory(h.iterations || []);
        if (!s.running) loadWorkflows();
      } catch { /* ignore */ }
    }, 3000);
    return () => clearInterval(interval);
  }, [status?.running, loadWorkflows]);

  // SSE for real-time status
  useEffect(() => {
    const sse = new EventSource('/workflow-loop/stream');
    sse.addEventListener('status', (e) => {
      try {
        const s = JSON.parse(e.data) as WorkflowLoopStatus;
        setStatus(s);
      } catch { /* ignore */ }
    });
    sse.onerror = () => { sse.close(); };
    return () => sse.close();
  }, []);

  const handleModeChange = (m: Mode) => {
    setMode(m);
    localStorage.setItem('wl-mode', m);
    if (m === 'pipeline' && (between === 'restart_on_signal' || between === 'restart_on_signal_no_rebuild')) {
      setBetween('restart_runner');
    }
  };

  const buildBetween = () => {
    if (between === 'restart_on_signal') return { type: 'restart_on_signal', rebuild: true };
    if (between === 'restart_on_signal_no_rebuild') return { type: 'restart_on_signal', rebuild: false };
    if (between === 'restart_runner') return { type: 'restart_runner', rebuild: true };
    if (between === 'restart_runner_no_rebuild') return { type: 'restart_runner', rebuild: false };
    if (between === 'wait_healthy') return { type: 'wait_healthy' };
    return { type: 'none' };
  };

  const handleStart = async () => {
    setError(null);
    let body: Record<string, unknown>;

    if (mode === 'pipeline') {
      if (!buildDesc && !pipelineExecId) {
        setError('Pipeline needs a build description or execute workflow ID');
        return;
      }
      const phases: Record<string, unknown> = { reflect: { reflection_workflow_id: null } };
      if (buildDesc) {
        phases.build = { description: buildDesc, ...(buildContext ? { context: buildContext } : {}) };
      }
      if (pipelineExecId) phases.execute_workflow_id = pipelineExecId;
      if (enableFixes) {
        phases.implement_fixes = {
          ...(fixContext ? { additional_context: fixContext } : {}),
          ...(fixTimeout !== 600 ? { timeout_secs: fixTimeout } : {}),
        };
      }
      body = { max_iterations: maxIter, between_iterations: buildBetween(), phases };
    } else {
      if (!selectedWorkflowId) {
        setError('Select a workflow first');
        return;
      }
      let es: Record<string, unknown>;
      if (exitStrategy === 'reflection') es = { type: 'reflection', reflection_workflow_id: null };
      else if (exitStrategy === 'workflow_verification') es = { type: 'workflow_verification' };
      else es = { type: 'fixed_iterations' };

      body = { workflow_id: selectedWorkflowId, max_iterations: maxIter, exit_strategy: es, between_iterations: buildBetween() };
    }

    try {
      await api.wlStart(body);
      const s = await api.wlStatus();
      setStatus(s);
    } catch (err) {
      setError(String(err));
    }
  };

  const handleStop = async () => {
    try {
      await api.wlStop();
      const s = await api.wlStatus();
      setStatus(s);
    } catch (err) {
      setError(String(err));
    }
  };

  const running = status?.running ?? false;
  const phase = status?.phase || 'idle';
  const cfgMax = status?.config?.max_iterations ?? maxIter;
  const progressPct = running && cfgMax > 0 ? Math.min(100, Math.round(((status?.current_iteration ?? 0) / cfgMax) * 100)) : (phase === 'complete' ? 100 : 0);
  const isPipelineMode = !!(status?.config?.phases);

  if (loading) {
    return <div style={{ padding: '2rem', color: 'var(--text-muted)' }}>Loading...</div>;
  }

  return (
    <div>
      <div className="page-header">
        <h1 className="page-title">Workflow Loop</h1>
        <div className="flex gap-2 items-center">
          {running ? (
            <>
              <span style={phaseBadgeStyle(phase)}>{phase.replace(/_/g, ' ')}</span>
              <span className="text-mono" style={{ fontSize: '0.85rem', color: 'var(--warning)' }}>
                Iteration {status?.current_iteration ?? 0} / {cfgMax}
              </span>
              <button className="btn" onClick={handleStop} style={{ background: 'var(--danger)', color: '#fff' }}>
                Stop
              </button>
            </>
          ) : (
            <button className="btn btn-primary" onClick={handleStart}>Run Loop</button>
          )}
        </div>
      </div>

      {error && (
        <div className="card mb-2" style={{ borderLeft: '3px solid var(--danger)', color: 'var(--danger)', fontSize: '0.85rem' }}>
          {error}
        </div>
      )}

      {/* Progress bar when running */}
      {(running || phase === 'complete') && (
        <div className="card mb-2" style={{ borderLeft: `3px solid ${phaseColor(phase)}` }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: '1rem' }}>
            <span style={phaseBadgeStyle(phase)}>{phase.replace(/_/g, ' ')}</span>
            <div style={{ flex: 1, background: 'var(--bg-tertiary)', borderRadius: 4, height: 8 }}>
              <div style={{
                width: `${progressPct}%`,
                background: phase === 'complete' ? 'var(--success)' : 'var(--accent)',
                height: '100%',
                borderRadius: 4,
                transition: 'width 0.3s ease',
              }} />
            </div>
            <span className="text-mono" style={{ fontSize: '0.8rem', color: 'var(--text-muted)' }}>
              {status?.current_iteration ?? 0}/{cfgMax}
            </span>
            {isPipelineMode && <span className="badge badge-warning">Pipeline</span>}
          </div>
          {status?.error && (
            <div style={{ marginTop: 8, fontSize: '0.8rem', color: 'var(--danger)' }}>{status.error}</div>
          )}
        </div>
      )}

      {/* Config form (hidden while running) */}
      {!running && (
        <div className="card mb-2">
          <div className="card-header">
            <span className="card-title">Configuration</span>
          </div>

          {/* Mode selector */}
          <div style={{ display: 'flex', gap: '1rem', marginBottom: '1rem' }}>
            <label style={{ display: 'flex', alignItems: 'center', gap: 4, fontSize: '0.85rem', cursor: 'pointer' }}>
              <input type="radio" name="wlMode" checked={mode === 'simple'} onChange={() => handleModeChange('simple')} /> Simple
            </label>
            <label style={{ display: 'flex', alignItems: 'center', gap: 4, fontSize: '0.85rem', cursor: 'pointer' }}>
              <input type="radio" name="wlMode" checked={mode === 'pipeline'} onChange={() => handleModeChange('pipeline')} /> Pipeline
            </label>
          </div>

          <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: '1.5rem' }}>
            {/* Left column: mode-specific config */}
            <div>
              {mode === 'simple' ? (
                <>
                  <div style={rowStyle}>
                    <span style={labelStyle}>Workflow</span>
                    <select value={selectedWorkflowId} onChange={e => setSelectedWorkflowId(e.target.value)} style={{ ...selectStyle, flex: 1, maxWidth: 250 }}>
                      <option value="">Select a workflow...</option>
                      {workflows.map(wf => (
                        <option key={wf.id} value={wf.id}>
                          {wf.name || wf.id} ({(wf.steps?.length ?? 0)} steps)
                        </option>
                      ))}
                    </select>
                  </div>
                  <div style={rowStyle}>
                    <span style={labelStyle}>Exit Strategy</span>
                    <select value={exitStrategy} onChange={e => setExitStrategy(e.target.value)} style={selectStyle}>
                      <option value="fixed_iterations">Fixed Iterations</option>
                      <option value="reflection">Reflection (0 fixes = exit)</option>
                      <option value="workflow_verification">Verification (pass = exit)</option>
                    </select>
                  </div>
                </>
              ) : (
                <>
                  <div style={{ marginBottom: 8 }}>
                    <div style={{ ...labelStyle, marginBottom: 4 }}>Build Description</div>
                    <textarea
                      value={buildDesc}
                      onChange={e => setBuildDesc(e.target.value)}
                      placeholder="Describe the workflow to generate..."
                      rows={3}
                      style={textareaStyle}
                    />
                  </div>
                  <div style={{ marginBottom: 8 }}>
                    <div style={{ ...labelStyle, marginBottom: 4 }}>Build Context (optional)</div>
                    <textarea
                      value={buildContext}
                      onChange={e => setBuildContext(e.target.value)}
                      placeholder="Additional context for the builder..."
                      rows={2}
                      style={textareaStyle}
                    />
                  </div>
                  <div style={rowStyle}>
                    <span style={labelStyle}>Execute Workflow (fallback)</span>
                    <select value={pipelineExecId} onChange={e => setPipelineExecId(e.target.value)} style={{ ...selectStyle, maxWidth: 200 }}>
                      <option value="">None (use build)</option>
                      {workflows.map(wf => (
                        <option key={wf.id} value={wf.id}>
                          {wf.name || wf.id}
                        </option>
                      ))}
                    </select>
                  </div>
                  <div style={{ display: 'flex', alignItems: 'center', gap: 6, padding: '6px 0', fontSize: '0.85rem' }}>
                    <input type="checkbox" checked={enableFixes} onChange={e => setEnableFixes(e.target.checked)} />
                    <span style={{ color: 'var(--text-secondary)' }}>Enable Fix Implementation</span>
                  </div>
                  {enableFixes && (
                    <>
                      <div style={{ marginBottom: 8 }}>
                        <div style={{ ...labelStyle, marginBottom: 4 }}>Fix Additional Context (optional)</div>
                        <textarea
                          value={fixContext}
                          onChange={e => setFixContext(e.target.value)}
                          placeholder="Extra instructions for the fix agent..."
                          rows={2}
                          style={textareaStyle}
                        />
                      </div>
                      <div style={rowStyle}>
                        <span style={labelStyle}>Fix Timeout (seconds)</span>
                        <input type="number" value={fixTimeout} onChange={e => setFixTimeout(Number(e.target.value))} min={60} max={3600} style={inputStyle} />
                      </div>
                    </>
                  )}
                </>
              )}
            </div>

            {/* Right column: shared config */}
            <div>
              <div style={rowStyle}>
                <span style={labelStyle}>Between Iterations</span>
                <select value={between} onChange={e => setBetween(e.target.value)} style={selectStyle}>
                  <option value="restart_on_signal">Restart on Signal (rebuild)</option>
                  <option value="restart_on_signal_no_rebuild">Restart on Signal (no rebuild)</option>
                  <option value="restart_runner">Always Restart (rebuild)</option>
                  <option value="restart_runner_no_rebuild">Always Restart (no rebuild)</option>
                  <option value="wait_healthy">Wait for Healthy</option>
                  <option value="none">None</option>
                </select>
              </div>
              <div style={rowStyle}>
                <span style={labelStyle}>Max Iterations</span>
                <input type="number" value={maxIter} onChange={e => setMaxIter(Number(e.target.value))} min={1} max={50} style={inputStyle} />
              </div>
            </div>
          </div>
        </div>
      )}

      {/* Iteration History */}
      <div className="card">
        <div className="card-header">
          <span className="card-title">Iteration History</span>
          <span className="text-muted" style={{ fontSize: '0.8rem' }}>{history.length} iterations</span>
        </div>
        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th>#</th>
                <th>Duration</th>
                <th>Exit</th>
                <th>Reason</th>
                <th>Pipeline</th>
              </tr>
            </thead>
            <tbody>
              {history.map(iter => {
                const exit = iter.exit_check;
                const duration = iter.started_at && iter.completed_at
                  ? ((new Date(iter.completed_at).getTime() - new Date(iter.started_at).getTime()) / 1000).toFixed(1) + 's'
                  : '--';
                const meta: string[] = [];
                if (iter.fix_count != null) meta.push(`fixes=${iter.fix_count}`);
                if (iter.fixes_implemented != null) meta.push(`applied=${iter.fixes_implemented ? 'yes' : 'no'}`);
                if (iter.rebuild_triggered) meta.push('rebuild');
                if (iter.generated_workflow_id) meta.push('built');
                return (
                  <tr key={iter.iteration}>
                    <td style={{ color: 'var(--accent)', fontWeight: 600 }}>#{iter.iteration}</td>
                    <td>{duration}</td>
                    <td>
                      <span className={exit?.should_exit ? 'text-success' : 'text-warning'}>
                        {exit?.should_exit ? 'EXIT' : 'CONTINUE'}
                      </span>
                    </td>
                    <td style={{ maxWidth: 300, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', fontFamily: '-apple-system, sans-serif', fontSize: '0.8rem' }}>
                      {exit?.reason || '--'}
                    </td>
                    <td style={{ fontFamily: '-apple-system, sans-serif', fontSize: '0.75rem', color: 'var(--text-muted)' }}>
                      {meta.length > 0 ? meta.join(', ') : '--'}
                    </td>
                  </tr>
                );
              })}
              {history.length === 0 && (
                <tr><td colSpan={5} style={{ textAlign: 'center', color: 'var(--text-muted)' }}>No iterations yet</td></tr>
              )}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
}
