import React, { useState } from 'react';
import { useUIElement } from '@qontinui/ui-bridge/react';
import type { BuildsResponse, OriginMainDriftProbe } from '../lib/api';

// ─── Build-staleness badge (plan 2026-09-05-supervisor-knows-the-runner-is-stale) ──
//
// The supervisor serves three proofs on `GET /builds` that the pool's binary is
// stale — `origin_main_drift` (the LKG sha is behind `origin/main`),
// `pool_behind_local_build` (a local `target/debug` build is newer than the
// picked slot and may not be what runs), and `lkg.built_at` — and until this
// badge the dashboard rendered none of them. It is the THIRD staleness surface
// on the primary row, and the subjects differ:
//
//   BuildRefreshBanner  — the dashboard's own web bundle in this tab (reload)
//   StaleBinaryBadge    — the running process vs the newest slot exe (Restart)
//   BuildStalenessBadge — the pool itself vs origin/main / a local build (Rebuild)
//
// Honesty rule: the drift reading is a timer-refreshed cache (the supervisor's
// 120 s `origin_drift` ticker — computing it runs `git fetch`, which a request
// path must never do). When that cache is pending, superseded, not computable,
// or too old, the badge reads UNKNOWN — never "clean". Absence of a drift
// reading is not absence of drift.

/// A drift probe older than this is treated as UNKNOWN: the supervisor's
/// default refresh cadence is 120 s (`QONTINUI_SUPERVISOR_ORIGIN_DRIFT_REFRESH_SECS`),
/// so a reading five times older than that means the ticker is dead or wedged,
/// and a stale reading presented as current is the exact defect this badge
/// exists to close.
export const DRIFT_PROBE_MAX_AGE_SECS = 600;

/// Format a seconds delta as a short relative-time string ("42s", "5m", "2h",
/// "3d"). Intentionally coarse — the badges are hints, not log lines. Shared
/// with `StaleBinaryBadge` in `Dashboard.tsx`.
export function formatRelativeAgeSecs(secs: number): string {
  const abs = Math.abs(Math.floor(secs));
  if (abs < 60) return `${abs}s`;
  if (abs < 3600) return `${Math.floor(abs / 60)}m`;
  if (abs < 86400) return `${Math.floor(abs / 3600)}h`;
  return `${Math.floor(abs / 86400)}d`;
}

/// What the dashboard currently knows about `GET /builds`. `pending` is the
/// state before the first response; `error` is a failed poll (carrying the
/// message so the panel can say why); `ok` is a parsed payload.
export type BuildsReading =
  | { kind: 'pending' }
  | { kind: 'error'; message: string }
  | { kind: 'ok'; builds: BuildsResponse };

/// The badge's verdict. `warn` carries one human-readable reason per proof
/// that fired; `unknown` carries the single reason the question could not be
/// answered; `clean` renders nothing.
export type BuildStalenessVerdict =
  | { verdict: 'clean' }
  | { verdict: 'warn'; reasons: string[] }
  | { verdict: 'unknown'; reason: string };

function shortSha(sha: string | null | undefined): string {
  return sha ? sha.slice(0, 9) : '?';
}

/// Why a probe is NOT a fresh answer, or `null` when it is one. The states are
/// the supervisor's own vocabulary (`routes/runners.rs` `list_builds`):
/// `fresh` | `not_computable` | `superseded_lkg_moved` | `pending`.
function probeUnknownReason(probe: OriginMainDriftProbe | undefined | null): string | null {
  if (!probe) {
    return 'this supervisor does not report an origin/main drift probe';
  }
  switch (probe.state) {
    case 'fresh':
      break;
    case 'pending':
      return 'origin/main drift has not been computed yet (probe pending)';
    case 'superseded_lkg_moved':
      return `the drift reading was computed for ${shortSha(probe.computed_for_sha)}, but the LKG has moved since (superseded)`;
    case 'not_computable':
      return 'origin/main drift is not computable here (no remote, or not a git repo)';
    default:
      return `drift probe in unrecognised state "${String(probe.state)}"`;
  }
  if (probe.age_secs != null && probe.age_secs > DRIFT_PROBE_MAX_AGE_SECS) {
    return `the drift reading is ${formatRelativeAgeSecs(probe.age_secs)} old (limit ${formatRelativeAgeSecs(DRIFT_PROBE_MAX_AGE_SECS)}) — the supervisor's drift ticker may be dead`;
  }
  return null;
}

/// Pure verdict function, exported so the rules are unit-testable without
/// rendering. Precedence: a positive proof of staleness wins over an
/// unanswerable probe (a local build being ignored is a fact regardless of
/// whether the git-side reading is current); an unanswerable probe wins over
/// "clean"; only a fresh, in-age probe with both proofs null is clean.
export function deriveBuildStaleness(reading: BuildsReading): BuildStalenessVerdict {
  if (reading.kind === 'pending') {
    return { verdict: 'unknown', reason: 'GET /builds has not answered yet' };
  }
  if (reading.kind === 'error') {
    return { verdict: 'unknown', reason: `GET /builds failed: ${reading.message}` };
  }
  const b = reading.builds;
  const reasons: string[] = [];
  const pool = b.pool_behind_local_build;
  if (pool) {
    reasons.push(
      pool.adopted
        ? `a local build newer than slot ${pool.picked_slot_id} exists and will be adopted on the next start`
        : `a local build newer than slot ${pool.picked_slot_id} exists and is NOT what runs`,
    );
  }
  const drift = b.origin_main_drift;
  if (drift && (drift.behind_count > 0 || drift.diverged)) {
    const n = drift.behind_count;
    reasons.push(
      drift.diverged
        ? `the LKG (${shortSha(drift.built_sha)}) has diverged from origin/main (${shortSha(drift.origin_main_sha)})`
        : `the LKG (${shortSha(drift.built_sha)}) is ${n} commit${n === 1 ? '' : 's'} behind origin/main (${shortSha(drift.origin_main_sha)})`,
    );
  }
  if (reasons.length > 0) return { verdict: 'warn', reasons };
  const unknown = probeUnknownReason(b.origin_main_drift_probe);
  if (unknown) return { verdict: 'unknown', reason: unknown };
  return { verdict: 'clean' };
}

interface BuildStalenessBadgeProps {
  reading: BuildsReading;
  /// Runner name, for test ids and accessible labels.
  runnerName: string;
  /// Optional UI Bridge element id; when set the badge is registered with a
  /// `toggle` custom action that flips the panel (same shape as
  /// `RunnerStatusBadge`).
  elementId?: string;
}

/// Amber "build behind" pill (or a muted "build: unknown" one) beside the
/// primary runner's status badge. Hidden when clean. Clicking toggles an
/// inline panel that shows WHY — the verbatim `pool_behind_local_build.message`
/// when present, the drift numbers, the probe's state and age, and the LKG's
/// age — because the operator's next action differs completely between "36
/// commits behind main" and "your local build is being ignored".
export function BuildStalenessBadge({ reading, runnerName, elementId }: BuildStalenessBadgeProps) {
  const [expanded, setExpanded] = useState(false);
  const verdict = deriveBuildStaleness(reading);

  const { ref: badgeRef } = useUIElement({
    id: elementId ?? '__build-staleness-badge-unregistered__',
    type: 'button',
    label: `Build staleness badge for ${runnerName}`,
    actions: ['click'],
    customActions: {
      toggle: {
        id: 'toggle',
        description: 'Toggle the build staleness detail panel',
        handler: () => setExpanded((v) => !v),
      },
    },
    autoRegister: elementId !== undefined,
  });

  if (verdict.verdict === 'clean') return null;

  const isWarn = verdict.verdict === 'warn';
  const label = isWarn ? 'build behind' : 'build: unknown';
  const tooltip = isWarn
    ? `${verdict.reasons.join('; ')}. Rebuild builds origin/main (not the working tree) and restarts.`
    : `Cannot tell whether the runner build is current: ${verdict.reason}.`;
  const toggle = () => setExpanded((v) => !v);

  return (
    <>
      <span
        ref={elementId !== undefined ? (badgeRef as React.RefCallback<HTMLSpanElement>) : undefined}
        data-ui-bridge-value={String(expanded)}
        data-testid={`build-staleness-badge-${runnerName}`}
        data-verdict={verdict.verdict}
        className={`badge ${isWarn ? 'badge-warning' : 'badge-secondary'} badge-clickable`}
        style={{ fontSize: '0.7rem' }}
        title={tooltip}
        role="button"
        tabIndex={0}
        aria-label={`Build staleness for ${runnerName}: ${label}`}
        onClick={toggle}
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            toggle();
          }
        }}
      >
        {label}
        <span style={{ marginLeft: '0.3rem', opacity: 0.7, fontSize: '0.65rem' }}>
          {expanded ? '▾' : '▸'}
        </span>
      </span>
      {expanded && <BuildStalenessPanel reading={reading} verdict={verdict} />}
    </>
  );
}

function BuildStalenessPanel({
  reading,
  verdict,
}: {
  reading: BuildsReading;
  verdict: BuildStalenessVerdict;
}) {
  const builds = reading.kind === 'ok' ? reading.builds : null;
  const probe = builds?.origin_main_drift_probe ?? null;
  const drift = builds?.origin_main_drift ?? null;
  const pool = builds?.pool_behind_local_build ?? null;
  const lkg = builds?.lkg ?? null;
  const now = Date.now();
  const lkgAgeSecs = lkg ? Math.max(0, Math.floor((now - Date.parse(lkg.built_at)) / 1000)) : null;
  const isWarn = verdict.verdict === 'warn';

  return (
    <div
      data-testid="build-staleness-panel"
      style={{
        flexBasis: '100%',
        marginTop: '0.5rem',
        padding: '0.6rem 0.75rem',
        background: isWarn ? 'rgba(245,158,11,0.08)' : 'rgba(148,163,184,0.08)',
        border: `1px solid ${isWarn ? 'rgba(245,158,11,0.35)' : 'rgba(148,163,184,0.35)'}`,
        borderRadius: 4,
        fontSize: '0.75rem',
      }}
    >
      {verdict.verdict === 'warn' && (
        <div style={{ marginBottom: '0.4rem' }}>
          <strong className="text-warning">Runner build is behind:</strong>
          <ul style={{ margin: '0.25rem 0 0 1rem', padding: 0 }}>
            {verdict.reasons.map((r) => (
              <li key={r}>{r}</li>
            ))}
          </ul>
        </div>
      )}
      {verdict.verdict === 'unknown' && (
        <div style={{ marginBottom: '0.4rem' }}>
          <strong className="text-muted">Build staleness UNKNOWN:</strong> {verdict.reason}
        </div>
      )}
      {pool && (
        <details open style={{ marginBottom: '0.4rem' }}>
          <summary className="text-muted" style={{ fontSize: '0.7rem', cursor: 'pointer' }}>
            Local build vs picked slot {pool.picked_slot_id} (adopted: {String(pool.adopted)})
          </summary>
          <pre
            data-testid="pool-behind-local-build-message"
            style={{
              margin: '0.25rem 0 0',
              padding: '0.4rem',
              background: 'var(--bg-tertiary, #1a1a2e)',
              borderRadius: 3,
              fontSize: '0.7rem',
              whiteSpace: 'pre-wrap',
              wordBreak: 'break-word',
              maxHeight: '200px',
              overflow: 'auto',
            }}
          >
            {pool.message}
          </pre>
        </details>
      )}
      <div
        className="text-muted"
        style={{ fontSize: '0.7rem', display: 'flex', gap: '1rem', flexWrap: 'wrap' }}
      >
        {drift && (
          <span>
            <strong>origin/main:</strong>{' '}
            <span style={{ fontFamily: 'var(--font-mono)' }}>
              {shortSha(drift.built_sha)} → {shortSha(drift.origin_main_sha)}
            </span>{' '}
            ({drift.behind_count} behind{drift.diverged ? ', diverged' : ''})
          </span>
        )}
        <span data-testid="drift-probe-line">
          <strong>Drift probe:</strong>{' '}
          {probe
            ? probe.state === 'pending'
              ? 'pending (never computed)'
              : `${probe.state}, ${probe.age_secs != null ? `${formatRelativeAgeSecs(probe.age_secs)} old` : 'age unknown'}, for ${shortSha(probe.computed_for_sha)}`
            : reading.kind === 'ok'
              ? 'absent'
              : 'unavailable'}
        </span>
        {lkg && lkgAgeSecs != null && (
          <span>
            <strong>LKG built:</strong> {formatRelativeAgeSecs(lkgAgeSecs)} ago ({lkg.built_at})
          </span>
        )}
      </div>
      {isWarn && (
        <div className="text-muted" style={{ fontSize: '0.7rem', marginTop: '0.4rem' }}>
          <strong>Remedy:</strong> Rebuild on this row builds <code>origin/main</code> (not your
          working tree) and restarts the runner. A restart ends live sessions in the runner — use
          &ldquo;Rebuild when idle&rdquo; to wait for them to drain.
        </div>
      )}
    </div>
  );
}
