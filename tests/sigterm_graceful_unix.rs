//! SIGTERM is a graceful shutdown (plan
//! `2026-11-04-linux-dev-supervisor-dies-with-its-launcher-and-orphans-its-temp-runners`,
//! Phase 1, test (a)).
//!
//! Before that phase `shutdown_signal` selected only Ctrl-C and the HTTP
//! trigger, so `kill <pid>` and `systemctl stop` — both SIGTERM — killed the
//! supervisor on the default disposition: no `Received shutdown signal` line,
//! no bounded drain, no temp-runner sweep. This boots the REAL binary and
//! sends it a real SIGTERM.
//!
//! **Hermetic by construction.** A supervisor booted with the operator's real
//! environment would share the live supervisor's temp-runner ledger
//! (`dirs::data_local_dir()/com.qontinui.supervisor`) — and its startup
//! reconcile sweep kills ledger-proven temp runners — and would publish this
//! device's budget to coord. So the child runs with a CLEARED environment:
//! `HOME`, `XDG_DATA_HOME` and `XDG_CONFIG_HOME` point into a tempdir, no
//! `COORD_HTTP_URL` and no `~/.qontinui/profiles.json` exist (so
//! `coord_client::coord_base` resolves nothing and no coord call or device-JWT
//! mint happens), and only `PATH` is carried over. The test then ASSERTS,
//! from the binary's own startup line, that its ledger resolved under the
//! tempdir — the property the isolation exists for, observed rather than
//! assumed. It binds an OS-allocated port, never 9875.
#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BOOT_BOUND: Duration = Duration::from_secs(90);
const EXIT_BOUND: Duration = Duration::from_secs(15);
/// The live dev supervisor's port. This test must never touch it.
const LIVE_SUPERVISOR_PORT: u16 = 9875;

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an OS-allocated port");
    listener.local_addr().expect("local addr").port()
}

/// `GET /health` over a raw socket; `Some(status)` when anything answered.
fn health_status(port: u16) -> Option<u16> {
    let mut stream =
        TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(2)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let req =
        format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = [0u8; 64];
    let n = stream.read(&mut buf).ok()?;
    let head = String::from_utf8_lossy(&buf[..n]);
    head.split_whitespace().nth(1)?.parse().ok()
}

/// The captured output with ANSI colour sequences removed (tracing's fmt
/// layer colours its output by default).
fn read_lossy(p: &Path) -> String {
    let raw = std::fs::read(p)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Kill the child if the test bails out early, so a red never leaks a process.
struct KillOnDrop(Option<Child>);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn sigterm_is_a_graceful_shutdown_and_names_its_signal() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let home = root.join("home");
    let data = root.join("xdg-data");
    let config = root.join("xdg-config");
    // `.dev-logs` resolves to the project dir's GRANDPARENT, so nest the
    // project two levels down to keep that under the tempdir too.
    let project = root.join("ws").join("checkout").join("project");
    let log_dir = root.join("logs");
    for d in [&home, &data, &config, &project, &log_dir] {
        std::fs::create_dir_all(d).expect("create test dir");
    }
    let out_path = root.join("supervisor.out");
    let out = std::fs::File::create(&out_path).expect("create stdout capture");
    let err = out.try_clone().expect("clone stdout capture");

    let port = free_port();
    assert_ne!(
        port, LIVE_SUPERVISOR_PORT,
        "the OS handed out the live supervisor's port"
    );

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_qontinui-supervisor"));
    cmd.env_clear()
        .env("HOME", &home)
        .env("XDG_DATA_HOME", &data)
        .env("XDG_CONFIG_HOME", &config)
        .env("RUST_LOG", "qontinui_supervisor=info")
        .env("NO_COLOR", "1");
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    cmd.arg("--port")
        .arg(port.to_string())
        .arg("--log-dir")
        .arg(&log_dir)
        .arg("--project-dir")
        .arg(&project)
        .arg("--no-prewarm")
        .arg("--no-webview")
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    let mut guard = KillOnDrop(Some(cmd.spawn().expect("spawn the supervisor binary")));
    let pid = guard.0.as_ref().expect("child").id();

    // Boot: /health must answer 200.
    let boot_started = Instant::now();
    let mut booted = false;
    while boot_started.elapsed() < BOOT_BOUND {
        if let Some(status) = guard.0.as_mut().and_then(|c| c.try_wait().ok()).flatten() {
            panic!(
                "the supervisor binary could not boot hermetically: it exited ({status}) before \
                 /health answered. Output:\n{}",
                read_lossy(&out_path)
            );
        }
        if health_status(port) == Some(200) {
            booted = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(
        booted,
        "the supervisor binary could not boot hermetically: /health on port {port} did not \
         answer 200 within {}s. Output:\n{}",
        BOOT_BOUND.as_secs(),
        read_lossy(&out_path)
    );

    // Hermeticity, observed from the binary itself: its temp-runner ledger
    // resolved under the tempdir, never under the operator's real data dir.
    let boot_log = read_lossy(&out_path);
    let ledger_line = boot_log
        .lines()
        .find(|l| l.contains("Temp-runner ledger: "))
        .unwrap_or_else(|| {
            panic!("the supervisor logged no `Temp-runner ledger:` line. Output:\n{boot_log}")
        });
    let ledger_path = ledger_line
        .split("Temp-runner ledger: ")
        .nth(1)
        .expect("split on the marker")
        .split_whitespace()
        .next()
        .unwrap_or("");
    assert!(
        Path::new(ledger_path).starts_with(&data),
        "the ledger must resolve under the test's XDG_DATA_HOME ({}), got `{ledger_path}` — a \
         booted test supervisor would otherwise share the live supervisor's ledger",
        data.display()
    );

    // Give the shutdown-signal task a beat to be polled (axum spawns it as
    // `serve` starts), then SIGTERM.
    std::thread::sleep(Duration::from_millis(500));
    // SAFETY: scalar arguments; `pid` is our own live child.
    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    assert_eq!(rc, 0, "SIGTERM could not be delivered to pid {pid}");

    let term_sent = Instant::now();
    let status = loop {
        if let Some(status) = guard.0.as_mut().and_then(|c| c.try_wait().ok()).flatten() {
            break status;
        }
        assert!(
            term_sent.elapsed() < EXIT_BOUND,
            "the supervisor did not exit within {}s of SIGTERM. Output:\n{}",
            EXIT_BOUND.as_secs(),
            read_lossy(&out_path)
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    guard.0 = None;

    let output = read_lossy(&out_path);
    assert!(
        output.contains("Received shutdown signal (sigterm)"),
        "SIGTERM must be handled as a graceful shutdown that names its signal (exit {status}). \
         Output:\n{output}"
    );
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        status.signal(),
        None,
        "the supervisor died ON the signal instead of handling it. Output:\n{output}"
    );
}
