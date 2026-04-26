import { useEffect, useRef } from 'react';

const HEARTBEAT_URL = '/supervisor-bridge/heartbeat';
const POLL_MS = 5000;

export function BootIdWatcher() {
  const seen = useRef<string | null>(null);
  useEffect(() => {
    let cancelled = false;
    const probe = async () => {
      try {
        const r = await fetch(HEARTBEAT_URL, { method: 'POST' });
        const body = await r.json();
        const id = body?.boot_id;
        if (!id) return;
        if (seen.current === null) {
          seen.current = id;
          return;
        }
        if (seen.current !== id && !cancelled) {
          window.location.reload();
        }
      } catch {
        /* offline — treat as transient */
      }
    };
    probe();
    const t = setInterval(probe, POLL_MS);
    return () => {
      cancelled = true;
      clearInterval(t);
    };
  }, []);
  return null;
}
