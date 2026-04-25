import { useEffect, useState } from 'react';
import { api, DetectedMonitor, MonitorConfig } from '../lib/api';

interface DraftMonitor extends MonitorConfig {
  // Stable per-row id so React keys survive label edits and reorders.
  _rid: string;
  // numeric fields edited as strings while typing so users can type "-" or
  // clear the field without the value snapping to 0.
  xText: string;
  yText: string;
  widthText: string;
  heightText: string;
}

let RID_SEQ = 0;
function nextRid(): string {
  RID_SEQ += 1;
  return `m${RID_SEQ}`;
}

function toDraft(m: MonitorConfig): DraftMonitor {
  return {
    ...m,
    _rid: nextRid(),
    xText: String(m.x),
    yText: String(m.y),
    widthText: String(m.width),
    heightText: String(m.height),
  };
}

function fromDraft(d: DraftMonitor): MonitorConfig {
  return {
    label: d.label,
    x: parseInt(d.xText, 10) || 0,
    y: parseInt(d.yText, 10) || 0,
    width: parseInt(d.widthText, 10) || 0,
    height: parseInt(d.heightText, 10) || 0,
    enabled: d.enabled,
  };
}

const NEW_MONITOR: MonitorConfig = {
  label: 'New monitor',
  x: 0,
  y: 0,
  width: 1920,
  height: 1080,
  enabled: true,
};

export default function SpawnMonitors() {
  const [drafts, setDrafts] = useState<DraftMonitor[]>([]);
  const [nextIndex, setNextIndex] = useState(0);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [savedAt, setSavedAt] = useState<number | null>(null);
  const [detected, setDetected] = useState<DetectedMonitor[] | null>(null);
  const [detecting, setDetecting] = useState(false);

  const refresh = async () => {
    setLoading(true);
    setError(null);
    try {
      const res = await api.getSpawnMonitors();
      setDrafts(res.monitors.map(toDraft));
      setNextIndex(res.next_index);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    refresh();
  }, []);

  const update = (i: number, patch: Partial<DraftMonitor>) => {
    setDrafts((prev) => prev.map((d, idx) => (idx === i ? { ...d, ...patch } : d)));
  };

  const remove = (i: number) => {
    setDrafts((prev) => prev.filter((_, idx) => idx !== i));
  };

  const add = () => {
    setDrafts((prev) => [...prev, toDraft(NEW_MONITOR)]);
  };

  const save = async () => {
    setSaving(true);
    setError(null);
    try {
      const res = await api.putSpawnMonitors(drafts.map(fromDraft));
      setDrafts(res.monitors.map(toDraft));
      setNextIndex(res.next_index);
      setSavedAt(Date.now());
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };

  const detect = async () => {
    setDetecting(true);
    setError(null);
    try {
      const res = await api.getDetectedMonitors();
      setDetected(res.monitors);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setDetecting(false);
    }
  };

  const dismissDetected = () => setDetected(null);

  const applyDetected = async () => {
    if (!detected) return;
    // Preserve existing enabled flags by label match. New labels default to
    // enabled=true for non-primary, enabled=false for primary (most common
    // user pattern: spawn windows onto secondaries).
    const currentByLabel = new Map<string, MonitorConfig>(
      drafts.map((d) => [d.label, fromDraft(d)]),
    );
    const next: MonitorConfig[] = detected.map((m) => {
      const prev = currentByLabel.get(m.label);
      const enabled = prev ? prev.enabled : !m.is_primary;
      return {
        label: m.label,
        x: m.x,
        y: m.y,
        width: m.width,
        height: m.height,
        enabled,
      };
    });
    setSaving(true);
    setError(null);
    try {
      const res = await api.putSpawnMonitors(next);
      setDrafts(res.monitors.map(toDraft));
      setNextIndex(res.next_index);
      setSavedAt(Date.now());
      setDetected(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };

  // True when a detected entry has a label match with identical x/y/w/h in the
  // current persisted-but-not-yet-edited drafts.
  const detectedMatchesCurrent = (m: DetectedMonitor): boolean => {
    const cur = drafts.find((d) => d.label === m.label);
    if (!cur) return false;
    const c = fromDraft(cur);
    return c.x === m.x && c.y === m.y && c.width === m.width && c.height === m.height;
  };

  const enabledCount = drafts.filter((d) => d.enabled).length;
  const enabledLabels = drafts
    .filter((d) => d.enabled)
    .map((d) => d.label)
    .join(' → ');

  return (
    <div>
      <div className="page-header">
        <div className="page-title">Spawn Monitors</div>
        <div style={{ display: 'flex', gap: '0.5rem' }}>
          <button
            className="btn"
            onClick={detect}
            disabled={loading || saving || detecting}
            title="Query the OS for the current monitor layout"
          >
            {detecting ? 'Detecting…' : 'Detect Monitors'}
          </button>
          <button className="btn" onClick={refresh} disabled={loading || saving}>
            Reload
          </button>
          <button className="btn btn-primary" onClick={save} disabled={loading || saving}>
            {saving ? 'Saving…' : 'Save'}
          </button>
        </div>
      </div>
      <div className="page-desc">
        Where the supervisor places spawned temp runner windows. Each
        <code> spawn-test </code> picks the next enabled monitor (round-robin) and
        passes its rect to the runner via
        <code> QONTINUI_WINDOW_X/Y/WIDTH/HEIGHT</code>. Coordinates are
        absolute virtual-desktop pixels — left of the primary monitor uses
        negative X.
      </div>

      {error && (
        <div
          className="card"
          style={{ borderColor: 'var(--danger)', color: 'var(--danger)', marginBottom: '1rem' }}
        >
          {error}
        </div>
      )}

      <div className="card mb-2" style={{ display: 'flex', gap: '2rem', flexWrap: 'wrap' }}>
        <div>
          <div className="card-title">Enabled</div>
          <div className="stat-value">
            {enabledCount} / {drafts.length}
          </div>
        </div>
        <div style={{ flex: 1, minWidth: 200 }}>
          <div className="card-title">Round-robin order</div>
          <div className="text-mono" style={{ marginTop: '0.5rem' }}>
            {enabledLabels || '— no monitors enabled —'}
          </div>
          <div className="text-muted" style={{ marginTop: '0.25rem', fontSize: '0.75rem' }}>
            Next spawn: index {enabledCount > 0 ? nextIndex % enabledCount : '—'}
          </div>
        </div>
        {savedAt && (
          <div className="text-success" style={{ alignSelf: 'center' }}>
            Saved {new Date(savedAt).toLocaleTimeString()}
          </div>
        )}
      </div>

      {detected !== null && (
        <div className="card mb-2">
          <div
            style={{
              display: 'flex',
              justifyContent: 'space-between',
              alignItems: 'center',
              marginBottom: '0.5rem',
            }}
          >
            <div className="card-title">Detected monitors</div>
            <div style={{ display: 'flex', gap: '0.5rem' }}>
              <button
                className="btn btn-primary"
                onClick={applyDetected}
                disabled={saving || detected.length === 0}
              >
                Apply detected layout
              </button>
              <button className="btn" onClick={dismissDetected} disabled={saving}>
                Dismiss
              </button>
            </div>
          </div>
          {detected.length === 0 ? (
            <div className="text-muted">
              Monitor detection is only supported on Windows. Edit values manually below.
            </div>
          ) : (
            <table style={{ width: '100%', fontSize: '0.85rem', borderCollapse: 'collapse' }}>
              <thead>
                <tr style={{ textAlign: 'left', color: 'var(--text-secondary)' }}>
                  <th style={{ padding: '0.25rem 0.5rem' }}> </th>
                  <th style={{ padding: '0.25rem 0.5rem' }}>Label</th>
                  <th style={{ padding: '0.25rem 0.5rem' }}>X</th>
                  <th style={{ padding: '0.25rem 0.5rem' }}>Y</th>
                  <th style={{ padding: '0.25rem 0.5rem' }}>Width</th>
                  <th style={{ padding: '0.25rem 0.5rem' }}>Height</th>
                  <th style={{ padding: '0.25rem 0.5rem' }}>Primary</th>
                </tr>
              </thead>
              <tbody>
                {detected.map((m, i) => {
                  const matches = detectedMatchesCurrent(m);
                  return (
                    <tr
                      key={`${m.label}-${i}`}
                      style={{ borderTop: '1px solid var(--border)' }}
                    >
                      <td
                        style={{
                          padding: '0.25rem 0.5rem',
                          color: matches ? 'var(--text-success, #4caf50)' : 'var(--warning, #e0b341)',
                          fontWeight: 'bold',
                        }}
                        title={matches ? 'Matches current config' : 'Would change on apply'}
                      >
                        {matches ? '✓' : '→'}
                      </td>
                      <td className="text-mono" style={{ padding: '0.25rem 0.5rem' }}>
                        {m.label}
                      </td>
                      <td className="text-mono" style={{ padding: '0.25rem 0.5rem' }}>
                        {m.x}
                      </td>
                      <td className="text-mono" style={{ padding: '0.25rem 0.5rem' }}>
                        {m.y}
                      </td>
                      <td className="text-mono" style={{ padding: '0.25rem 0.5rem' }}>
                        {m.width}
                      </td>
                      <td className="text-mono" style={{ padding: '0.25rem 0.5rem' }}>
                        {m.height}
                      </td>
                      <td style={{ padding: '0.25rem 0.5rem' }}>
                        {m.is_primary ? (
                          <span
                            style={{
                              fontSize: '0.7rem',
                              padding: '0.1rem 0.4rem',
                              borderRadius: 4,
                              background: 'var(--bg-tertiary)',
                              color: 'var(--text-secondary)',
                            }}
                          >
                            primary
                          </span>
                        ) : null}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          )}
        </div>
      )}

      {loading && <div className="text-muted">Loading…</div>}

      <div className="card-grid">
        {drafts.map((d, i) => (
          <MonitorCard
            key={d._rid}
            draft={d}
            onChange={(patch) => update(i, patch)}
            onRemove={() => remove(i)}
          />
        ))}
      </div>

      <button className="btn" onClick={add}>
        + Add monitor
      </button>
    </div>
  );
}

interface MonitorCardProps {
  draft: DraftMonitor;
  onChange: (patch: Partial<DraftMonitor>) => void;
  onRemove: () => void;
}

function MonitorCard({ draft, onChange, onRemove }: MonitorCardProps) {
  return (
    <div className="card">
      <div className="card-header">
        <input
          value={draft.label}
          onChange={(e) => onChange({ label: e.target.value })}
          className="text-mono"
          style={{
            background: 'transparent',
            border: '1px solid var(--border)',
            color: 'var(--text-primary)',
            padding: '0.25rem 0.5rem',
            borderRadius: 4,
            fontSize: '0.95rem',
            flex: 1,
            marginRight: '0.5rem',
          }}
        />
        <label
          style={{
            display: 'flex',
            alignItems: 'center',
            gap: '0.25rem',
            fontSize: '0.8rem',
            color: 'var(--text-secondary)',
          }}
        >
          <input
            type="checkbox"
            checked={draft.enabled}
            onChange={(e) => onChange({ enabled: e.target.checked })}
          />
          Enabled
        </label>
      </div>

      <div
        style={{
          display: 'grid',
          gridTemplateColumns: '1fr 1fr',
          gap: '0.5rem 0.75rem',
          fontSize: '0.8rem',
        }}
      >
        <NumberField
          label="X"
          value={draft.xText}
          onChange={(v) => onChange({ xText: v })}
        />
        <NumberField
          label="Y"
          value={draft.yText}
          onChange={(v) => onChange({ yText: v })}
        />
        <NumberField
          label="Width"
          value={draft.widthText}
          onChange={(v) => onChange({ widthText: v })}
        />
        <NumberField
          label="Height"
          value={draft.heightText}
          onChange={(v) => onChange({ heightText: v })}
        />
      </div>

      <div style={{ marginTop: '0.75rem', display: 'flex', justifyContent: 'flex-end' }}>
        <button
          className="btn"
          style={{ color: 'var(--danger)', borderColor: 'var(--danger)' }}
          onClick={onRemove}
        >
          Remove
        </button>
      </div>
    </div>
  );
}

interface NumberFieldProps {
  label: string;
  value: string;
  onChange: (v: string) => void;
}

function NumberField({ label, value, onChange }: NumberFieldProps) {
  return (
    <label style={{ display: 'flex', flexDirection: 'column', gap: '0.25rem' }}>
      <span className="text-muted" style={{ fontSize: '0.7rem' }}>
        {label}
      </span>
      <input
        type="text"
        inputMode="numeric"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className="text-mono"
        style={{
          background: 'var(--bg-tertiary)',
          border: '1px solid var(--border)',
          color: 'var(--text-primary)',
          padding: '0.25rem 0.5rem',
          borderRadius: 4,
        }}
      />
    </label>
  );
}
