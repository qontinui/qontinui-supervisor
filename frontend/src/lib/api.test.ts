import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { api } from './api';

// Save original fetch
const originalFetch = globalThis.fetch;

beforeEach(() => {
  globalThis.fetch = vi.fn();
});

afterEach(() => {
  globalThis.fetch = originalFetch;
});

function mockFetchOk(data: unknown) {
  (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValue({
    ok: true,
    status: 200,
    text: () => Promise.resolve(JSON.stringify(data)),
    json: () => Promise.resolve(data),
  });
}

function mockFetchError(status: number, statusText: string) {
  (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValue({
    ok: false,
    status,
    statusText,
    text: () => Promise.resolve(''),
    json: () => Promise.resolve({}),
  });
}

describe('api.health()', () => {
  it('calls GET /health and returns parsed JSON', async () => {
    const healthData = {
      status: 'ok',
      runner: { running: true, api_responding: true },
      watchdog: { enabled: true, restart_attempts: 0, crash_count: 0 },
      build: { in_progress: false, error_detected: false },
      ai: {
        ai_running: false,
        ai_provider: 'claude',
        ai_model: 'opus',
        auto_debug_enabled: false,
      },
      code_activity: {
        code_being_edited: false,
        external_claude_session: false,
        pending_debug: false,
      },
      expo: { running: false, port: 8081, configured: false },
      ports: { api_port: { port: 9876, in_use: true } },
      supervisor: { version: '0.1.0', project_dir: '/test' },
    };

    mockFetchOk(healthData);

    const result = await api.health();

    expect(globalThis.fetch).toHaveBeenCalledWith('/health', undefined);
    expect(result).toEqual(healthData);
    expect(result.status).toBe('ok');
    expect(result.runner.running).toBe(true);
  });
});

describe('api error handling', () => {
  it('throws when response is not ok', async () => {
    mockFetchError(500, 'Internal Server Error');

    await expect(api.health()).rejects.toThrow('500 Internal Server Error');
  });

  it('throws on 404 responses', async () => {
    mockFetchError(404, 'Not Found');

    await expect(api.builds()).rejects.toThrow('404 Not Found');
  });
});

describe('api.runnerRestart()', () => {
  it('sends POST with rebuild flag', async () => {
    mockFetchOk({ status: 'ok' });

    await api.runnerRestart(true);

    expect(globalThis.fetch).toHaveBeenCalledWith('/runner/restart', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ rebuild: true }),
    });
  });

  it('sends POST with rebuild=false', async () => {
    mockFetchOk({ status: 'ok' });

    await api.runnerRestart(false);

    expect(globalThis.fetch).toHaveBeenCalledWith('/runner/restart', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ rebuild: false }),
    });
  });
});

describe('api.runnerStop()', () => {
  it('sends POST to /runner/stop', async () => {
    mockFetchOk({ status: 'ok' });

    await api.runnerStop();

    expect(globalThis.fetch).toHaveBeenCalledWith('/runner/stop', {
      method: 'POST',
    });
  });
});

describe('api.builds()', () => {
  it('calls GET /builds with no query string — a request path must never trigger git fetch', async () => {
    const data = {
      pool_size: 3,
      available_permits: 3,
      lkg: null,
      origin_main_drift: null,
      origin_main_drift_probe: {
        state: 'pending',
        computed_at: null,
        age_secs: null,
        computed_for_sha: null,
      },
      pool_behind_local_build: null,
    };
    mockFetchOk(data);

    const result = await api.builds();

    expect(globalThis.fetch).toHaveBeenCalledWith('/builds', undefined);
    expect(result).toEqual(data);
  });
});
