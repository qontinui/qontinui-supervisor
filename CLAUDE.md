# Qontinui Supervisor

Rust-based process manager for the qontinui-runner. Replaces the Python `dev-supervisor.py` for core process lifecycle management.

## Architecture

Standalone Axum HTTP server that manages the runner process:
- **Start/stop/restart** with optional cargo rebuild
- **Watchdog** auto-recovery with crash loop detection
- **Log capture** with SSE streaming and circular buffer
- **Build error detection** during first 60s of runner startup

## Building & Running

```bash
cargo build                    # Build debug binary
cargo check                    # Type-check only
cargo fmt                      # Format code
cargo clippy -- -D warnings    # Lint

# Start in dev mode with watchdog
./target/debug/qontinui-supervisor -p ../qontinui-runner/src-tauri -d -w

# Start in exe mode (no Vite, runs compiled binary directly)
./target/debug/qontinui-supervisor -p ../qontinui-runner/src-tauri -a
```

## CLI Flags

| Flag | Description |
|------|-------------|
| `-p, --project-dir` | Path to `qontinui-runner/src-tauri` (required) |
| `-d, --dev-mode` | Run `npm run tauri dev` instead of compiled exe |
| `-w, --watchdog` | Enable watchdog (implies auto-start) |
| `-a, --auto-start` | Start runner on supervisor launch |
| `-l, --log-file` | Log file for runner output |
| `--port` | Supervisor HTTP port (default: 9875) |

## API Endpoints

| Method | Path | Description |
|--------|------|-------------|
| GET | `/health` | Comprehensive status |
| POST | `/runner/stop` | Stop runner + cleanup |
| POST | `/runner/restart` | Stop + rebuild + start. Body: `{"rebuild": bool}` |
| POST | `/runner/watchdog` | Control watchdog. Body: `{"enabled": bool, "reset_attempts": bool}` |
| GET | `/logs/history` | Recent log entries from circular buffer |
| GET | `/logs/stream` | SSE stream of real-time log events |
| GET | `/logs/file/{type}` | Read `.dev-logs/` files |
| GET | `/logs/files` | List available log files |
| POST | `/supervisor/restart` | Self-restart with same CLI args |

## Key Constants

| Constant | Value |
|----------|-------|
| Supervisor port | 9875 |
| Runner API port | 9876 |
| Vite port | 1420 |
| Watchdog check interval | 10s |
| Max restart attempts | 3 |
| Crash loop threshold | 5 crashes in 10min |
| Restart cooldown | 60s |
| Build timeout | 5min |
| Log buffer | 500 entries |

## Code Standards

- Idiomatic Rust, `Result` types for errors
- `tracing` for logging, `thiserror` for error types
- `cargo fmt` and `cargo clippy -D warnings` enforced via pre-commit hooks
