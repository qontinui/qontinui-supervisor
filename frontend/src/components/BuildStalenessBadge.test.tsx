import { describe, it, expect } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/react';
import {
  BuildStalenessBadge,
  deriveBuildStaleness,
  DRIFT_PROBE_MAX_AGE_SECS,
  type BuildsReading,
} from './BuildStalenessBadge';
import type { BuildsResponse, OriginMainDriftProbe } from '../lib/api';

/// Plan 2026-09-05-supervisor-knows-the-runner-is-stale-and-never-says-so,
/// Phase 4. The supervisor serves three proofs of a stale pool on
/// `GET /builds`; these tests pin that the badge renders them, and — the
/// honesty half — that an unanswerable drift probe reads UNKNOWN, never clean.

const FRESH_PROBE: OriginMainDriftProbe = {
  state: 'fresh',
  computed_at: '2026-09-04T11:40:10+00:00',
  age_secs: 62,
  computed_for_sha: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
};

const POOL_BEHIND_MESSAGE =
  'pool_behind_local_build: local build D:/qontinui-root/qontinui-runner/src-tauri/target/debug/qontinui-runner.exe (mtime 2026-09-04T11:40:10Z) is NEWER than picked slot 0 (mtime 2026-09-03T04:44:01Z); resolution runs the SLOT exe, not the local build — no provenance sidecar beside the local build. Rebuild through the supervisor to promote a binary.';

function builds(overrides: Partial<BuildsResponse> = {}): BuildsResponse {
  return {
    pool_size: 3,
    available_permits: 3,
    lkg: {
      built_at: '2026-09-03T04:56:00+00:00',
      source_slot: 0,
      exe_size: 1,
      sha: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
      source: 'origin_main',
    },
    origin_main_drift: null,
    origin_main_drift_probe: FRESH_PROBE,
    pool_behind_local_build: null,
    ...overrides,
  };
}

const ok = (b: BuildsResponse): BuildsReading => ({ kind: 'ok', builds: b });

describe('deriveBuildStaleness', () => {
  it('1. behind_count > 0 ⇒ warn', () => {
    const v = deriveBuildStaleness(
      ok(
        builds({
          origin_main_drift: {
            built_sha: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
            origin_main_sha: 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
            behind_count: 36,
            is_ancestor: true,
            diverged: false,
            fetched: true,
          },
        }),
      ),
    );
    expect(v.verdict).toBe('warn');
    expect(v.verdict === 'warn' && v.reasons.join(' ')).toContain('36 commits behind origin/main');
  });

  it('2. pool_behind_local_build non-null ⇒ warn, even when the drift probe is pending', () => {
    const v = deriveBuildStaleness(
      ok(
        builds({
          origin_main_drift_probe: {
            state: 'pending',
            computed_at: null,
            age_secs: null,
            computed_for_sha: null,
          },
          pool_behind_local_build: {
            legacy_path: 'D:/x/qontinui-runner.exe',
            legacy_mtime: '2026-09-04T11:40:10Z',
            picked_slot_id: 0,
            picked_slot_mtime: '2026-09-03T04:44:01Z',
            target_dir_source: 'legacy',
            adopted: false,
            local_build_sha: null,
            local_build_source: null,
            message: POOL_BEHIND_MESSAGE,
          },
        }),
      ),
    );
    expect(v.verdict).toBe('warn');
    expect(v.verdict === 'warn' && v.reasons.join(' ')).toContain('NOT what runs');
  });

  it('3. both proofs null with a fresh, in-age probe ⇒ clean', () => {
    expect(deriveBuildStaleness(ok(builds())).verdict).toBe('clean');
  });

  it('4. an unanswerable drift probe ⇒ unknown, never clean (pins §4)', () => {
    const cases: Array<[string, OriginMainDriftProbe | undefined]> = [
      ['pending', { state: 'pending', computed_at: null, age_secs: null, computed_for_sha: null }],
      ['superseded_lkg_moved', { ...FRESH_PROBE, state: 'superseded_lkg_moved' }],
      ['not_computable', { ...FRESH_PROBE, state: 'not_computable' }],
      ['fresh but too old', { ...FRESH_PROBE, age_secs: DRIFT_PROBE_MAX_AGE_SECS + 1 }],
      ['absent (older supervisor)', undefined],
    ];
    for (const [name, probe] of cases) {
      const b = builds();
      if (probe === undefined) {
        delete (b as Partial<BuildsResponse>).origin_main_drift_probe;
      } else {
        b.origin_main_drift_probe = probe;
      }
      const v = deriveBuildStaleness(ok(b));
      expect(v.verdict, name).toBe('unknown');
    }
    expect(deriveBuildStaleness({ kind: 'pending' }).verdict).toBe('unknown');
    expect(deriveBuildStaleness({ kind: 'error', message: 'boom' })).toEqual({
      verdict: 'unknown',
      reason: 'GET /builds failed: boom',
    });
  });

  it('a fresh probe exactly at the age limit is still fresh', () => {
    const b = builds({
      origin_main_drift_probe: { ...FRESH_PROBE, age_secs: DRIFT_PROBE_MAX_AGE_SECS },
    });
    expect(deriveBuildStaleness(ok(b)).verdict).toBe('clean');
  });
});

describe('BuildStalenessBadge', () => {
  it('renders nothing when clean', () => {
    render(<BuildStalenessBadge reading={ok(builds())} runnerName="primary" />);
    expect(screen.queryByTestId('build-staleness-badge-primary')).not.toBeInTheDocument();
  });

  it('renders an amber "build behind" pill on drift, and the panel shows the numbers', () => {
    render(
      <BuildStalenessBadge
        reading={ok(
          builds({
            origin_main_drift: {
              built_sha: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
              origin_main_sha: 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
              behind_count: 36,
              is_ancestor: true,
              diverged: false,
              fetched: true,
            },
          }),
        )}
        runnerName="primary"
      />,
    );
    const badge = screen.getByTestId('build-staleness-badge-primary');
    expect(badge).toHaveTextContent('build behind');
    expect(badge.className).toContain('badge-warning');
    expect(badge.getAttribute('title')).toContain('origin/main (not the working tree)');
    fireEvent.click(badge);
    const panel = screen.getByTestId('build-staleness-panel');
    expect(panel).toHaveTextContent('36 behind');
    expect(screen.getByTestId('drift-probe-line')).toHaveTextContent('fresh, 1m old');
    expect(panel).toHaveTextContent('LKG built');
  });

  it('shows pool_behind_local_build.message VERBATIM in the panel', () => {
    render(
      <BuildStalenessBadge
        reading={ok(
          builds({
            pool_behind_local_build: {
              legacy_path: 'D:/x/qontinui-runner.exe',
              legacy_mtime: '2026-09-04T11:40:10Z',
              picked_slot_id: 0,
              picked_slot_mtime: '2026-09-03T04:44:01Z',
              target_dir_source: 'legacy',
              adopted: false,
              local_build_sha: null,
              local_build_source: null,
              message: POOL_BEHIND_MESSAGE,
            },
          }),
        )}
        runnerName="primary"
      />,
    );
    fireEvent.click(screen.getByTestId('build-staleness-badge-primary'));
    expect(screen.getByTestId('pool-behind-local-build-message').textContent).toBe(
      POOL_BEHIND_MESSAGE,
    );
  });

  it('reads UNKNOWN (muted pill, reason in the panel) when the probe is stale', () => {
    render(
      <BuildStalenessBadge
        reading={ok(builds({ origin_main_drift_probe: { ...FRESH_PROBE, age_secs: 3600 } }))}
        runnerName="primary"
      />,
    );
    const badge = screen.getByTestId('build-staleness-badge-primary');
    expect(badge).toHaveTextContent('build: unknown');
    expect(badge.className).toContain('badge-secondary');
    expect(badge.getAttribute('data-verdict')).toBe('unknown');
    fireEvent.click(badge);
    expect(screen.getByTestId('build-staleness-panel')).toHaveTextContent('UNKNOWN');
    expect(screen.getByTestId('build-staleness-panel')).toHaveTextContent('1h old');
  });

  it('reads UNKNOWN before /builds has answered and when it failed', () => {
    const { unmount } = render(
      <BuildStalenessBadge reading={{ kind: 'pending' }} runnerName="primary" />,
    );
    expect(screen.getByTestId('build-staleness-badge-primary')).toHaveTextContent('build: unknown');
    unmount();
    render(
      <BuildStalenessBadge reading={{ kind: 'error', message: 'HTTP 503' }} runnerName="primary" />,
    );
    const badge = screen.getByTestId('build-staleness-badge-primary');
    expect(badge.getAttribute('title')).toContain('GET /builds failed: HTTP 503');
  });
});
