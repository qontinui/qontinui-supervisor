import { describe, it, expect } from 'vitest';
import { render, screen } from '@testing-library/react';
import { PrebindHangBadge } from './Dashboard';
import type { WatchdogHealthWire } from '../lib/api';

const ID = 'runner-prebind-hang-badge';
const SPAWN = '2026-11-04T01:00:00+00:00';
const SENT = '2026-11-04T01:01:35+00:00';

function wd(overrides: Partial<WatchdogHealthWire> = {}): WatchdogHealthWire {
  return {
    enabled: true,
    restart_attempts: 0,
    crash_count: 0,
    last_prebind_alert_at: SENT,
    last_prebind_alert_spawn_started_at: SPAWN,
    ...overrides,
  };
}

describe('PrebindHangBadge', () => {
  it('renders when the alert names the current spawn and it never answered', () => {
    render(<PrebindHangBadge watchdog={wd()} startedAt={SPAWN} lastSeenRespondingAt={null} />);
    expect(screen.getByTestId(ID).getAttribute('title')).toContain(SENT);
  });

  it('compares instants, not strings (Z vs +00:00)', () => {
    render(
      <PrebindHangBadge
        watchdog={wd()}
        startedAt="2026-11-04T01:00:00Z"
        lastSeenRespondingAt={null}
      />,
    );
    expect(screen.getByTestId(ID)).toBeInTheDocument();
  });

  it('stays hidden for an alert about an earlier spawn', () => {
    render(
      <PrebindHangBadge
        watchdog={wd()}
        startedAt="2026-11-04T02:00:00Z"
        lastSeenRespondingAt={null}
      />,
    );
    expect(screen.queryByTestId(ID)).not.toBeInTheDocument();
  });

  it('stays hidden once the runner answered during this spawn', () => {
    render(
      <PrebindHangBadge
        watchdog={wd()}
        startedAt={SPAWN}
        lastSeenRespondingAt="2026-11-04T01:03:00Z"
      />,
    );
    expect(screen.queryByTestId(ID)).not.toBeInTheDocument();
  });

  it('still renders when the last answer predates this spawn', () => {
    render(
      <PrebindHangBadge
        watchdog={wd()}
        startedAt={SPAWN}
        lastSeenRespondingAt="2026-11-03T20:00:00Z"
      />,
    );
    expect(screen.getByTestId(ID)).toBeInTheDocument();
  });

  it('renders nothing with no alert, no watchdog, or an older supervisor', () => {
    const { container, rerender } = render(
      <PrebindHangBadge
        watchdog={wd({ last_prebind_alert_at: null, last_prebind_alert_spawn_started_at: null })}
        startedAt={SPAWN}
      />,
    );
    expect(container).toBeEmptyDOMElement();
    rerender(<PrebindHangBadge watchdog={undefined} startedAt={SPAWN} />);
    expect(container).toBeEmptyDOMElement();
    rerender(
      <PrebindHangBadge
        watchdog={wd({
          last_prebind_alert_at: undefined,
          last_prebind_alert_spawn_started_at: undefined,
        })}
        startedAt={SPAWN}
      />,
    );
    expect(container).toBeEmptyDOMElement();
  });
});
