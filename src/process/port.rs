use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::time::{sleep, timeout};
use tracing::debug;

use crate::config::{PORT_CHECK_INTERVAL_MS, PORT_WAIT_TIMEOUT_SECS};

/// Short bound on the synchronous connect probe in [`is_port_in_use`]. A
/// localhost connect either completes or is refused effectively instantly;
/// the timeout only guards a pathological backlog-full listener.
const PORT_PROBE_TIMEOUT_MS: u64 = 250;

/// Check if a TCP port is currently in use (something LISTENING on
/// `127.0.0.1:port`).
///
/// Uses a **blocking** connect with a short timeout via
/// [`socket2::Socket::connect_timeout`]. The previous implementation used a
/// nonblocking connect and only recognised the Windows "in progress" error
/// codes (10035/10056) as "listening" — on Unix a nonblocking localhost
/// connect returns `EINPROGRESS` (errno 36) regardless of whether anything is
/// listening, so the function **always returned `false` on macOS/Linux**.
/// That silently broke every caller that gates on it: the stop path's
/// port-free confirmation, the reaper's crash detection, and the reconcile
/// sweep's orphan detection (the D7 orphan-leak surface). A synchronous
/// connect is unambiguous on all platforms: it succeeds when a listener
/// accepts, and is refused (`ECONNREFUSED`) when the port is free.
pub fn is_port_in_use(port: u16) -> bool {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let socket = match socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    ) {
        Ok(s) => s,
        Err(_) => return false,
    };

    // Blocking connect with a short timeout. Ok => a listener accepted =>
    // port in use. Err (connection refused / timeout) => nothing listening.
    socket
        .connect_timeout(&addr.into(), Duration::from_millis(PORT_PROBE_TIMEOUT_MS))
        .is_ok()
}

/// Check if an HTTP health endpoint is responding at the given port and path.
pub async fn check_http_health(port: u16, path: &str) -> bool {
    let url = format!("http://127.0.0.1:{}{}", port, path);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build();

    let client = match client {
        Ok(c) => c,
        Err(_) => return false,
    };

    match client.get(&url).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

/// Check if the runner HTTP API is responding at the given port.
pub async fn is_runner_responding(port: u16) -> bool {
    check_http_health(port, "/health").await
}

/// Wait for a port to become available (something starts listening).
/// Returns true if the port became available within the timeout.
#[allow(dead_code)]
pub async fn wait_for_port(port: u16, timeout_secs: u64) -> bool {
    let deadline = Duration::from_secs(timeout_secs);
    let interval = Duration::from_millis(PORT_CHECK_INTERVAL_MS);

    let result = timeout(deadline, async {
        loop {
            if is_port_in_use(port) {
                debug!("Port {} is now in use", port);
                return true;
            }
            sleep(interval).await;
        }
    })
    .await;

    result.unwrap_or(false)
}

/// Wait for the runner API to respond to health checks.
#[allow(dead_code)]
pub async fn wait_for_runner_api(port: u16) -> bool {
    let deadline = Duration::from_secs(PORT_WAIT_TIMEOUT_SECS);
    let interval = Duration::from_millis(PORT_CHECK_INTERVAL_MS);

    let result = timeout(deadline, async {
        loop {
            if is_runner_responding(port).await {
                debug!("Runner API on port {} is responding", port);
                return true;
            }
            sleep(interval).await;
        }
    })
    .await;

    result.unwrap_or(false)
}

/// Wait for a port to become free (nothing listening).
pub async fn wait_for_port_free(port: u16, timeout_secs: u64) -> bool {
    let deadline = Duration::from_secs(timeout_secs);
    let interval = Duration::from_millis(PORT_CHECK_INTERVAL_MS);

    let result = timeout(deadline, async {
        loop {
            if !is_port_in_use(port) {
                debug!("Port {} is now free", port);
                return true;
            }
            sleep(interval).await;
        }
    })
    .await;

    result.unwrap_or(false)
}
