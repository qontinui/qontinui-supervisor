import { describe, it, expect } from 'vitest';
import { render, screen } from '@testing-library/react';
import { RunnerWatchdogBadge } from './Dashboard';
import type { WatchdogHealthWire } from '../lib/api';

const RED = 'runner-crash-loop-disarmed-badge';
const AMBER = 'runner-crash-restart-disarmed-badge';

function wd(overrides: Partial<WatchdogHealthWire> = {}): WatchdogHealthWire {
  return {
    enabled: false,
    restart_attempts: 0,
    crash_count: 0,
    crash_restart_armed: true,
    ...overrides,
  };
}

describe('RunnerWatchdogBadge', () => {
  it('renders nothing when watchdog is absent (older supervisor / no data)', () => {
    const { container } = render(
      <RunnerWatchdogBadge watchdog={undefined} running={true} isPrimary={false} />,
    );
    expect(container).toBeEmptyDOMElement();
  });

  it('renders nothing in the nominal armed, no-reason state', () => {
    render(
      <RunnerWatchdogBadge
        watchdog={wd({ enabled: true, crash_restart_armed: true })}
        running={true}
        isPrimary={false}
      />,
    );
    expect(screen.queryByTestId(RED)).not.toBeInTheDocument();
    expect(screen.queryByTestId(AMBER)).not.toBeInTheDocument();
  });

  it('shows the RED crash-loop disarmed badge when disabled_reason is set and the arm is on', () => {
    render(
      <RunnerWatchdogBadge
        watchdog={wd({
          enabled: true,
          crash_restart_armed: true,
          disabled_reason: 'crash loop — operator required',
          crash_count: 3,
          restart_attempts: 3,
        })}
        running={false}
        isPrimary={false}
      />,
    );
    const badge = screen.getByTestId(RED);
    expect(badge).toBeInTheDocument();
    // Persistent state: shown even though the runner is down (running=false).
    expect(badge.getAttribute('title')).toContain('3 crashes');
    expect(badge.getAttribute('title')).toContain('3 auto-restart attempts');
    // Mutually exclusive with the amber badge.
    expect(screen.queryByTestId(AMBER)).not.toBeInTheDocument();
  });

  it('pluralizes crash/attempt counts correctly for a single occurrence', () => {
    render(
      <RunnerWatchdogBadge
        watchdog={wd({
          enabled: true,
          crash_restart_armed: true,
          disabled_reason: 'crash loop — operator required',
          crash_count: 1,
          restart_attempts: 1,
        })}
        running={false}
        isPrimary={false}
      />,
    );
    const title = screen.getByTestId(RED).getAttribute('title') ?? '';
    expect(title).toContain('1 crash,');
    expect(title).toContain('1 auto-restart attempt)');
    expect(title).not.toContain('1 crashes');
    expect(title).not.toContain('1 auto-restart attempts');
  });

  it('shows the AMBER disarmed badge for a NON-primary armed runner while the global arm is off', () => {
    render(
      <RunnerWatchdogBadge
        watchdog={wd({ enabled: true, crash_restart_armed: false })}
        running={true}
        isPrimary={false}
      />,
    );
    expect(screen.getByTestId(AMBER)).toBeInTheDocument();
    expect(screen.queryByTestId(RED)).not.toBeInTheDocument();
  });

  it('does NOT show the amber badge on the PRIMARY row (the status bar already covers the global arm)', () => {
    render(
      <RunnerWatchdogBadge
        watchdog={wd({ enabled: true, crash_restart_armed: false })}
        running={true}
        isPrimary={true}
      />,
    );
    expect(screen.queryByTestId(AMBER)).not.toBeInTheDocument();
    expect(screen.queryByTestId(RED)).not.toBeInTheDocument();
  });

  it('does NOT show the amber badge for a runner that is not armed (enabled=false is the named/temp default)', () => {
    render(
      <RunnerWatchdogBadge
        watchdog={wd({ enabled: false, crash_restart_armed: false })}
        running={true}
        isPrimary={false}
      />,
    );
    expect(screen.queryByTestId(AMBER)).not.toBeInTheDocument();
    expect(screen.queryByTestId(RED)).not.toBeInTheDocument();
  });

  it('does NOT show the amber badge for a stopped (not running) armed runner', () => {
    render(
      <RunnerWatchdogBadge
        watchdog={wd({ enabled: true, crash_restart_armed: false })}
        running={false}
        isPrimary={false}
      />,
    );
    expect(screen.queryByTestId(AMBER)).not.toBeInTheDocument();
  });

  it('still shows the RED badge (not amber) even on the primary — crash-loop disarm needs surfacing everywhere', () => {
    render(
      <RunnerWatchdogBadge
        watchdog={wd({
          enabled: true,
          crash_restart_armed: true,
          disabled_reason: 'crash loop — operator required',
        })}
        running={true}
        isPrimary={true}
      />,
    );
    expect(screen.getByTestId(RED)).toBeInTheDocument();
  });

  it('treats an omitted crash_restart_armed as "on" for the red badge (older supervisor with a reason set)', () => {
    render(
      <RunnerWatchdogBadge
        watchdog={{
          enabled: true,
          restart_attempts: 2,
          crash_count: 2,
          disabled_reason: 'crash loop — operator required',
          // crash_restart_armed omitted
        }}
        running={true}
        isPrimary={false}
      />,
    );
    expect(screen.getByTestId(RED)).toBeInTheDocument();
  });
});
