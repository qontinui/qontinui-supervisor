use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::time::{sleep, timeout};
use tracing::debug;

use crate::config::{PORT_CHECK_INTERVAL_MS, PORT_WAIT_TIMEOUT_SECS};

/// Check if a TCP port is currently in use by attempting to bind to it.
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

    // Try to connect — if it succeeds, something is listening
    socket.set_nonblocking(true).ok();
    match socket.connect(&addr.into()) {
        Ok(()) => true,
        Err(e) => {
            // On Windows, WSAEWOULDBLOCK (10035) means connection is in progress
            // which means something is listening
            let code = e.raw_os_error().unwrap_or(0);
            code == 10035 || code == 10056 // WSAEWOULDBLOCK or WSAEISCONN
        }
    }
}

/// Check if the runner HTTP API is responding at the given port.
pub async fn is_runner_responding(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{}/health", port);
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
