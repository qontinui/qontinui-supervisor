import { useEffect, useRef } from 'react';

export function useSSE<T>(
  url: string,
  eventName: string,
  onEvent: (data: T) => void,
  enabled = true,
) {
  const onEventRef = useRef(onEvent);
  onEventRef.current = onEvent;

  useEffect(() => {
    if (!enabled) return;

    let es: EventSource | null = null;
    let retryDelay = 1000;
    let retryTimer: ReturnType<typeof setTimeout>;
    let stopped = false;

    function connect() {
      if (stopped) return;
      es = new EventSource(url);

      es.onopen = () => {
        retryDelay = 1000; // reset on successful connection
      };

      es.addEventListener(eventName, (e: MessageEvent) => {
        try {
          const parsed = JSON.parse(e.data);
          onEventRef.current(parsed);
        } catch {
          /* ignore parse errors */
        }
      });

      es.onerror = () => {
        es?.close();
        if (!stopped) {
          retryTimer = setTimeout(() => {
            retryDelay = Math.min(retryDelay * 2, 30000);
            connect();
          }, retryDelay);
        }
      };
    }

    connect();

    return () => {
      stopped = true;
      es?.close();
      clearTimeout(retryTimer);
    };
  }, [url, eventName, enabled]);
}
