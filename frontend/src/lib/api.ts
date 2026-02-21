const BASE = "";

async function fetchJson<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`${BASE}${path}`, init);
  if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
  return res.json();
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

export interface IngestResult {
  total_new_spans: number;
  files_processed: { file: string; new_spans: number; errors: number }[];
}

export interface HealthResponse {
  runner: { running: boolean; pid?: number };
  watchdog: { enabled: boolean };
  build: { in_progress: boolean; error_detected: boolean; last_error?: string };
}

export interface DevStartResponse {
  status: string;
  flag: string;
  stdout: string;
  stderr: string;
  exit_code: number | null;
}

export interface AiDebugResponse {
  status: string;
  message: string;
}

export interface LogFileResponse {
  file: string;
  type: string;
  content: string;
  lines: number;
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
  ingest: () => fetchJson<IngestResult>("/velocity/ingest", { method: "POST" }),
  summary: (params?: string) =>
    fetchJson<ServiceSummary[]>(
      `/velocity/summary${params ? `?${params}` : ""}`,
    ),
  endpoints: (params?: string) =>
    fetchJson<EndpointSummary[]>(
      `/velocity/endpoints${params ? `?${params}` : ""}`,
    ),
  slow: (params?: string) =>
    fetchJson<SlowRequest[]>(`/velocity/slow${params ? `?${params}` : ""}`),
  timeline: (params?: string) =>
    fetchJson<TimelineBucket[]>(
      `/velocity/timeline${params ? `?${params}` : ""}`,
    ),
  compare: (params: string) =>
    fetchJson<CompareResult[]>(`/velocity/compare?${params}`),
  trace: (requestId: string) =>
    fetchJson<TraceSpan[]>(`/velocity/trace/${encodeURIComponent(requestId)}`),

  // Evaluation
  evalStatus: () => fetchJson<EvalStatus>("/eval/status"),
  evalStart: (promptIds?: string[]) =>
    fetchJson<MessageResponse>("/eval/start", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ prompt_ids: promptIds ?? null }),
    }),
  evalStop: () => fetchJson<MessageResponse>("/eval/stop", { method: "POST" }),
  evalContinuousStart: (intervalSecs: number) =>
    fetchJson<MessageResponse>("/eval/continuous/start", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ interval_secs: intervalSecs }),
    }),
  evalContinuousStop: () =>
    fetchJson<MessageResponse>("/eval/continuous/stop", { method: "POST" }),
  evalRuns: () => fetchJson<EvalRunSummary[]>("/eval/runs"),
  evalRun: (id: string) => fetchJson<EvalRunWithResults>(`/eval/runs/${id}`),
  evalCompare: (id: string, baselineId: string) =>
    fetchJson<CompareReport>(`/eval/runs/${id}/compare/${baselineId}`),
  evalTestSuite: () => fetchJson<TestPrompt[]>("/eval/test-suite"),
  evalTestSuiteAdd: (prompt: TestPrompt) =>
    fetchJson<MessageResponse>("/eval/test-suite", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(prompt),
    }),
  evalTestSuiteUpdate: (id: string, prompt: TestPrompt) =>
    fetchJson<MessageResponse>(`/eval/test-suite/${id}`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(prompt),
    }),
  evalTestSuiteDelete: (id: string) =>
    fetchJson<MessageResponse>(`/eval/test-suite/${id}`, { method: "DELETE" }),
  evalSetGroundTruth: (promptId: string, workflowId: string) =>
    fetchJson<MessageResponse>(`/eval/test-suite/${promptId}/ground-truth`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ workflow_id: workflowId }),
    }),
  evalClearGroundTruth: (promptId: string) =>
    fetchJson<MessageResponse>(`/eval/test-suite/${promptId}/ground-truth`, {
      method: "DELETE",
    }),

  // Velocity Tests
  vtStatus: () => fetchJson<VtStatus>("/velocity-tests/status"),
  vtStart: () =>
    fetchJson<MessageResponse>("/velocity-tests/start", { method: "POST" }),
  vtStop: () =>
    fetchJson<MessageResponse>("/velocity-tests/stop", { method: "POST" }),
  vtRuns: () => fetchJson<VtRun[]>("/velocity-tests/runs"),
  vtRun: (id: string) =>
    fetchJson<VtRunWithResults>(`/velocity-tests/runs/${id}`),
  vtTrend: (limit?: number) =>
    fetchJson<VtTrendPoint[]>(
      `/velocity-tests/trend${limit ? `?limit=${limit}` : ""}`,
    ),

  // Workflow Loop
  wlStatus: () => fetchJson<WorkflowLoopStatus>("/workflow-loop/status"),
  wlHistory: () => fetchJson<WorkflowLoopHistory>("/workflow-loop/history"),
  wlStart: (config: Record<string, unknown>) =>
    fetchJson<unknown>("/workflow-loop/start", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(config),
    }),
  wlStop: () => fetchJson<unknown>("/workflow-loop/stop", { method: "POST" }),
  wlWorkflows: () =>
    fetch("http://127.0.0.1:9876/unified-workflows")
      .then((r) => r.json())
      .then(
        (d: { data?: UnifiedWorkflow[] }) => (d.data || d) as UnifiedWorkflow[],
      ),

  // Velocity Improvement
  viStatus: () =>
    fetchJson<VelocityImprovementStatus>("/velocity-improvement/status"),
  viStart: (config: Record<string, unknown>) =>
    fetchJson<MessageResponse>("/velocity-improvement/start", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(config),
    }),
  viStop: () =>
    fetchJson<MessageResponse>("/velocity-improvement/stop", {
      method: "POST",
    }),
  viHistory: () =>
    fetchJson<VelocityImprovementHistory>("/velocity-improvement/history"),

  // Supervisor
  health: () => fetchJson<HealthResponse>("/health"),
  runnerRestart: (rebuild: boolean) =>
    fetchJson<unknown>("/runner/restart", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ rebuild }),
    }),
  devStartStatus: () =>
    fetchJson<{
      services: { name: string; port: number; available: boolean }[];
    }>("/dev-start/status"),
  devStartAction: (action: string) =>
    fetchJson<DevStartResponse>(`/dev-start/${action}`, { method: "POST" }),
  runnerStop: () => fetchJson<unknown>("/runner/stop", { method: "POST" }),

  // AI Debug
  aiDebug: (prompt: string) =>
    fetchJson<AiDebugResponse>("/ai/debug", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ prompt }),
    }),

  // Logs
  logFile: (type: string, tailLines?: number) =>
    fetchJson<LogFileResponse>(
      `/logs/file/${type}${tailLines ? `?tail_lines=${tailLines}` : ""}`,
    ),

  // Runner Monitor (proxied to runner at port 9876)
  runnerHealth: () => fetchJson<Record<string, unknown>>("/runner-api/health"),
  runnerTaskRunsRunning: () =>
    fetchJson<RunnerTaskRun[]>("/runner-api/task-runs/running"),
  runnerWorkflowState: (id: string) =>
    fetchJson<Record<string, unknown>>(
      `/runner-api/task-runs/${encodeURIComponent(id)}/workflow-state`,
    ),
  runnerTaskOutput: (id: string, tailChars = 15000) =>
    fetch(
      `/runner-api/task-runs/${encodeURIComponent(id)}/output?tail_chars=${tailChars}`,
    ).then((r) => {
      if (!r.ok) throw new Error(`${r.status} ${r.statusText}`);
      return r.text();
    }),
  runnerStopTask: (id: string) =>
    fetchJson<Record<string, unknown>>(
      `/runner-api/task-runs/${encodeURIComponent(id)}/stop`,
      { method: "POST" },
    ),

  // Expo
  expoStart: () => fetchJson<unknown>("/expo/start", { method: "POST" }),
  expoStop: () => fetchJson<unknown>("/expo/stop", { method: "POST" }),
  expoStatus: () => fetchJson<Record<string, unknown>>("/expo/status"),
};
