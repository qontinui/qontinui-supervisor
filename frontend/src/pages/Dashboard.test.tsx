import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import Dashboard from './Dashboard';
import { api } from '../lib/api';

// Mock the api module
vi.mock('../lib/api', () => {
  const healthResponse = {
    status: 'healthy',
    runner: {
      running: true,
      pid: 1234,
      api_responding: true,
    },
    ports: {
      api_port: { port: 9876, in_use: true },
    },
    watchdog: {
      enabled: true,
      restart_attempts: 0,
      crash_count: 0,
      crash_restart_armed: true,
    },
    build: {
      in_progress: false,
      available_slots: 3,
      error_detected: false,
    },
    expo: {
      running: false,
      port: 8081,
      configured: true,
    },
    supervisor: {
      version: '0.1.0',
      project_dir: '/test',
    },
  };

  return {
    api: {
      health: vi.fn().mockResolvedValue(healthResponse),
      devStartStatus: vi.fn().mockResolvedValue({
        services: [
          { name: 'backend', port: 8000, available: true },
          { name: 'frontend', port: 3001, available: true },
        ],
      }),
      expoStatus: vi.fn().mockResolvedValue({ running: false, configured: true }),
      devStartAction: vi.fn().mockResolvedValue({
        status: 'ok',
        flag: 'backend',
        stdout: 'Started',
        stderr: '',
        exit_code: 0,
      }),
      logFile: vi.fn().mockResolvedValue({ content: '', lines: 0 }),
      expoStart: vi.fn().mockResolvedValue({}),
      expoStop: vi.fn().mockResolvedValue({}),
      listRunners: vi.fn().mockResolvedValue([]),
    },
    HealthResponse: {},
    DevStartResponse: {},
  };
});

// Mock EventSource since jsdom doesn't provide it
class MockEventSource {
  static instances: MockEventSource[] = [];
  url: string;
  listeners: Record<string, ((e: MessageEvent) => void)[]> = {};
  onerror: (() => void) | null = null;

  constructor(url: string) {
    this.url = url;
    MockEventSource.instances.push(this);
  }

  addEventListener(event: string, handler: (e: MessageEvent) => void) {
    if (!this.listeners[event]) this.listeners[event] = [];
    this.listeners[event].push(handler);
  }

  removeEventListener() {}
  close() {}
}

beforeEach(() => {
  MockEventSource.instances = [];
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  (globalThis as any).EventSource = MockEventSource;
});

afterEach(() => {
  vi.clearAllMocks();
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  delete (globalThis as any).EventSource;
});

function renderDashboard() {
  return render(
    <MemoryRouter>
      <Dashboard />
    </MemoryRouter>,
  );
}

describe('Dashboard', () => {
  it('renders the Runner Instances panel', async () => {
    renderDashboard();
    expect(await screen.findByText('Runner Instances')).toBeInTheDocument();
  });

  it('renders the Logs panel', async () => {
    renderDashboard();
    expect(await screen.findByText('Logs')).toBeInTheDocument();
  });

  it('shows health status from the health endpoint', async () => {
    renderDashboard();
    // The status bar should show "healthy"
    expect(await screen.findByText(/healthy/i)).toBeInTheDocument();
  });

  it('does NOT show the crash-restart disarmed badge when the primary is armed', async () => {
    // Default mock has watchdog.crash_restart_armed: true.
    renderDashboard();
    expect(await screen.findByText(/healthy/i)).toBeInTheDocument();
    expect(screen.queryByTestId('crash-restart-disarmed-badge')).not.toBeInTheDocument();
  });

  it('shows the crash-restart disarmed badge when the running primary is unarmed (#111)', async () => {
    // Override just this render: a supervisor-managed (running) primary whose
    // global crash-restart arm is OFF must surface the disarmed pill, even
    // though per-runner `enabled` reads true (the anti-lie the field fixes).
    vi.mocked(api.health).mockResolvedValueOnce({
      status: 'healthy',
      runner: { running: true, pid: 1234, api_responding: true },
      ports: { api_port: { port: 9876, in_use: true } },
      watchdog: {
        enabled: true,
        restart_attempts: 0,
        crash_count: 0,
        crash_restart_armed: false,
      },
      build: { in_progress: false, available_slots: 3, error_detected: false },
      expo: { running: false, port: 8081, configured: true },
      supervisor: { version: '0.1.0', project_dir: '/test' },
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
    } as any);
    renderDashboard();
    expect(await screen.findByTestId('crash-restart-disarmed-badge')).toBeInTheDocument();
  });

  it('does NOT show the crash-loop disarmed badge in the nominal (armed, no reason) state', async () => {
    // Default mock: armed + no disabled_reason → neither disarm badge fires.
    renderDashboard();
    expect(await screen.findByText(/healthy/i)).toBeInTheDocument();
    expect(screen.queryByTestId('crash-loop-disarmed-badge')).not.toBeInTheDocument();
  });

  it('shows the crash-loop disarmed badge when an armed primary self-disarmed after a crash loop (#111/#113)', async () => {
    // The watchdog WAS armed (crash_restart_armed: true) and auto-restarted the
    // primary, but it kept crashing — so the crash-loop guard disarmed itself
    // (disabled_reason set). The "never armed" pill must NOT fire (arm is on);
    // this distinct red badge must surface the operator-required state.
    vi.mocked(api.health).mockResolvedValueOnce({
      status: 'degraded',
      runner: { running: true, pid: 1234, api_responding: false },
      ports: { api_port: { port: 9876, in_use: true } },
      watchdog: {
        enabled: true,
        restart_attempts: 3,
        crash_count: 3,
        crash_restart_armed: true,
        disabled_reason: 'crash loop — operator required',
      },
      build: { in_progress: false, available_slots: 3, error_detected: false },
      expo: { running: false, port: 8081, configured: true },
      supervisor: { version: '0.1.0', project_dir: '/test' },
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
    } as any);
    renderDashboard();
    expect(await screen.findByTestId('crash-loop-disarmed-badge')).toBeInTheDocument();
    // The "never armed" pill is mutually exclusive here — arm is on.
    expect(screen.queryByTestId('crash-restart-disarmed-badge')).not.toBeInTheDocument();
  });

  it('does NOT show the crash-loop disarmed badge for the degraded "watchdog state unavailable" payload', async () => {
    // The unavailable() path sets disabled_reason AND crash_restart_armed:false.
    // That is covered by the "never armed" pill, not the crash-loop badge — the
    // `!== false` arm guard keeps the two from double-firing.
    vi.mocked(api.health).mockResolvedValueOnce({
      status: 'degraded',
      runner: { running: true, pid: 1234, api_responding: true },
      ports: { api_port: { port: 9876, in_use: true } },
      watchdog: {
        enabled: true,
        restart_attempts: 0,
        crash_count: 0,
        crash_restart_armed: false,
        disabled_reason: 'watchdog state unavailable',
      },
      build: { in_progress: false, available_slots: 3, error_detected: false },
      expo: { running: false, port: 8081, configured: true },
      supervisor: { version: '0.1.0', project_dir: '/test' },
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
    } as any);
    renderDashboard();
    // The "never armed" pill fires (arm=false); the crash-loop badge must not.
    expect(await screen.findByTestId('crash-restart-disarmed-badge')).toBeInTheDocument();
    expect(screen.queryByTestId('crash-loop-disarmed-badge')).not.toBeInTheDocument();
  });
});
