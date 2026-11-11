//! The ONE loopback address the supervisor listens on and dials.
//!
//! The supervisor binds the IPv4 loopback only (`main.rs`: a
//! `socket2::Domain::IPV4` socket on [`LOOPBACK_IP`]). So does every runner it
//! manages, the primary and every temp or named runner alike: qontinui-runner's
//! `mcp_api::try_bind_port` binds `mcp::types::RUNNER_API_BIND_IP`
//! (`Ipv4Addr::LOCALHOST`) through `socket2::Domain::IPV4`. Every URL the
//! supervisor builds for itself or for a runner therefore names that address,
//! never `localhost`. Windows resolves `localhost` to `::1` first, nothing
//! listens there, and each NEW connection pays a doomed IPv6 connect before
//! reaching the socket that answers. The runner measured that cost on
//! 2026-10-25 against a live `GET /health`: about +305 ms for a fresh reqwest
//! client, +205 ms for curl, and +2 s for Python `urllib` and PowerShell 5.1,
//! which have no fallback.
//!
//! The cost is not only the supervisor's own. The `api_url` and `ui_bridge_url`
//! it returns from `POST /runners/spawn-test`, `POST /runners/spawn-named` and
//! the runner lists are the addresses agents are told to follow, so a
//! `localhost` here becomes a `localhost` in every client that follows one.
//!
//! Plan `2026-11-11-supervisor-and-temp-runner-probes-still-dial-localhost`,
//! Phase 2, which follows the runner's own
//! `2026-10-25-runner-self-calls-dial-localhost-and-pay-a-doomed-ipv6-connect`.
//! `tests/supervisor_urls_use_ipv4_loopback.rs` keeps a new call site from
//! spelling `localhost` again.

use std::net::Ipv4Addr;

/// The address the supervisor's listener binds, and the host every URL it
/// builds names.
///
/// It must equal the bind of every runner the supervisor dials:
/// qontinui-runner's `mcp::types::RUNNER_API_BIND_IP`, which is
/// `Ipv4Addr::LOCALHOST`.
pub const LOOPBACK_IP: Ipv4Addr = Ipv4Addr::LOCALHOST;

/// The base URL of the HTTP service listening on [`LOOPBACK_IP`] at `port`,
/// e.g. `"http://127.0.0.1:9875"`. No trailing slash.
///
/// For the supervisor's own address, pass the port it bound
/// (`SupervisorConfig::port`, set by `--port`), never `DEFAULT_SUPERVISOR_PORT`.
pub fn loopback_base(port: u16) -> String {
    format!("http://{LOOPBACK_IP}:{port}")
}

/// The base URL of the runner whose HTTP API listens on `port`, whether the
/// primary or any temp or named runner, e.g. `"http://127.0.0.1:9877"`. No
/// trailing slash.
///
/// qontinui-runner's `mcp::types::runner_loopback_base` has the same name and
/// builds the same text from the same address.
pub fn runner_loopback_base(port: u16) -> String {
    loopback_base(port)
}

/// The UI Bridge base of the runner on `port`, e.g.
/// `"http://127.0.0.1:9877/ui-bridge"`. No trailing slash.
pub fn runner_ui_bridge_base(port: u16) -> String {
    format!("{}/ui-bridge", runner_loopback_base(port))
}

#[cfg(test)]
mod tests {
    use super::{loopback_base, runner_loopback_base, runner_ui_bridge_base, LOOPBACK_IP};

    // Spelled-out literals, not built from the helpers, so a change to the
    // helpers' output fails here instead of agreeing with itself.
    #[test]
    fn every_url_names_the_ipv4_loopback() {
        assert_eq!(LOOPBACK_IP, std::net::Ipv4Addr::new(127, 0, 0, 1));
        assert_eq!(loopback_base(9875), "http://127.0.0.1:9875");
        assert_eq!(runner_loopback_base(9876), "http://127.0.0.1:9876");
        assert_eq!(
            runner_ui_bridge_base(9899),
            "http://127.0.0.1:9899/ui-bridge"
        );
    }
}
