import { useState } from 'react';
import type { RunnerDerivedStatus, UiErrorSummary } from '../lib/api';

interface RunnerStatusBadgeProps {
  /// Derived status published by the supervisor. Optional so callers reading
  /// from older `/runners` entries still render a sensible fallback.
  derivedStatus?: RunnerDerivedStatus;
  /// UI-level error reported by the runner, if any. When present, the badge
  /// becomes clickable and exposes a details panel.
  uiError?: UiErrorSummary | null;
  /// Optional extra inline style for the badge wrapper (e.g. smaller font).
  style?: React.CSSProperties;
  /// If the runner does not expose a derived_status (older supervisor build
  /// or degraded cache), fall back to this up/down boolean.
  fallbackUp?: boolean;
}

interface StatusDisplay {
  label: string;
  badgeClass: string;
}

/// Map a derived status variant to a badge class + user-facing label.
///
/// Color mapping (matches the spec):
///   healthy   → green  (badge-success)
///   degraded  → yellow (badge-warning)
///   errored   → red    (badge-danger)
///   offline   → gray   (badge-secondary)
///   starting  → blue   (badge-info)
function statusDisplay(
  status: RunnerDerivedStatus | undefined,
  fallbackUp: boolean | undefined,
): StatusDisplay {
  if (!status) {
    // Older supervisor or lock contention — fall back to the up/down signal
    // the caller has at hand.
    if (fallbackUp === true) return { label: 'Healthy', badgeClass: 'badge-success' };
    if (fallbackUp === false) return { label: 'Offline', badgeClass: 'badge-secondary' };
    return { label: 'Unknown', badgeClass: 'badge-warning' };
  }
  switch (status.kind) {
    case 'healthy':
      return { label: 'Healthy', badgeClass: 'badge-success' };
    case 'degraded':
      return { label: 'Degraded', badgeClass: 'badge-warning' };
    case 'errored':
      return { label: 'Errored', badgeClass: 'badge-danger' };
    case 'offline':
      return { label: 'Offline', badgeClass: 'badge-secondary' };
    case 'starting':
      return { label: 'Starting', badgeClass: 'badge-info' };
    default:
      // Exhaustiveness guard — if the Rust side adds a new variant, we
      // render a neutral "Unknown" instead of silently dropping it.
      return { label: 'Unknown', badgeClass: 'badge-warning' };
  }
}

function formatTimestamp(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleString();
}

/// Build the `title` tooltip text shown on hover. Summarizes the UI error in
/// a single line without markup (browser tooltips don't render HTML).
function buildTooltip(uiError: UiErrorSummary): string {
  return `${uiError.message}\n(reported at ${formatTimestamp(uiError.reported_at)}, count=${uiError.count})`;
}

/// Status badge for a single runner. Clicking the badge when a ui_error is
/// attached toggles an inline details panel (no modal); otherwise the badge
/// is inert.
export function RunnerStatusBadge({
  derivedStatus,
  uiError,
  style,
  fallbackUp,
}: RunnerStatusBadgeProps) {
  const [expanded, setExpanded] = useState(false);
  const { label, badgeClass } = statusDisplay(derivedStatus, fallbackUp);
  const hasError = uiError != null;
  const tooltip = hasError ? buildTooltip(uiError) : undefined;

  const toggle = () => {
    if (hasError) setExpanded((v) => !v);
  };

  return (
    <>
      <span
        className={`badge ${badgeClass}${hasError ? ' badge-clickable' : ''}`}
        style={style}
        title={tooltip}
        role={hasError ? 'button' : undefined}
        tabIndex={hasError ? 0 : undefined}
        onClick={toggle}
        onKeyDown={(e) => {
          if (hasError && (e.key === 'Enter' || e.key === ' ')) {
            e.preventDefault();
            toggle();
          }
        }}
      >
        {label}
        {hasError && (
          <span style={{ marginLeft: '0.3rem', opacity: 0.7, fontSize: '0.65rem' }}>
            {expanded ? '▾' : '▸'}
          </span>
        )}
      </span>
      {expanded && hasError && <UiErrorPanel uiError={uiError} />}
    </>
  );
}

interface UiErrorPanelProps {
  uiError: UiErrorSummary;
}

/// Inline details panel showing the full ui_error payload. Rendered next to
/// the badge, below it on its own row — callers typically wrap the badge in
/// a flex container so this panel wraps to the next line.
function UiErrorPanel({ uiError }: UiErrorPanelProps) {
  return (
    <div
      style={{
        flexBasis: '100%',
        marginTop: '0.5rem',
        padding: '0.6rem 0.75rem',
        background: 'rgba(239,68,68,0.08)',
        border: '1px solid rgba(239,68,68,0.3)',
        borderRadius: 4,
        fontSize: '0.75rem',
      }}
    >
      <div style={{ marginBottom: '0.4rem' }}>
        <strong className="text-danger">UI Error:</strong>{' '}
        <span style={{ fontFamily: 'var(--font-mono)' }}>{uiError.message}</span>
      </div>
      <div
        className="text-muted"
        style={{ fontSize: '0.7rem', marginBottom: '0.4rem', display: 'flex', gap: '1rem', flexWrap: 'wrap' }}
      >
        <span>
          <strong>First seen:</strong> {formatTimestamp(uiError.first_seen)}
        </span>
        <span>
          <strong>Last reported:</strong> {formatTimestamp(uiError.reported_at)}
        </span>
        <span>
          <strong>Count:</strong> {uiError.count}
        </span>
        {uiError.digest && (
          <span>
            <strong>Digest:</strong>{' '}
            <span style={{ fontFamily: 'var(--font-mono)' }}>{uiError.digest}</span>
          </span>
        )}
      </div>
      {uiError.stack && (
        <details style={{ marginBottom: '0.4rem' }}>
          <summary className="text-muted" style={{ fontSize: '0.7rem', cursor: 'pointer' }}>
            Stack trace
          </summary>
          <pre
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
            {uiError.stack}
          </pre>
        </details>
      )}
      {uiError.component_stack && (
        <details>
          <summary className="text-muted" style={{ fontSize: '0.7rem', cursor: 'pointer' }}>
            Component stack
          </summary>
          <pre
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
            {uiError.component_stack}
          </pre>
        </details>
      )}
    </div>
  );
}
