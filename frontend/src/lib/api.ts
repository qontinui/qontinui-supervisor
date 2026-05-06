const BASE = '';

async function fetchJson<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`${BASE}${path}`, init);
  if (!res.ok) {
    const body = await res.text().catch(() => '');
    throw new Error(`${res.status} ${res.statusText}${body ? `: ${body.slice(0, 200)}` : ''}`);
  }
  const text = await res.text();
  try {
    return JSON.parse(text) as T;
  } catch (e) {
    throw new Error(
      `Failed to parse JSON from ${path}: ${e instanceof Error ? e.message : String(e)}. Response body: ${text.slice(0, 200)}`,
    );
  }
}

export interface ServiceSummary {
  service: string;
  total_requests: number;
  avg_duration_ms: number;
  p50_duration_ms: number;
  p95_duration_ms: number;
  p99_duration_ms: number;
  error_count: number;
  error_rate: number;
}

export interface EndpointSummary {
  service: string;
  http_method: string;
  http_route: string;
  request_count: number;
  avg_duration_ms: number;
  p50_duration_ms: number;
  p95_duration_ms: number;
  p99_duration_ms: number;
  error_count: number;
}

export interface SlowRequest {
  id: number;
  service: string;
  http_method: string;
  http_route: string;
  duration_ms: number;
  http_status_code: number | null;
  start_ts: string;
  request_id: string | null;
  error: string | null;
}

export interface TimelineBucket {
  bucket: string;
  service: string;
  request_count: number;
  avg_duration_ms: number;
  p95_duration_ms: number;
  error_count: number;
}

export interface CompareResult {
  http_method: string;
  http_route: string;
  before_count: number;
  before_p50: number;
  before_p95: number;
  after_count: number;
  after_p50: number;
  after_p95: number;
  p50_change_pct: number;
  p95_change_pct: number;
}

export interface TraceSpan {
  id: number;
  service: string;
  name: string;
  start_ts: string;
  end_ts: string | null;
  duration_ms: number | null;
  http_method: string | null;
  http_route: string | null;
  http_status_code: number | null;
  success: boolean;
  error: string | null;
  attributes: string | null;
}

interface IngestResult {
  total_new_spans: number;
  files_processed: { file: string; new_spans: number; errors: number }[];
}

/// UI-level error surfaced by a runner's /health endpoint. Populated by the
/// runner's React error boundary and passed through by the supervisor's
/// background health refresher. See qontinui-supervisor/src/health_cache.rs
/// (`UiErrorSummary`) for the source of truth.
export interface UiErrorSummary {
  message: string;
  digest?: string | null;
  stack?: string | null;
  component_stack?: string | null;
  first_seen: string;
  reported_at: string;
  count: number;
}

/// Most recent Rust crash dump surfaced by a runner's /health endpoint.
/// The field names are camelCase because the runner serializes
/// `RecentCrash` with `serde(rename_all = "camelCase")`; the supervisor passes
/// the payload through verbatim. Non-unwinding panics abort the process
/// before the React error boundary sees them, so a restarted runner only
/// looks "errored" via this object. See qontinui-supervisor/src/health_cache.rs
/// (`RecentCrashSummary`) for the source of truth.
export interface RecentCrashSummary {
  filePath: string;
  reportedAt: string;
  panicLocation?: string | null;
  panicMessage?: string | null;
  thread?: string | null;
}

/// Phase 2c (Item 9) stale-binary summary. Surfaced on `/runners` and
/// `/runners/{id}/logs` responses when a pool slot holds a binary that's
/// more than 30 seconds newer than the copy the running runner was started
/// from. `null`/absent is the normal state — the running binary is as fresh
/// as or newer than any available slot binary. The dashboard renders this as
/// a yellow "stale binary" badge next to the runner name, with a click-through
/// to restart the runner (picking up the newer build). See
/// `qontinui-supervisor::process::manager::StaleBinary` for the Rust source
/// of truth; field names are snake_case on the wire (default serde).
export interface StaleBinarySummary {
  /// Unix millis of the copy the supervisor made at start time
  /// (`target/debug/qontinui-runner-<id>.exe`).
  running_mtime_ms: number;
  /// Unix millis of the newest `target-pool/slot-*/debug/qontinui-runner.exe`.
  slot_mtime_ms: number;
  /// Which slot (0, 1, 2, ...) holds the newer build.
  slot_id: number;
  /// `slot_mtime - running_mtime` in whole seconds. Always > 30 when surfaced.
  age_delta_secs: number;
}

/// Phase 2b startup-panic telemetry. Parsed from the runner's
/// `runner-panic.log` when the supervisor observes a non-zero exit AND a
/// fresh panic file is on disk (see
/// `qontinui-supervisor::process::panic_log::RecentPanic`). Distinct from
/// `RecentCrashSummary`, which is the on-runtime WebView2 crash dump polled
/// via /health. Field names are snake_case on the wire (default serde).
export interface RecentPanicSummary {
  /// RFC3339 timestamp the panic was written at.
  timestamp: string;
  /// Panic payload (first arg to `panic!()` or the implicit
  /// assert/unwrap message).
  payload: string;
  /// `file.rs:line:col` where the panic fired, if known.
  location?: string | null;
  /// Thread name, or null if the thread was unnamed.
  thread?: string | null;
  /// First 15 frames of the backtrace, joined with `\n`.
  backtrace_preview?: string | null;
  /// Runner id taken from the panic-log header.
  runner_id?: string | null;
  /// Runner PID at the time of the panic.
  pid?: number | null;
  /// Runner binary version that panicked.
  version?: string | null;
  /// Absolute path to the source file on disk — useful when diagnosing
  /// from the supervisor CLI.
  file_path: string;
}

/// Supervisor-derived status for a runner. Serialized via serde's `tag=kind`
/// so variants with payloads (degraded/errored) carry `reason` as a sibling
/// field.
export type RunnerDerivedStatus =
  | { kind: 'healthy' }
  | { kind: 'degraded'; reason: string }
  | { kind: 'errored'; reason: string }
  | { kind: 'offline' }
  | { kind: 'starting' };

/// Wire-format classification of a runner. Mirrors
/// `qontinui_types::wire::runner_kind::RunnerKind`. Serde uses
/// `tag = "type"` (not `"kind"`) so when this is embedded as a `kind` field
/// on a parent struct the on-the-wire shape is `"kind": {"type": "primary"}`
/// rather than the doubly-nested `"kind": {"kind": "primary"}`.
export type RunnerKindWire =
  | { type: 'primary' }
  | { type: 'named'; name: string }
  | { type: 'temp'; id: string }
  | { type: 'external' };

/// One runner's entry as surfaced by the supervisor `/health` endpoint's
/// `runners[]` array and by `/runners`. Mirrors
/// `qontinui-supervisor::routes::health::RunnerInstanceHealth`.
export interface RunnerInstanceHealth {
  id: string;
  name: string;
  port: number;
  kind: RunnerKindWire;
  running: boolean;
  pid?: number;
  started_at?: string;
  api_responding: boolean;
  ui_error?: UiErrorSummary | null;
  recent_crash?: RecentCrashSummary | null;
  derived_status: RunnerDerivedStatus;
}

export interface HealthResponse {
  status: string;
  runner: {
    running: boolean;
    pid?: number;
    started_at?: string;
    api_responding: boolean;
  };
  ports: {
    api_port: { port: number; in_use: boolean };
  };
  watchdog: {
    enabled: boolean;
    restart_attempts: number;
    last_restart_at?: string;
    disabled_reason?: string;
    crash_count: number;
  };
  build: {
    in_progress: boolean;
    available_slots: number;
    error_detected: boolean;
    last_error?: string;
    last_build_at?: string;
  };
  expo: {
    running: boolean;
    pid?: number;
    port: number;
    configured: boolean;
  };
  supervisor: {
    version: string;
    project_dir: string;
  };
  runners?: RunnerInstanceHealth[];
}

export interface DevStartResponse {
  status: string;
  flag: string;
  stdout: string;
  stderr: string;
  exit_code: number | null;
}

export interface LogFileResponse {
  file: string;
  type: string;
  content: string;
  lines: number;
}

export interface ExpoStatus {
  running: boolean;
  pid: number | null;
  port: number;
  started_at: string | null;
  configured: boolean;
}

export interface LogEntry {
  timestamp: string;
  level: string;
  source: string;
  message: string;
}

// Evaluation types
export interface TestPrompt {
  id: string;
  prompt: string;
  category: string;
  complexity: string;
  expected_phases: string[] | null;
  expected_step_types: string[] | null;
  tags: string[] | null;
  ground_truth_json: string | null;
  enabled: boolean;
  created_at: string;
  updated_at: string;
}

export interface EvalRunSummary {
  id: string;
  mode: string;
  status: string;
  prompts_total: number;
  prompts_completed: number;
  // Combined averages
  avg_overall_score: number | null;
  avg_structural: number | null;
  avg_command_accuracy: number | null;
  avg_phase_flow: number | null;
  avg_step_completeness: number | null;
  avg_prompt_quality: number | null;
  avg_determinism: number | null;
  // Ground-truth prompt averages
  gt_avg_overall: number | null;
  gt_avg_structural: number | null;
  gt_avg_command_accuracy: number | null;
  gt_avg_phase_flow: number | null;
  gt_avg_step_completeness: number | null;
  gt_avg_prompt_quality: number | null;
  gt_avg_determinism: number | null;
  gt_count: number | null;
  // Generic prompt averages
  gen_avg_overall: number | null;
  gen_avg_structural: number | null;
  gen_avg_command_accuracy: number | null;
  gen_avg_phase_flow: number | null;
  gen_avg_step_completeness: number | null;
  gen_avg_prompt_quality: number | null;
  gen_avg_determinism: number | null;
  gen_count: number | null;

  error: string | null;
  started_at: string;
  completed_at: string | null;
}

export interface EvalResultItem {
  id: number;
  run_id: string;
  test_prompt_id: string;
  generated_workflow_json: string | null;
  task_run_id: string | null;
  workflow_id: string | null;
  structural_correctness: number | null;
  command_accuracy: number | null;
  phase_flow_logic: number | null;
  step_completeness: number | null;
  prompt_quality: number | null;
  determinism: number | null;
  overall_score: number | null;
  score_rationales: string | null;
  generation_error: string | null;
  scoring_error: string | null;
  generation_duration_ms: number | null;
  scoring_duration_ms: number | null;
  started_at: string;
  completed_at: string | null;
}

export interface EvalRunWithResults extends EvalRunSummary {
  results: EvalResultItem[];
}

export interface EvalStatus {
  running: boolean;
  current_run_id: string | null;
  continuous_mode: boolean;
  continuous_interval_secs: number;
  current_prompt_index: number;
  total_prompts: number;
}

export interface PromptComparison {
  test_prompt_id: string;
  baseline_overall: number | null;
  current_overall: number | null;
  delta: number | null;
  regression: boolean;
  improvement: boolean;
}

export interface CompareReport {
  current_run_id: string;
  baseline_run_id: string;
  per_prompt: PromptComparison[];
  aggregate: {
    avg_overall_delta: number | null;
    regressions: number;
    improvements: number;
    unchanged: number;
  };
}

export interface MessageResponse {
  ok: boolean;
  message: string;
}

// Workflow Loop types
export interface WorkflowLoopStatus {
  running: boolean;
  config: WorkflowLoopConfig | null;
  current_iteration: number;
  phase: string;
  started_at: string | null;
  error: string | null;
  iteration_count: number;
  restart_signaled: boolean;
  build_task_run_id?: string | null;
  execute_task_run_id?: string | null;
}

export interface Checkpoint {
  step_index: number;
  step_name: string;
  status: string;
  phase: string;
  duration_ms: number | null;
  error: string | null;
  stage_index: number;
}

export interface WorkflowLoopConfig {
  workflow_id?: string;
  max_iterations: number;
  exit_strategy?: { type: string; reflection_workflow_id?: string | null };
  between_iterations: { type: string; rebuild?: boolean };
  phases?: PipelinePhases;
}

export interface PipelinePhases {
  build?: { description: string; context?: string };
  execute_workflow_id?: string;
  reflect: { reflection_workflow_id: string | null };
  implement_fixes?: {
    additional_context?: string;
    timeout_secs?: number;
  };
}

export interface IterationResult {
  iteration: number;
  started_at: string;
  completed_at: string | null;
  task_run_id: string | null;
  exit_check: { should_exit: boolean; reason: string } | null;
  generated_workflow_id?: string;
  reflection_task_run_id?: string;
  fix_count?: number;
  fixes_implemented?: boolean;
  rebuild_triggered?: boolean;
}

export interface WorkflowLoopHistory {
  iterations: IterationResult[];
}

export interface UnifiedWorkflow {
  id: string;
  name: string;
  steps?: unknown[];
}

// Velocity Test types
export interface VtStatus {
  running: boolean;
  current_run_id: string | null;
  current_test_index: number;
  total_tests: number;
}

export interface VtRun {
  id: string;
  started_at: string;
  completed_at: string | null;
  overall_score: number | null;
  status: string;
  tests_total: number;
  tests_completed: number;
}

export interface VtResult {
  id: number;
  run_id: string;
  test_name: string;
  page_url: string;
  load_time_ms: number | null;
  console_errors: number;
  element_found: boolean;
  score: number | null;
  error: string | null;
  tested_at: string;
  // Diagnostic fields
  api_response_time_ms: number | null;
  api_status_code: number | null;
  ttfb_ms: number | null;
  dom_interactive_ms: number | null;
  dom_complete_ms: number | null;
  fcp_ms: number | null;
  long_task_count: number;
  long_task_total_ms: number;
  resource_count: number;
  total_transfer_size_bytes: number;
  slowest_resource_ms: number;
  bottleneck: string | null;
  diagnostics_json: string | null;
}

export interface VtDiagnostics {
  navigation?: {
    ttfbMs: number;
    domInteractiveMs: number;
    domCompleteMs: number;
    loadEventMs: number;
    redirectMs: number;
    dnsMs: number;
    tcpMs: number;
  };
  resources?: Array<{
    name: string;
    initiatorType: string;
    startTime: number;
    duration: number;
    transferSize: number;
    ttfbMs: number;
    downloadMs: number;
  }>;
  paint?: Array<{
    name: string;
    startTime: number;
  }>;
  longTasks?: Array<{
    duration: number;
    startTime?: number;
    [key: string]: unknown;
  }>;
  scriptAttribution?: Array<{
    sourceURL: string;
    sourceFunctionName: string;
    duration: number;
    invoker: string;
  }>;
}

export interface VtRunWithResults extends VtRun {
  results: VtResult[];
}

export interface VtTrendPoint {
  run_id: string;
  started_at: string;
  overall_score: number | null;
}

// Velocity Improvement types
export interface VelocityImprovementStatus {
  running: boolean;
  phase: string;
  current_iteration: number;
  max_iterations: number;
  target_score: number;
  started_at: string | null;
  error: string | null;
}

export interface VelocityImprovementIteration {
  iteration: number;
  started_at: string;
  completed_at: string | null;
  run_id: string | null;
  overall_score: number | null;
  per_page_scores: Array<{ name: string; score: number; bottleneck: string }>;
  fix_applied: boolean;
  fix_summary: string | null;
  exit_reason: string | null;
}

export interface VelocityImprovementHistory {
  iterations: VelocityImprovementIteration[];
}

// Web Fleet types — mirrors the qontinui-web `RunnerResponse` schema at
// `qontinui-web/backend/app/schemas/runner_fleet.py`. Read-only; the Fleet tab
// proxies `GET /api/v1/runners` via supervisor's `/web-fleet` endpoint.
export interface WebFleetRunner {
  id: string;
  user_id: string;
  name: string;
  hostname: string;
  port: number;
  capabilities: string[];
  server_mode: boolean;
  restate_enabled: boolean;
  restate_healthy: boolean;
  last_heartbeat: string | null;
  status: string;
  // Phase 3J.5 + post-3J follow-up heartbeat extensions. All optional — the
  // web backend leaves them null until the runner heartbeats in with the
  // extended shape. Snake-case on the wire (matches the Python schema).
  derived_status?: string | null;
  ui_error?: {
    message: string;
    stack?: string | null;
    component_stack?: string | null;
    digest?: string | null;
    first_seen: string;
    reported_at: string;
    count: number;
  } | null;
  recent_crash?: {
    file_path: string;
    reported_at: string;
    panic_location?: string | null;
    panic_message?: string | null;
    thread?: string | null;
  } | null;
  created_at: string;
}

// Runner Monitor types
export interface RunnerTaskRun {
  id: string;
  status: string;
  prompt?: string;
  workflow_id?: string;
  started_at?: string;
  [key: string]: unknown;
}

export const api = {
  // Velocity
  ingest: () => fetchJson<IngestResult>('/velocity/ingest', { method: 'POST' }),
  summary: (params?: string) =>
    fetchJson<ServiceSummary[]>(`/velocity/summary${params ? `?${params}` : ''}`),
  endpoints: (params?: string) =>
    fetchJson<EndpointSummary[]>(`/velocity/endpoints${params ? `?${params}` : ''}`),
  slow: (params?: string) =>
    fetchJson<SlowRequest[]>(`/velocity/slow${params ? `?${params}` : ''}`),
  timeline: (params?: string) =>
    fetchJson<TimelineBucket[]>(`/velocity/timeline${params ? `?${params}` : ''}`),
  compare: (params: string) => fetchJson<CompareResult[]>(`/velocity/compare?${params}`),
  trace: (requestId: string) =>
    fetchJson<TraceSpan[]>(`/velocity/trace/${encodeURIComponent(requestId)}`),

  // Evaluation
  evalStatus: () => fetchJson<EvalStatus>('/eval/status'),
  evalStart: (promptIds?: string[]) =>
    fetchJson<MessageResponse>('/eval/start', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ prompt_ids: promptIds ?? null }),
    }),
  evalStop: () => fetchJson<MessageResponse>('/eval/stop', { method: 'POST' }),
  evalContinuousStart: (intervalSecs: number) =>
    fetchJson<MessageResponse>('/eval/continuous/start', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ interval_secs: intervalSecs }),
    }),
  evalContinuousStop: () => fetchJson<MessageResponse>('/eval/continuous/stop', { method: 'POST' }),
  evalRuns: () => fetchJson<EvalRunSummary[]>('/eval/runs'),
  evalRun: (id: string) => fetchJson<EvalRunWithResults>(`/eval/runs/${id}`),
  evalCompare: (id: string, baselineId: string) =>
    fetchJson<CompareReport>(`/eval/runs/${id}/compare/${baselineId}`),
  evalTestSuite: () => fetchJson<TestPrompt[]>('/eval/test-suite'),
  evalTestSuiteAdd: (prompt: TestPrompt) =>
    fetchJson<MessageResponse>('/eval/test-suite', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(prompt),
    }),
  evalTestSuiteUpdate: (id: string, prompt: TestPrompt) =>
    fetchJson<MessageResponse>(`/eval/test-suite/${id}`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(prompt),
    }),
  evalTestSuiteDelete: (id: string) =>
    fetchJson<MessageResponse>(`/eval/test-suite/${id}`, { method: 'DELETE' }),
  evalSetGroundTruth: (promptId: string, workflowId: string) =>
    fetchJson<MessageResponse>(`/eval/test-suite/${promptId}/ground-truth`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ workflow_id: workflowId }),
    }),
  evalClearGroundTruth: (promptId: string) =>
    fetchJson<MessageResponse>(`/eval/test-suite/${promptId}/ground-truth`, {
      method: 'DELETE',
    }),

  // Velocity Tests
  vtStatus: () => fetchJson<VtStatus>('/velocity-tests/status'),
  vtStart: () => fetchJson<MessageResponse>('/velocity-tests/start', { method: 'POST' }),
  vtStop: () => fetchJson<MessageResponse>('/velocity-tests/stop', { method: 'POST' }),
  vtRuns: () => fetchJson<VtRun[]>('/velocity-tests/runs'),
  vtRun: (id: string) => fetchJson<VtRunWithResults>(`/velocity-tests/runs/${id}`),
  vtTrend: (limit?: number) =>
    fetchJson<VtTrendPoint[]>(`/velocity-tests/trend${limit ? `?limit=${limit}` : ''}`),

  // Workflow Loop
  wlStatus: () => fetchJson<WorkflowLoopStatus>('/workflow-loop/status'),
  wlHistory: () => fetchJson<WorkflowLoopHistory>('/workflow-loop/history'),
  wlStart: (config: Record<string, unknown>) =>
    fetchJson<unknown>('/workflow-loop/start', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(config),
    }),
  wlStop: () => fetchJson<unknown>('/workflow-loop/stop', { method: 'POST' }),
  wlCheckpoints: (taskRunId: string) =>
    fetchJson<{ checkpoints: Checkpoint[] }>(`/workflow-loop/checkpoints/${taskRunId}`),
  wlWorkflows: () =>
    fetch('http://127.0.0.1:9876/unified-workflows')
      .then((r) => r.json())
      .then((d: { data?: UnifiedWorkflow[] }) => (d.data || d) as UnifiedWorkflow[]),

  // Velocity Improvement
  viStatus: () => fetchJson<VelocityImprovementStatus>('/velocity-improvement/status'),
  viStart: (config: Record<string, unknown>) =>
    fetchJson<MessageResponse>('/velocity-improvement/start', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(config),
    }),
  viStop: () =>
    fetchJson<MessageResponse>('/velocity-improvement/stop', {
      method: 'POST',
    }),
  viHistory: () => fetchJson<VelocityImprovementHistory>('/velocity-improvement/history'),

  // Supervisor
  health: () => fetchJson<HealthResponse>('/health'),
  runnerRestart: (rebuild: boolean) =>
    fetchJson<unknown>('/runner/restart', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ rebuild }),
    }),
  devStartStatus: () =>
    fetchJson<{
      services: { name: string; port: number; available: boolean }[];
    }>('/dev-start/status'),
  devStartAction: (action: string) =>
    fetchJson<DevStartResponse>(`/dev-start/${action}`, { method: 'POST' }),
  runnerStop: () => fetchJson<unknown>('/runner/stop', { method: 'POST' }),
  listRunners: () =>
    fetchJson<
      {
        id: string;
        name: string;
        port: number;
        kind: RunnerKindWire;
        protected: boolean;
        running: boolean;
        pid?: number;
        api_responding?: boolean;
        ui_error?: UiErrorSummary | null;
        recent_crash?: RecentCrashSummary | null;
        recent_panic?: RecentPanicSummary | null;
        stale_binary?: StaleBinarySummary | null;
        derived_status?: RunnerDerivedStatus;
      }[]
    >('/runners'),
  protectRunner: (id: string, isProtected: boolean) =>
    fetchJson<unknown>(`/runners/${encodeURIComponent(id)}/protect`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ protected: isProtected }),
    }),
  addRunner: (name: string, port: number) =>
    fetchJson<{ id: string; name: string; port: number }>('/runners', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name, port }),
    }),
  startRunner: (id: string) =>
    fetchJson<unknown>(`/runners/${encodeURIComponent(id)}/start`, { method: 'POST' }),
  stopRunner: (id: string) =>
    fetchJson<unknown>(`/runners/${encodeURIComponent(id)}/stop`, { method: 'POST' }),
  restartRunnerById: (id: string, rebuild: boolean) =>
    fetchJson<unknown>(`/runners/${encodeURIComponent(id)}/restart`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ rebuild, source: 'manual' }),
    }),
  removeRunner: (id: string) =>
    fetchJson<unknown>(`/runners/${encodeURIComponent(id)}`, { method: 'DELETE' }),
  spawnInstance: (name: string, port: number) =>
    fetchJson<{ success: boolean; data: { id: string; port: number; pid: number } }>(
      '/runner-api/instances/spawn',
      {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ name, port }),
      },
    ),
  spawnNamedRunner: (opts: {
    name: string;
    port?: number;
    rebuild?: boolean;
    wait?: boolean;
    protected?: boolean;
    requester_id?: string;
  }) =>
    fetchJson<{
      id: string;
      name: string;
      port: number;
      status: string;
      api_url: string;
      ui_bridge_url: string;
      binary_mtime?: string;
    }>('/runners/spawn-named', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(opts),
    }),
  runnerFixAndRebuild: (prompt: string) =>
    fetchJson<unknown>('/runner/fix-and-rebuild', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ prompt }),
    }),

  // Logs
  logFile: (type: string, tailLines?: number) =>
    fetchJson<LogFileResponse>(`/logs/file/${type}${tailLines ? `?tail_lines=${tailLines}` : ''}`),

  // Runner Monitor (proxied to runner at port 9876)
  runnerHealth: () => fetchJson<Record<string, unknown>>('/runner-api/health'),
  runnerTaskRunsRunning: () => fetchJson<RunnerTaskRun[]>('/runner-api/task-runs/running'),
  runnerWorkflowState: (id: string) =>
    fetchJson<Record<string, unknown>>(
      `/runner-api/task-runs/${encodeURIComponent(id)}/workflow-state`,
    ),
  runnerTaskOutput: (id: string, tailChars = 15000) =>
    fetch(`/runner-api/task-runs/${encodeURIComponent(id)}/output?tail_chars=${tailChars}`).then(
      (r) => {
        if (!r.ok) throw new Error(`${r.status} ${r.statusText}`);
        return r.text();
      },
    ),
  runnerStopTask: (id: string) =>
    fetchJson<Record<string, unknown>>(`/runner-api/task-runs/${encodeURIComponent(id)}/stop`, {
      method: 'POST',
    }),

  // Web Fleet — proxies to {backend_url}/api/v1/runners with a user-supplied JWT.
  // Supervisor does not hold credentials; caller supplies them per-request.
  // On error, surfaces the backend's body so the user sees the real reason
  // (e.g. "401 invalid signature", "404 user not found") rather than just the
  // HTTP status line.
  webFleet: async (backendUrl: string, jwt: string): Promise<WebFleetRunner[]> => {
    const res = await fetch(
      `/web-fleet?backend_url=${encodeURIComponent(backendUrl)}`,
      { headers: { Authorization: `Bearer ${jwt}` } },
    );
    const text = await res.text();
    if (!res.ok) {
      // Try to extract `error` from JSON body; fall back to plain text or
      // finally the status line.
      let detail = text;
      try {
        const parsed = JSON.parse(text);
        if (parsed && typeof parsed === 'object' && 'error' in parsed) {
          detail = String((parsed as { error: unknown }).error);
        } else if (parsed && typeof parsed === 'object' && 'detail' in parsed) {
          detail = String((parsed as { detail: unknown }).detail);
        }
      } catch {
        // not JSON, use text as-is
      }
      throw new Error(
        `${res.status} ${res.statusText}${detail ? `: ${detail.slice(0, 500)}` : ''}`,
      );
    }
    try {
      return JSON.parse(text) as WebFleetRunner[];
    } catch (e) {
      throw new Error(
        `Failed to parse JSON from /web-fleet: ${
          e instanceof Error ? e.message : String(e)
        }. Response body: ${text.slice(0, 200)}`,
      );
    }
  },

  // Expo
  expoStart: () => fetchJson<unknown>('/expo/start', { method: 'POST' }),
  expoStop: () => fetchJson<unknown>('/expo/stop', { method: 'POST' }),
  expoStatus: () => fetchJson<ExpoStatus>('/expo/status'),
};
