import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter } from 'react-router-dom';
import Dashboard from './Dashboard';

// Mock the api module
vi.mock('../lib/api', () => {
  const healthResponse = {
    status: 'ok',
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
      error_detected: false,
    },
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
          { name: 'postgresql', port: 5432, available: true },
          { name: 'redis', port: 6379, available: true },
          { name: 'minio', port: 9000, available: false },
          { name: 'vite', port: 1420, available: true },
        ],
      }),
      expoStatus: vi.fn().mockResolvedValue({ running: false }),
      devStartAction: vi.fn().mockResolvedValue({
        status: 'ok',
        flag: 'backend',
        stdout: 'Started',
        stderr: '',
        exit_code: 0,
      }),
      runnerRestart: vi.fn().mockResolvedValue({ status: 'ok' }),
      runnerStop: vi.fn().mockResolvedValue({ status: 'ok' }),
      aiModels: vi.fn().mockResolvedValue({ models: [] }),
      aiStop: vi.fn().mockResolvedValue({ status: 'ok' }),
      aiDebug: vi.fn().mockResolvedValue({ status: 'ok', message: 'started' }),
      logFile: vi.fn().mockResolvedValue({ content: '', lines: 0 }),
      wlStatus: vi.fn().mockResolvedValue({ running: false, phase: 'idle', current_iteration: 0 }),
      wlStop: vi.fn().mockResolvedValue({}),
      expoStart: vi.fn().mockResolvedValue({}),
      expoStop: vi.fn().mockResolvedValue({}),
    },
    HealthResponse: {},
    DevStartResponse: {},
    AiModelInfo: {},
    WorkflowLoopStatus: {},
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
  it("renders the page header with 'Dashboard'", async () => {
    renderDashboard();

    expect(await screen.findByRole('heading', { name: 'Dashboard' })).toBeInTheDocument();
  });

  it('renders the service table with expected service rows', async () => {
    renderDashboard();

    // Wait for data to load
    const table = await screen.findByRole('table');
    expect(table).toBeInTheDocument();

    // Check that all expected service names appear in the table
    const expectedServices = [
      'Runner',
      'Backend',
      'Frontend',
      'PostgreSQL',
      'Redis',
      'MinIO',
      'Vite',
      'Expo',
      'Watchdog',
    ];

    for (const service of expectedServices) {
      expect(within(table).getByText(service)).toBeInTheDocument();
    }
  });

  it('renders table column headers', async () => {
    renderDashboard();

    const table = await screen.findByRole('table');
    expect(within(table).getByText('Service')).toBeInTheDocument();
    expect(within(table).getByText('Port')).toBeInTheDocument();
    expect(within(table).getByText('Status')).toBeInTheDocument();
    expect(within(table).getByText('Actions')).toBeInTheDocument();
  });

  it('shows Start/Restart button for Backend that calls the api', async () => {
    const { api } = await import('../lib/api');
    const user = userEvent.setup();
    renderDashboard();

    // Wait for the table to render, then find the Backend row
    const table = await screen.findByRole('table');
    const backendRow = within(table).getByText('Backend').closest('tr')!;
    const backendRestartBtn = within(backendRow).getByRole('button', {
      name: /Restart/,
    });

    await user.click(backendRestartBtn);

    expect(api.devStartAction).toHaveBeenCalledWith('backend');
  });

  it('shows Rebuild button for Runner that calls runnerRestart with true', async () => {
    const { api } = await import('../lib/api');
    const user = userEvent.setup();
    renderDashboard();

    const table = await screen.findByRole('table');
    const runnerRow = within(table).getByText('Runner').closest('tr')!;
    const rebuildBtn = within(runnerRow).getByRole('button', {
      name: 'Rebuild',
    });

    await user.click(rebuildBtn);

    expect(api.runnerRestart).toHaveBeenCalledWith(true);
  });

  it('renders the Bulk Actions section', async () => {
    renderDashboard();

    expect(await screen.findByText('Bulk Actions')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Docker' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Start All' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Clean' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Migrate' })).toBeInTheDocument();
  });
});
