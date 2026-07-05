use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::time::{sleep, timeout};
use tracing::debug;

use crate::config::{PORT_CHECK_INTERVAL_MS, PORT_WAIT_TIMEOUT_SECS};

/// Check whether a live socket (a LISTENer or another bound socket) holds
/// `127.0.0.1:<port>`, by attempting to **bind** it.
///
/// Bind-based on purpose — this probe is authoritative for LISTEN state:
///
/// * On Windows a default bind (no `SO_REUSEADDR`) **succeeds** against
///   TIME_WAIT remnants of a just-killed process and fails with
///   `WSAEADDRINUSE` only when a real listener/bound socket holds the port.
/// * The previous implementation did a non-blocking **connect** and treated
///   `WSAEWOULDBLOCK (10035)` as "something is listening". On Windows a
///   non-blocking connect reports refusal *asynchronously*, so 10035 in the
///   synchronous window means only "attempt in progress" — TIME_WAIT
///   remnants read as occupancy. That false positive made the stop-reap in
///   `stop_runner_by_id` refuse to confirm a completed stop (live incident
///   2026-07-03 17:17Z: port 9876 "still in use" after PID-kill + tree-kill
///   + kill-by-port while nothing was listening).
///
/// The probe socket is bound but never listened/connected, so dropping it
/// creates no TIME_WAIT state of its own.
///
/// For "is a runner actually serving HTTP?" use [`is_runner_responding`] —
/// that is an application-level question, not a port-occupancy one.
pub fn is_port_listening(port: u16) -> bool {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let socket = match socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    ) {
        Ok(s) => s,
        // Couldn't even create a socket — we cannot claim the port is held.
        Err(_) => return false,
    };

    // Deliberately no SO_REUSEADDR: the default-bind semantics above are
    // exactly what makes this probe ignore TIME_WAIT but fail against a
    // live listener. Any bind error (WSAEADDRINUSE, or WSAEACCES when the
    // holder bound exclusively) means a live socket owns the port.
    socket.bind(&addr.into()).is_err()
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
            if is_port_listening(port) {
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
            if !is_port_listening(port) {
                debug!("Port {} is now free", port);
                return true;
            }
            sleep(interval).await;
        }
    })
    .await;

    result.unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};

    /// Bind an ephemeral port and return (listener, port).
    fn ephemeral_listener() -> (TcpListener, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
        let port = listener.local_addr().expect("local_addr").port();
        (listener, port)
    }

    #[test]
    fn test_real_listener_reads_as_listening() {
        let (_listener, port) = ephemeral_listener();
        assert!(
            is_port_listening(port),
            "a live TcpListener on port {} must read as listening",
            port
        );
    }

    #[test]
    fn test_free_port_reads_as_free() {
        // Grab an ephemeral port, then release it (no connection was ever
        // made, so no TIME_WAIT state exists) — the port must read free.
        let (listener, port) = ephemeral_listener();
        drop(listener);
        assert!(
            !is_port_listening(port),
            "a freshly-released port {} with no connections must read as free",
            port
        );
    }

    /// Regression for the 2026-07-03 17:17Z stop-confirmation wedge: a
    /// just-killed process leaves TIME_WAIT remnants on its port. The old
    /// connect-based probe latched onto them and reported the port in use;
    /// the bind-based probe must report it free.
    #[test]
    fn test_time_wait_remnant_reads_as_free() {
        let (listener, port) = ephemeral_listener();

        // Create a real connection so closing the server side leaves a
        // TIME_WAIT entry on the server's port (the side that closes first
        // enters TIME_WAIT).
        let mut client = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let (server_sock, _) = listener.accept().expect("accept");

        // Server closes first: drop the accepted socket and the listener.
        drop(server_sock);
        drop(listener);

        // Nudge the client so the close handshake completes, then close it.
        let _ = client.write_all(b"x");
        drop(client);

        // Give the stack a moment to transition the server side to TIME_WAIT.
        std::thread::sleep(std::time::Duration::from_millis(200));

        assert!(
            !is_port_listening(port),
            "TIME_WAIT remnants on port {} must NOT read as listening",
            port
        );
    }
}
