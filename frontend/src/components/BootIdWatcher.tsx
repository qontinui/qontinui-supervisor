import { useEffect, useRef } from 'react';

const BOOT_ID_URL = '/supervisor-bridge/boot-id';
const POLL_MS = 5000;
// If boot_id is missing this many polls in a row, log once. Catches the
// silent-stuck case where the endpoint is shadowed by a proxy or the
// response body is being rewritten.
const MISSING_BOOT_ID_LOG_AFTER = 6;

export function BootIdWatcher() {
  const seen = useRef<string | null>(null);
  const missingCount = useRef(0);
  const loggedMissing = useRef(false);
  useEffect(() => {
    let cancelled = false;
    const ctrl = new AbortController();
    const probe = async () => {
      try {
        const r = await fetch(BOOT_ID_URL, { signal: ctrl.signal });
        if (!r.ok) return;
        const body = await r.json();
        const id = body?.boot_id;
        if (!id) {
          missingCount.current += 1;
          if (missingCount.current >= MISSING_BOOT_ID_LOG_AFTER && !loggedMissing.current) {
            loggedMissing.current = true;
            console.warn(
              '[BootIdWatcher] /boot-id response missing boot_id — auto-reload disabled',
            );
          }
          return;
        }
        missingCount.current = 0;
        loggedMissing.current = false;
        if (seen.current === null) {
          seen.current = id;
          return;
        }
        if (seen.current !== id && !cancelled) {
          window.location.reload();
        }
      } catch {
        /* offline / aborted — treat as transient */
      }
    };
    probe();
    const t = setInterval(probe, POLL_MS);
    return () => {
      cancelled = true;
      clearInterval(t);
      ctrl.abort();
    };
  }, []);
  return null;
}
