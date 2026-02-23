import { useState, useEffect } from 'react';

type ToastVariant = 'success' | 'error' | 'info';
interface Toast {
  id: number;
  message: string;
  variant: ToastVariant;
  dismissing?: boolean;
}

let toastId = 0;
let globalAddToast: ((msg: string, variant: ToastVariant) => void) | null = null;

export function addToast(msg: string, variant: ToastVariant = 'info') {
  globalAddToast?.(msg, variant);
}

export function ToastContainer() {
  const [toasts, setToasts] = useState<Toast[]>([]);

  useEffect(() => {
    globalAddToast = (msg, variant) => {
      const id = ++toastId;
      setToasts((prev) => [...prev, { id, message: msg, variant }]);
      setTimeout(() => dismiss(id), 4000);
    };
    return () => {
      globalAddToast = null;
    };
  }, []);

  const dismiss = (id: number) => {
    setToasts((prev) => prev.map((t) => (t.id === id ? { ...t, dismissing: true } : t)));
    setTimeout(() => setToasts((prev) => prev.filter((t) => t.id !== id)), 200);
  };

  if (toasts.length === 0) return null;
  return (
    <div className="toast-container">
      {toasts.map((t) => (
        <div key={t.id} className={`toast toast-${t.variant}${t.dismissing ? ' dismissing' : ''}`}>
          <span>{t.message}</span>
          <button className="toast-close" onClick={() => dismiss(t.id)}>
            x
          </button>
        </div>
      ))}
    </div>
  );
}
