import { useEffect, useState } from 'react';
import { api, MonitorConfig } from '../lib/api';

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
