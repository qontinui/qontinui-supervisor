const BASE = '';

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
  [key: string]: unknown;
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
  avg_overall_score: number | null;
  avg_structural: number | null;
  avg_command_accuracy: number | null;
  avg_phase_flow: number | null;
  avg_step_completeness: number | null;
  avg_prompt_quality: number | null;
  avg_determinism: number | null;
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
}

export interface VtRunWithResults extends VtRun {
  results: VtResult[];
}

export interface VtTrendPoint {
  run_id: string;
  started_at: string;
  overall_score: number | null;
}

export const api = {
  // Velocity
  ingest: () => fetchJson<IngestResult>('/velocity/ingest', { method: 'POST' }),
  summary: (params?: string) => fetchJson<ServiceSummary[]>(`/velocity/summary${params ? `?${params}` : ''}`),
  endpoints: (params?: string) => fetchJson<EndpointSummary[]>(`/velocity/endpoints${params ? `?${params}` : ''}`),
  slow: (params?: string) => fetchJson<SlowRequest[]>(`/velocity/slow${params ? `?${params}` : ''}`),
  timeline: (params?: string) => fetchJson<TimelineBucket[]>(`/velocity/timeline${params ? `?${params}` : ''}`),
  compare: (params: string) => fetchJson<CompareResult[]>(`/velocity/compare?${params}`),
  trace: (requestId: string) => fetchJson<TraceSpan[]>(`/velocity/trace/${encodeURIComponent(requestId)}`),

  // Evaluation
  evalStatus: () => fetchJson<EvalStatus>('/eval/status'),
  evalStart: (promptIds?: string[]) => fetchJson<MessageResponse>('/eval/start', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ prompt_ids: promptIds ?? null }),
  }),
  evalStop: () => fetchJson<MessageResponse>('/eval/stop', { method: 'POST' }),
  evalContinuousStart: (intervalSecs: number) => fetchJson<MessageResponse>('/eval/continuous/start', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ interval_secs: intervalSecs }),
  }),
  evalContinuousStop: () => fetchJson<MessageResponse>('/eval/continuous/stop', { method: 'POST' }),
  evalRuns: () => fetchJson<EvalRunSummary[]>('/eval/runs'),
  evalRun: (id: string) => fetchJson<EvalRunWithResults>(`/eval/runs/${id}`),
  evalCompare: (id: string, baselineId: string) => fetchJson<CompareReport>(`/eval/runs/${id}/compare/${baselineId}`),
  evalTestSuite: () => fetchJson<TestPrompt[]>('/eval/test-suite'),
  evalTestSuiteAdd: (prompt: TestPrompt) => fetchJson<MessageResponse>('/eval/test-suite', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(prompt),
  }),
  evalTestSuiteUpdate: (id: string, prompt: TestPrompt) => fetchJson<MessageResponse>(`/eval/test-suite/${id}`, {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(prompt),
  }),
  evalTestSuiteDelete: (id: string) => fetchJson<MessageResponse>(`/eval/test-suite/${id}`, { method: 'DELETE' }),

  // Velocity Tests
  vtStatus: () => fetchJson<VtStatus>('/velocity-tests/status'),
  vtStart: () => fetchJson<MessageResponse>('/velocity-tests/start', { method: 'POST' }),
  vtStop: () => fetchJson<MessageResponse>('/velocity-tests/stop', { method: 'POST' }),
  vtRuns: () => fetchJson<VtRun[]>('/velocity-tests/runs'),
  vtRun: (id: string) => fetchJson<VtRunWithResults>(`/velocity-tests/runs/${id}`),
  vtTrend: (limit?: number) => fetchJson<VtTrendPoint[]>(`/velocity-tests/trend${limit ? `?limit=${limit}` : ''}`),

  // Supervisor
  health: () => fetchJson<HealthResponse>('/health'),
  runnerRestart: (rebuild: boolean) => fetchJson<unknown>('/runner/restart', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ rebuild }),
  }),
  devStartStatus: () => fetchJson<Record<string, unknown>>('/dev-start/status'),
};
