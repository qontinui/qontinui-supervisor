import { describe, it, expect } from 'vitest';
import { render, screen } from '@testing-library/react';
import { RunnerStatusBadge } from './RunnerStatusBadge';

/// The wedge — "the process is ALIVE and holding its port, and its API answers
/// nothing" — is the state this badge was blind to. `derived_status` cannot
/// express it: the supervisor's `derive_runner_status` maps "believed up, API
/// silent" to `starting`, so a runner wedged for 14 hours rendered a blue
/// "Starting" pill; and for a primary (whose `running` is synced down to the
/// probe) the same wedge rendered a grey "Offline" pill for a live process.
describe('RunnerStatusBadge — liveness', () => {
  it('renders Wedged, overriding a derived_status of starting', () => {
    render(
      <RunnerStatusBadge
        derivedStatus={{ kind: 'starting' }}
        liveness={{ state: 'wedged', unresponsive_since: '2026-08-30T04:11:07.918+00:00' }}
      />,
    );

    const badge = screen.getByText('Wedged');
    expect(badge).toBeInTheDocument();
    expect(badge.className).toContain('badge-danger');
    expect(screen.queryByText('Starting')).not.toBeInTheDocument();
  });

  it('renders Wedged, overriding a derived_status of offline', () => {
    render(
      <RunnerStatusBadge
        derivedStatus={{ kind: 'offline' }}
        liveness={{ state: 'wedged', unresponsive_since: '2026-08-30T04:11:07.918+00:00' }}
      />,
    );

    expect(screen.getByText('Wedged')).toBeInTheDocument();
    expect(screen.queryByText('Offline')).not.toBeInTheDocument();
  });

  it('says the process is alive and warns against restarting, in the tooltip', () => {
    render(
      <RunnerStatusBadge
        derivedStatus={{ kind: 'starting' }}
        liveness={{ state: 'wedged', unresponsive_since: '2026-08-30T04:11:07.918+00:00' }}
      />,
    );

    const title = screen.getByText('Wedged').getAttribute('title') ?? '';
    expect(title).toContain('ALIVE');
    expect(title).toContain('not a stopped runner');
    expect(title).toContain('thread dump');
  });

  it('leaves every non-wedged verdict to derived_status', () => {
    for (const state of ['responding', 'stopped', 'unknown'] as const) {
      const { unmount } = render(
        <RunnerStatusBadge
          derivedStatus={{ kind: 'healthy' }}
          liveness={{ state, unresponsive_since: null }}
        />,
      );
      expect(screen.getByText('Healthy')).toBeInTheDocument();
      unmount();
    }
  });

  it('is unchanged when the supervisor omits liveness (older build)', () => {
    render(<RunnerStatusBadge derivedStatus={{ kind: 'degraded', reason: 'runner reported derived_status=degraded' }} />);
    expect(screen.getByText('Degraded')).toBeInTheDocument();
  });
});
