import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import Dashboard from './Dashboard';

// Mock the api module
vi.mock('../lib/api', () => {
  const healthResponse = {
    status: 'healthy',
    runner: {
      running: true,
      pid: 1234,
      api_responding: true,
      mode: 'dev',
    },
    ports: {
      api_port: { port: 9876, in_use: true },
      vite_port: { port: 1420, in_use: true },
    },
    watchdog: {
      enabled: true,
      restart_attempts: 0,
      crash_count: 0,
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
      dev_mode: true,
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

  it('shows runner mode in the status bar', async () => {
    renderDashboard();
    expect(await screen.findByText(/Mode: dev/)).toBeInTheDocument();
  });
});
