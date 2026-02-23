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
    json: () => Promise.resolve(data),
  });
}

function mockFetchError(status: number, statusText: string) {
  (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValue({
    ok: false,
    status,
    statusText,
    json: () => Promise.resolve({}),
  });
}

describe('api.health()', () => {
  it('calls GET /health and returns parsed JSON', async () => {
    const healthData = {
      status: 'ok',
      runner: { running: true, api_responding: true, mode: 'dev' },
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
      supervisor: { version: '0.1.0', dev_mode: true, project_dir: '/test' },
    };

    mockFetchOk(healthData);

    const result = await api.health();

    expect(globalThis.fetch).toHaveBeenCalledWith('/health', undefined);
    expect(result).toEqual(healthData);
    expect(result.status).toBe('ok');
    expect(result.runner.running).toBe(true);
  });
});

describe('api.devStartAction()', () => {
  it('sends POST to /dev-start/{action}', async () => {
    const responseData = {
      status: 'ok',
      flag: 'backend',
      stdout: 'Started',
      stderr: '',
      exit_code: 0,
    };

    mockFetchOk(responseData);

    const result = await api.devStartAction('backend');

    expect(globalThis.fetch).toHaveBeenCalledWith('/dev-start/backend', {
      method: 'POST',
    });
    expect(result.status).toBe('ok');
    expect(result.flag).toBe('backend');
  });

  it('sends POST to /dev-start/frontend/stop for stop actions', async () => {
    mockFetchOk({
      status: 'ok',
      flag: 'frontend/stop',
      stdout: '',
      stderr: '',
      exit_code: 0,
    });

    await api.devStartAction('frontend/stop');

    expect(globalThis.fetch).toHaveBeenCalledWith('/dev-start/frontend/stop', {
      method: 'POST',
    });
  });
});

describe('api error handling', () => {
  it('throws when response is not ok', async () => {
    mockFetchError(500, 'Internal Server Error');

    await expect(api.health()).rejects.toThrow('500 Internal Server Error');
  });

  it('throws on 404 responses', async () => {
    mockFetchError(404, 'Not Found');

    await expect(api.devStartStatus()).rejects.toThrow('404 Not Found');
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

describe('api.devStartStatus()', () => {
  it('calls GET /dev-start/status and returns services', async () => {
    const statusData = {
      services: [
        { name: 'backend', port: 8000, available: true },
        { name: 'frontend', port: 3001, available: false },
      ],
    };

    mockFetchOk(statusData);

    const result = await api.devStartStatus();

    expect(globalThis.fetch).toHaveBeenCalledWith('/dev-start/status', undefined);
    expect(result.services).toHaveLength(2);
    expect(result.services[0].name).toBe('backend');
    expect(result.services[0].available).toBe(true);
  });
});
