import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import RunnerMonitor from './RunnerMonitor';
import { api } from '../lib/api';

// `GET /task-runs/running` changed from a bare array to an envelope
// `{ scope, task_runs }` (2026-08-29-no-single-answer-to-is-it-safe-to-restart-the-runner).
// These tests pin the client-side parse of that envelope and the operator-facing
// rendering of `scope`, which is the entire point of the change: an operator
// reading "no running tasks" must see what that statement is actually scoped to.
vi.mock('../lib/api', () => ({
  api: {
    runnerHealth: vi.fn().mockResolvedValue({ status: 'ok' }),
    runnerTaskRunsRunning: vi.fn(),
    runnerWorkflowState: vi.fn(),
    runnerTaskOutput: vi.fn(),
    runnerStopTask: vi.fn(),
  },
}));

const SCOPE_TEXT =
  'workflow task-runs on API port 9876; NOT a session census — see /restart-readiness';

beforeEach(() => {
  vi.clearAllMocks();
});

afterEach(() => {
  vi.clearAllMocks();
});

function renderPage() {
  return render(<RunnerMonitor />);
}

describe('RunnerMonitor task-runs envelope', () => {
  it('parses task_runs out of the envelope and renders the scope string', async () => {
    (api.runnerTaskRunsRunning as ReturnType<typeof vi.fn>).mockResolvedValue({
      scope: SCOPE_TEXT,
      task_runs: [
        { id: 'task-1', status: 'running', prompt: 'do the thing' },
        { id: 'task-2', status: 'running' },
      ],
    });

    renderPage();

    fireEvent.click(screen.getByRole('button', { name: /refresh/i }));

    await waitFor(() => {
      expect(screen.getByText('task-1')).toBeInTheDocument();
      expect(screen.getByText('task-2')).toBeInTheDocument();
    });

    // The active count badge reflects task_runs.length, unwrapped from the envelope.
    expect(screen.getByText('2 active')).toBeInTheDocument();

    // The scope string is surfaced near the list, not silently dropped.
    expect(screen.getByText(new RegExp(SCOPE_TEXT.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')))).toBeInTheDocument();
  });

  it('shows the scope alongside the empty state instead of an unqualified "no running tasks"', async () => {
    (api.runnerTaskRunsRunning as ReturnType<typeof vi.fn>).mockResolvedValue({
      scope: SCOPE_TEXT,
      task_runs: [],
    });

    renderPage();

    fireEvent.click(screen.getByRole('button', { name: /refresh/i }));

    await waitFor(() => {
      expect(screen.getByText('0 active')).toBeInTheDocument();
    });

    // Empty task_runs must not be readable as "runner idle" without qualification —
    // this is the false-idle reading the envelope change exists to prevent.
    const emptyState = screen.getByText(/No running task runs/);
    expect(emptyState.textContent).toContain(SCOPE_TEXT);
  });

  it('surfaces a malformed (non-envelope) response through the error path rather than silently showing empty', async () => {
    // Before this change, `Array.isArray(runs) ? runs : []` swallowed any
    // non-array response into an empty list with no error — the same
    // false-idle failure mode the plan exists to eliminate. Now that the
    // guard is removed, reading `.task_runs` off a response that isn't a
    // proper envelope (here: null, e.g. a bad `JSON.parse` result) throws
    // inside the `.then`, which the chained `.catch` turns into a visible
    // `taskRunsError` instead of a silently empty list.
    (api.runnerTaskRunsRunning as ReturnType<typeof vi.fn>).mockResolvedValue(
      null as unknown as { scope: string; task_runs: unknown[] },
    );

    renderPage();

    fireEvent.click(screen.getByRole('button', { name: /refresh/i }));

    await waitFor(() => {
      expect(screen.getByText('0 active')).toBeInTheDocument();
    });

    // The malformed response surfaced as a visible error, not a silent empty list.
    expect(
      screen.getByText(/Cannot read propert(y|ies) of null/),
    ).toBeInTheDocument();
    expect(screen.queryByText('task-1')).not.toBeInTheDocument();
  });
});
