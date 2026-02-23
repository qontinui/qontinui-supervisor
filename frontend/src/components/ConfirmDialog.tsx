import { useState, useEffect } from 'react';

interface ConfirmState {
  title: string;
  message: string;
  resolve: (ok: boolean) => void;
}

let globalConfirm: ((title: string, message: string) => Promise<boolean>) | null = null;

export function confirm(title: string, message: string): Promise<boolean> {
  return globalConfirm ? globalConfirm(title, message) : Promise.resolve(true);
}

export function ConfirmDialog() {
  const [state, setState] = useState<ConfirmState | null>(null);

  useEffect(() => {
    globalConfirm = (title, message) =>
      new Promise((resolve) => setState({ title, message, resolve }));
    return () => {
      globalConfirm = null;
    };
  }, []);

  if (!state) return null;

  const answer = (ok: boolean) => {
    state.resolve(ok);
    setState(null);
  };

  return (
    <div className="confirm-overlay" onClick={() => answer(false)}>
      <div className="confirm-dialog" onClick={(e) => e.stopPropagation()}>
        <h3>{state.title}</h3>
        <p>{state.message}</p>
        <div className="confirm-actions">
          <button className="btn" onClick={() => answer(false)}>
            Cancel
          </button>
          <button
            className="btn"
            style={{ borderColor: 'var(--danger)', color: 'var(--danger)' }}
            onClick={() => answer(true)}
          >
            Confirm
          </button>
        </div>
      </div>
    </div>
  );
}
