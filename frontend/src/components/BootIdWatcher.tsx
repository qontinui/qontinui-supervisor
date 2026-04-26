/**
 * BootIdWatcher
 *
 * Fallback watcher for catastrophic boot_id changes. Since commit 297a32a, the
 * supervisor persists its `boot_id` to disk and reuses it across normal
 * restarts, so a fresh boot_id is no longer the canonical "supervisor was
 * restarted" signal. Routine restarts no longer trigger a reload here.
 *
 * This watcher only fires when boot_id genuinely changes — i.e. the persistence
 * file was deleted, corrupted, or wiped (e.g. a `target/` clean, manual
 * removal, or a fresh checkout). In those cases we still want connected tabs
 * to reload so they don't keep talking to a fundamentally different supervisor
 * instance.
 *
 * For the normal "a new bundle is available" UX, see BuildRefreshBanner —
 * that's the primary signal users should rely on.
 *
 * Migration note: the very first deploy of the persist-boot_id change will
 * cause a one-time boot_id transition (in-memory UUID → newly persisted UUID)
 * and existing tabs will auto-reload once. After that, restarts are silent.
 */
import { useEffect, useRef } from 'react';

const BOOT_ID_URL = '/supervisor-bridge/boot-id';
const POLL_MS = 60000;
// If boot_id is missing this many polls in a row, log once. Catches the
// silent-stuck case where the endpoint is shadowed by a proxy or the
// response body is being rewritten. At a 60s cadence, 2 polls = 2 minutes,
// which is short enough to surface real misconfigurations but long enough
// to ride out transient blips.
const MISSING_BOOT_ID_LOG_AFTER = 2;

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
          console.info(
            '[BootIdWatcher] supervisor boot_id changed (' + seen.current + ' → ' + id +
            ') — reloading. If this is your first deploy after the persist-boot_id change, ' +
            'this is the one-time migration; future restarts will not auto-reload.'
          );
          window.location.reload();
        }
      } catch {
        /* offline / aborted — treat as transient */
      }
    };
    probe();
    const t = setInterval(probe, POLL_MS);
    const onVisibilityChange = () => {
      if (document.visibilityState === 'visible') {
        probe();
      }
    };
    document.addEventListener('visibilitychange', onVisibilityChange);
    return () => {
      cancelled = true;
      clearInterval(t);
      document.removeEventListener('visibilitychange', onVisibilityChange);
      ctrl.abort();
    };
  }, []);
  return null;
}
