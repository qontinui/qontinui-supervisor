import React, { useState, useEffect, useCallback, useRef } from "react";
import { api, HealthResponse, DevStartResponse } from "../lib/api";

type ActionState = string | null;

interface StatusData {
  health: HealthResponse | null;
  services: { name: string; port: number; available: boolean }[];
  expo: Record<string, unknown> | null;
}

// Tracks a failed action: which service, what went wrong
interface ServiceError {
  service: string;
  stderr: string;
  stdout: string;
  action: string;
}

function SmallBtn({
  label,
  activeLabel,
  onClick,
  busy,
  busyKey,
  variant,
}: {
  label: string;
  activeLabel: string;
  onClick: () => void;
  busy: ActionState;
  busyKey?: string;
  variant?: "danger" | "warning";
}) {
  const isActive = busy === (busyKey ?? label);
  const style: React.CSSProperties = {
    padding: "0.2rem 0.5rem",
    fontSize: "0.75rem",
  };
  if (variant === "danger") {
    style.borderColor = "var(--danger)";
    style.color = "var(--danger)";
  } else if (variant === "warning") {
    style.borderColor = "var(--warning)";
    style.color = "var(--warning)";
  }
  return (
    <button
      className="btn"
      style={style}
      disabled={busy !== null}
      onClick={onClick}
    >
      {isActive ? activeLabel : label}
    </button>
  );
}

function StatusDot({ up, error }: { up: boolean; error?: boolean }) {
  const color = error
    ? "var(--warning)"
    : up
      ? "var(--success)"
      : "var(--danger)";
  return (
    <span
      style={{
        display: "inline-block",
        width: 8,
        height: 8,
        borderRadius: "50%",
        background: color,
        marginRight: 6,
      }}
    />
  );
}

// Map service row names to their log file type for error context
const SERVICE_LOG_MAP: Record<string, string> = {
  Runner: "runner-tauri",
  Backend: "backend-err",
  Frontend: "frontend-err",
};

interface ActionDef {
  key: string;
  display: string;
  activeLabel: string;
  service: string;
  fn: () => Promise<unknown>;
}

interface RowDef {
  name: string;
  port: string;
  up: boolean;
  actions?: ActionDef[];
}

export default function Dashboard() {
  const [data, setData] = useState<StatusData>({
    health: null,
    services: [],
    expo: null,
  });
  const [busy, setBusy] = useState<ActionState>(null);
  const [lastRefresh, setLastRefresh] = useState<Date | null>(null);
  const [errors, setErrors] = useState<Map<string, ServiceError>>(new Map());
  const [aiFixBusy, setAiFixBusy] = useState<string | null>(null);
  const mountedRef = useRef(true);

  const refresh = useCallback(async () => {
    const [health, devStatus, expo] = await Promise.allSettled([
      api.health(),
      api.devStartStatus(),
      api.expoStatus(),
    ]);
    if (!mountedRef.current) return;
    setData({
      health: health.status === "fulfilled" ? health.value : null,
      services:
        devStatus.status === "fulfilled"
          ? (devStatus.value.services ?? [])
          : [],
      expo: expo.status === "fulfilled" ? expo.value : null,
    });
    setLastRefresh(new Date());
  }, []);

  useEffect(() => {
    mountedRef.current = true;
    refresh();
    const id = setInterval(refresh, 5000);
    return () => {
      mountedRef.current = false;
      clearInterval(id);
    };
  }, [refresh]);

  // Run an action, detect failures from the response, and record errors
  const doAction = useCallback(
    (key: string, service: string, fn: () => Promise<unknown>) => {
      return async () => {
        setBusy(key);
        try {
          const result = await fn();
          // Check if the response indicates failure (DevStartResponse shape)
          const resp = result as DevStartResponse | undefined;
          if (
            resp &&
            typeof resp.status === "string" &&
            (resp.status === "error" || resp.status === "timeout")
          ) {
            setErrors((prev) => {
              const next = new Map(prev);
              next.set(service, {
                service,
                stderr: resp.stderr || "",
                stdout: resp.stdout || "",
                action: key,
              });
              return next;
            });
          } else {
            // Success — clear any previous error for this service
            setErrors((prev) => {
              if (!prev.has(service)) return prev;
              const next = new Map(prev);
              next.delete(service);
              return next;
            });
          }
        } catch {
          // Network/HTTP error
          setErrors((prev) => {
            const next = new Map(prev);
            next.set(service, {
              service,
              stderr: "Request failed",
              stdout: "",
              action: key,
            });
            return next;
          });
        }
        setBusy(null);
        setTimeout(refresh, 1500);
      };
    },
    [refresh],
  );

  // Trigger AI debug with service-specific context
  const triggerAiFix = useCallback(
    async (service: string) => {
      setAiFixBusy(service);
      try {
        // Gather error context
        const err = errors.get(service);
        const parts: string[] = [`Service "${service}" failed to start/load.`];

        if (err?.stderr) parts.push(`\nStderr:\n${err.stderr}`);
        if (err?.stdout) parts.push(`\nStdout:\n${err.stdout}`);

        // Try to fetch relevant log tail for extra context
        const logType = SERVICE_LOG_MAP[service];
        if (logType) {
          try {
            const log = await api.logFile(logType, 80);
            if (log.content?.trim()) {
              parts.push(
                `\nRecent ${logType} log (last 80 lines):\n${log.content}`,
              );
            }
          } catch {
            /* log may not exist */
          }
        }

        parts.push("\nPlease diagnose the root cause and fix the issue.");

        await api.aiDebug(parts.join("\n"));
      } catch {
        // AI debug endpoint may fail (cooldown, already running, etc.)
      }
      setAiFixBusy(null);
    },
    [errors],
  );

  const clearError = useCallback((service: string) => {
    setErrors((prev) => {
      if (!prev.has(service)) return prev;
      const next = new Map(prev);
      next.delete(service);
      return next;
    });
  }, []);

  const runner = data.health?.runner;
  const watchdog = data.health?.watchdog;
  const build = data.health?.build;
  const expo = data.expo;

  // If build has an error, surface it on the Runner row
  useEffect(() => {
    if (build?.error_detected && build.last_error) {
      setErrors((prev) => {
        if (prev.has("Runner") && prev.get("Runner")!.action === "build-error")
          return prev;
        const next = new Map(prev);
        next.set("Runner", {
          service: "Runner",
          stderr: build.last_error!,
          stdout: "",
          action: "build-error",
        });
        return next;
      });
    } else {
      setErrors((prev) => {
        if (!prev.has("Runner") || prev.get("Runner")!.action !== "build-error")
          return prev;
        const next = new Map(prev);
        next.delete("Runner");
        return next;
      });
    }
  }, [build?.error_detected, build?.last_error]);

  // Build service rows from port status + health data
  const svcMap = new Map(data.services.map((s) => [s.name, s]));

  const rows: RowDef[] = [
    {
      name: "Runner",
      port: "9876",
      up: !!runner?.running,
      actions: [
        {
          key: "runner-restart",
          display: "Restart",
          activeLabel: "Restarting…",
          service: "Runner",
          fn: () => api.runnerRestart(false),
        },
        {
          key: "runner-rebuild",
          display: "Rebuild",
          activeLabel: "Rebuilding…",
          service: "Runner",
          fn: () => api.runnerRestart(true),
        },
        {
          key: "runner-stop",
          display: "Stop",
          activeLabel: "Stopping…",
          service: "Runner",
          fn: () => api.runnerStop(),
        },
      ],
    },
    {
      name: "Backend",
      port: "8000",
      up: svcMap.get("backend")?.available ?? false,
      actions: [
        {
          key: "backend-start",
          display: "Start",
          activeLabel: "Starting…",
          service: "Backend",
          fn: () => api.devStartAction("backend"),
        },
        {
          key: "backend-stop",
          display: "Stop",
          activeLabel: "Stopping…",
          service: "Backend",
          fn: () => api.devStartAction("backend/stop"),
        },
      ],
    },
    {
      name: "Frontend",
      port: "3001",
      up: svcMap.get("frontend")?.available ?? false,
      actions: [
        {
          key: "frontend-start",
          display: "Start",
          activeLabel: "Starting…",
          service: "Frontend",
          fn: () => api.devStartAction("frontend"),
        },
        {
          key: "frontend-stop",
          display: "Stop",
          activeLabel: "Stopping…",
          service: "Frontend",
          fn: () => api.devStartAction("frontend/stop"),
        },
      ],
    },
    {
      name: "PostgreSQL",
      port: "5432",
      up: svcMap.get("postgresql")?.available ?? false,
    },
    {
      name: "Redis",
      port: "6379",
      up: svcMap.get("redis")?.available ?? false,
    },
    {
      name: "MinIO",
      port: "9000",
      up: svcMap.get("minio")?.available ?? false,
    },
    {
      name: "Vite",
      port: "1420",
      up: svcMap.get("vite")?.available ?? false,
    },
    {
      name: "Expo",
      port: "8081",
      up: !!expo?.running,
      actions: [
        {
          key: "expo-start",
          display: "Start",
          activeLabel: "Starting…",
          service: "Expo",
          fn: () => api.expoStart(),
        },
        {
          key: "expo-stop",
          display: "Stop",
          activeLabel: "Stopping…",
          service: "Expo",
          fn: () => api.expoStop(),
        },
      ],
    },
    {
      name: "Watchdog",
      port: "—",
      up: !!watchdog?.enabled,
    },
  ];

  return (
    <div>
      <div className="page-header">
        <h1 className="page-title">Dashboard</h1>
        <div className="flex items-center gap-2">
          {lastRefresh && (
            <span className="text-muted" style={{ fontSize: "0.75rem" }}>
              {lastRefresh.toLocaleTimeString()}
            </span>
          )}
          <button className="btn" onClick={refresh} disabled={busy !== null}>
            Refresh
          </button>
        </div>
      </div>

      <div className="card" style={{ marginBottom: "1rem" }}>
        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th>Service</th>
                <th style={{ width: 70 }}>Port</th>
                <th style={{ width: 80 }}>Status</th>
                <th>Actions</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => {
                const err = errors.get(row.name);
                return (
                  <React.Fragment key={row.name}>
                    <tr>
                      <td style={{ fontFamily: "inherit", fontWeight: 500 }}>
                        {row.name}
                      </td>
                      <td>{row.port}</td>
                      <td>
                        <StatusDot up={row.up} error={!!err} />
                        <span
                          className={
                            err
                              ? "text-warning"
                              : row.up
                                ? "text-success"
                                : "text-danger"
                          }
                          style={{ fontSize: "0.75rem" }}
                        >
                          {err
                            ? "ERR"
                            : row.name === "Watchdog"
                              ? row.up
                                ? "ON"
                                : "OFF"
                              : row.up
                                ? "UP"
                                : "DOWN"}
                        </span>
                      </td>
                      <td>
                        <div className="flex gap-2">
                          {row.actions?.map((a) => (
                            <SmallBtn
                              key={a.key}
                              label={a.display}
                              activeLabel={a.activeLabel}
                              onClick={doAction(a.key, a.service, a.fn)}
                              busy={busy}
                              busyKey={a.key}
                            />
                          ))}
                          {err && (
                            <SmallBtn
                              label="AI Fix"
                              activeLabel="Sending…"
                              onClick={() => triggerAiFix(row.name)}
                              busy={aiFixBusy}
                              busyKey={row.name}
                              variant="warning"
                            />
                          )}
                        </div>
                      </td>
                    </tr>
                    {err && (
                      <tr>
                        <td
                          colSpan={4}
                          style={{
                            padding: "0 0.75rem 0.5rem",
                            borderBottom: "1px solid var(--border)",
                          }}
                        >
                          <div
                            style={{
                              background: "rgba(239,68,68,0.08)",
                              border: "1px solid rgba(239,68,68,0.2)",
                              borderRadius: 4,
                              padding: "0.4rem 0.6rem",
                              fontSize: "0.75rem",
                              fontFamily: "var(--font-mono)",
                              whiteSpace: "pre-wrap",
                              maxHeight: 120,
                              overflowY: "auto",
                              position: "relative",
                            }}
                          >
                            <button
                              onClick={() => clearError(row.name)}
                              style={{
                                position: "absolute",
                                top: 2,
                                right: 6,
                                background: "none",
                                border: "none",
                                color: "var(--text-muted)",
                                cursor: "pointer",
                                fontSize: "0.8rem",
                                padding: "0 4px",
                              }}
                              title="Dismiss"
                            >
                              x
                            </button>
                            {(err.stderr || err.stdout).trim() ||
                              "Action failed (no output)"}
                          </div>
                        </td>
                      </tr>
                    )}
                  </React.Fragment>
                );
              })}
            </tbody>
          </table>
        </div>
      </div>

      <div className="card">
        <div className="card-header" style={{ marginBottom: "0.5rem" }}>
          <span className="card-title">Bulk Actions</span>
        </div>
        <div className="flex gap-2" style={{ flexWrap: "wrap" }}>
          <SmallBtn
            label="Docker"
            activeLabel="Starting…"
            onClick={doAction("Docker", "Docker", () =>
              api.devStartAction("docker"),
            )}
            busy={busy}
          />
          <SmallBtn
            label="Stop Docker"
            activeLabel="Stopping…"
            onClick={doAction("Stop Docker", "Docker", () =>
              api.devStartAction("docker/stop"),
            )}
            busy={busy}
          />
          <span
            style={{
              borderLeft: "1px solid var(--border)",
              margin: "0 0.25rem",
            }}
          />
          <SmallBtn
            label="Start All"
            activeLabel="Starting…"
            onClick={doAction("Start All", "All", () =>
              api.devStartAction("all"),
            )}
            busy={busy}
          />
          <SmallBtn
            label="Stop All"
            activeLabel="Stopping…"
            onClick={doAction("Stop All", "All", () =>
              api.devStartAction("stop"),
            )}
            busy={busy}
          />
          <span
            style={{
              borderLeft: "1px solid var(--border)",
              margin: "0 0.25rem",
            }}
          />
          <SmallBtn
            label="Clean"
            activeLabel="Cleaning…"
            onClick={doAction("Clean", "Clean", () =>
              api.devStartAction("clean"),
            )}
            busy={busy}
          />
          <SmallBtn
            label="Fresh"
            activeLabel="Starting…"
            onClick={doAction("Fresh", "Fresh", () =>
              api.devStartAction("fresh"),
            )}
            busy={busy}
          />
          <SmallBtn
            label="Migrate"
            activeLabel="Migrating…"
            onClick={doAction("Migrate", "Migrate", () =>
              api.devStartAction("migrate"),
            )}
            busy={busy}
          />
        </div>
      </div>
    </div>
  );
}
