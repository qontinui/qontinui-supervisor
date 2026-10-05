//! Test-only: a loopback port that is guaranteed to refuse connections.
//!
//! Binding a listener and DROPPING it frees the port, and a concurrently
//! running test's mock server can bind it and answer (coord finding
//! `2d01deb3`, the `flywheel` flake). A socket that is bound but never
//! `listen()`ed refuses connects, and keeping it alive keeps the port taken.

/// Reserve a refused loopback port for the rest of the test process. The
/// socket is deliberately leaked: callers get a bare port number with no
/// lifetime to hold, and the process exits when the suite does.
pub fn reserve_refused_port() -> u16 {
    let sock = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )
    .expect("create socket");
    sock.bind(&std::net::SocketAddr::from(([127, 0, 0, 1], 0)).into())
        .expect("bind ephemeral port");
    let port = sock
        .local_addr()
        .expect("local addr")
        .as_socket()
        .expect("an IPv4 bind yields an IP socket address")
        .port();
    std::mem::forget(sock);
    port
}
