use super::*;

/// The releases a fixture file holds, all of it test code.
fn releases(src: &str) -> Vec<Release> {
    scan(src, true).expect("the fixture lexes").releases
}

/// The fixture's one release, by function name and shape.
fn only_release(src: &str) -> (String, Shape) {
    let found = releases(src);
    assert_eq!(
        found.len(),
        1,
        "expected exactly one release: {found:?}\n{src}"
    );
    (found[0].function.clone(), found[0].shape)
}

fn used_after_release(shape: Shape) -> bool {
    matches!(shape, Shape::UsedAfterRelease { .. })
}

/// Every release shape Phase 2 removed from the tree, as it stood at
/// qontinui-supervisor 4c075d2 (two helpers renamed, so no fixture here spells a
/// deleted helper). Each is a release, so each must be flagged.
#[test]
fn every_release_shape_the_migration_removed_is_flagged() {
    let shapes: [(&str, &str, bool); 7] = [
        (
            "routes_free_port",
            r#"
fn routes_free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = l.local_addr().expect("local addr").port();
    drop(l);
    port
}"#,
            true,
        ),
        (
            "a_stopped_spawns_early_log_outlives_the_longest_window",
            r#"
async fn a_stopped_spawns_early_log_outlives_the_longest_window() {
    let (state, root) = state_over_an_empty_runner_tree();
    let id = "named-early-log-kept";
    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    };
    let config = crate::config::RunnerConfig { id: id.to_string(), port, ..Default::default() };
}"#,
            false,
        ),
        (
            "port_held_guard_does_not_refuse_a_non_runner_listener",
            r#"
async fn port_held_guard_does_not_refuse_a_non_runner_listener() {
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = dead.local_addr().unwrap().port();
    drop(dead);
    assert!(refuse_if_port_held_by_live_runner(dead_port).await.is_ok());
}"#,
            true,
        ),
        (
            "register_dead_named_runner",
            r#"
async fn register_dead_named_runner(state: &SharedState, id: &str) -> Arc<ManagedRunner> {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    register_named_runner_on_port(state, id, port).await
}"#,
            true,
        ),
        (
            "register_dead_primary",
            r#"
async fn register_dead_primary(state: &SharedState) -> Arc<ManagedRunner> {
    let mut config = crate::config::RunnerConfig::default_primary();
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    config.port = probe.local_addr().unwrap().port();
    drop(probe);
    let id = config.id.clone();
    let managed = Arc::new(ManagedRunner::new(config, false));
    state.runners.write().await.insert(id, managed.clone());
    managed
}"#,
            true,
        ),
        (
            "health_probe_silent_port",
            r#"
    fn health_probe_silent_port() -> u16 {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    }"#,
            false,
        ),
        (
            "no_answer_from_coord_has_no_basis",
            r#"
    async fn no_answer_from_coord_has_no_basis() {
        let refused = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let base = format!("http://{}", listener.local_addr().expect("addr"));
            drop(listener);
            base
        };
        let fetched = fetch_expectations(&client, &refused, &[]).await;
    }"#,
            true,
        ),
    ];
    for (function, src, after_release) in shapes {
        let (found, shape) = only_release(src);
        assert_eq!(found, function);
        assert_eq!(
            used_after_release(shape),
            after_release,
            "{function}: {shape:?}"
        );
    }
}

/// Spellings of a release the migration did not have, so the scan is shown to
/// follow the data rather than match the seven sites above.
#[test]
fn a_release_is_found_by_data_flow_in_any_spelling() {
    let cases: [(&str, &str); 10] = [
        (
            "a path-qualified drop",
            r#"fn t() {
                let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                let port = l.local_addr().unwrap().port();
                std::mem::drop(l);
                register(port);
            }"#,
        ),
        (
            "a temporary read in the same chain",
            r#"fn t() {
                let port = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
                register(port);
            }"#,
        ),
        (
            "a wildcard dropped at once",
            r#"fn ephemeral_listener() -> (TcpListener, u16) {
                let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
                let port = listener.local_addr().expect("addr").port();
                (listener, port)
            }
            fn t() {
                let (_, port) = ephemeral_listener();
                register(port);
            }"#,
        ),
        (
            "a pair helper's holder dropped by its caller",
            r#"async fn ephemeral() -> (tokio::net::TcpListener, u16) {
                let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let p = l.local_addr().unwrap().port();
                (l, p)
            }
            async fn t() {
                let (l, p) = ephemeral().await;
                drop(l);
                register(p).await;
            }"#,
        ),
        (
            "a use inside a format string",
            r#"fn t() {
                let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                let port = l.local_addr().unwrap().port();
                drop(l);
                let url = format!("http://127.0.0.1:{port}/health");
            }"#,
        ),
        (
            "a port returned from a function",
            r#"fn t() -> u16 {
                let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                let port = l.local_addr().unwrap().port();
                return port;
            }"#,
        ),
        (
            "a socket2 socket bound by method",
            r#"fn t() {
                let sock = socket2::Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
                sock.bind(&std::net::SocketAddr::from(([127, 0, 0, 1], 0)).into()).unwrap();
                let port = sock.local_addr().unwrap().as_socket().unwrap().port();
                drop(sock);
                register(port);
            }"#,
        ),
        (
            "an address bound through a variable",
            r#"fn t() {
                let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
                let l = std::net::TcpListener::bind(addr).unwrap();
                let p = l.local_addr().unwrap().port();
                drop(l);
                register(p);
            }"#,
        ),
        (
            "a drop on a branch that continues",
            r#"fn t(close: bool) {
                let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                let p = l.local_addr().unwrap().port();
                if close {
                    drop(l);
                }
                register(p);
            }"#,
        ),
        (
            "a holder that dies with its inner block",
            r#"fn t() {
                let port;
                {
                    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                    port = l.local_addr().unwrap().port();
                }
                register(port);
            }"#,
        ),
    ];
    for (case, src) in cases {
        let found = releases(src);
        assert_eq!(found.len(), 1, "{case}: {found:?}");
    }
}

/// A listener held past the port's last use, however it is held, is not a release.
#[test]
fn a_held_listener_is_not_a_release() {
    let cases: [(&str, &str); 10] = [
        (
            "dropped after the last use",
            r#"fn t() {
                let live = std::net::TcpListener::bind("127.0.0.1:0").expect("bind live");
                let live_port = live.local_addr().expect("addr").port();
                assert!(probe(live_port));
                drop(live);
            }"#,
        ),
        (
            "moved into a served task",
            r#"async fn t() -> String {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
                format!("http://127.0.0.1:{}", addr.port())
            }"#,
        ),
        (
            "handed out with its port",
            r#"fn ephemeral_listener() -> (std::net::TcpListener, u16) {
                let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
                let port = listener.local_addr().expect("local_addr").port();
                (listener, port)
            }
            fn t() {
                let (_listener, port) = ephemeral_listener();
                assert!(probe(port));
            }"#,
        ),
        (
            "leaked on purpose",
            r#"pub fn reserve_refused_port() -> u16 {
                let sock = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).expect("create socket");
                sock.bind(&std::net::SocketAddr::from(([127, 0, 0, 1], 0)).into()).expect("bind");
                let port = sock.local_addr().expect("local addr").as_socket().expect("ip").port();
                std::mem::forget(sock);
                port
            }"#,
        ),
        (
            "rebound by an adapter and served",
            r#"async fn serve() -> CountingListener {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
                let base = format!("http://{}", listener.local_addr().expect("local addr"));
                let listener = listener.tap_io(move |_| { counter.fetch_add(1, Ordering::SeqCst); });
                tokio::spawn(async move { axum::serve(listener, app).await });
                CountingListener { base, connections }
            }"#,
        ),
        (
            "dropped on a branch that returns",
            r#"fn t(close: bool) {
                let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                let p = l.local_addr().unwrap().port();
                if close {
                    drop(l);
                    return;
                }
                register(p);
            }"#,
        ),
        (
            "a later binding of the same name",
            r#"fn t() {
                let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                let port = l.local_addr().unwrap().port();
                assert!(probe(port));
                drop(l);
                let port = 7;
                register(port);
            }"#,
        ),
        (
            "an accept loop moved into a task",
            r#"async fn t() {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let port = listener.local_addr().unwrap().port();
                tokio::spawn(async move {
                    let mut held = Vec::new();
                    while let Ok((sock, _)) = listener.accept().await {
                        held.push(sock);
                    }
                });
                let base = format!("http://127.0.0.1:{}", port);
                call(&base).await;
            }"#,
        ),
        (
            "held across a whole loop body",
            r#"fn t() {
                for _ in 0..3 {
                    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                    let p = l.local_addr().unwrap().port();
                    register(p);
                }
            }"#,
        ),
        (
            "a port passed to a stub that is served on it",
            r#"async fn t() {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let (state, managed, _root) = runner_on(listener.local_addr().unwrap().port()).await;
                tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
                poll(&state, &managed).await;
            }"#,
        ),
    ];
    for (case, src) in cases {
        let scanned = scan(src, true).expect("lexes");
        assert!(
            scanned.releases.is_empty(),
            "{case}: {:?}",
            scanned.releases
        );
        assert!(
            scanned.binds >= 1,
            "{case}: the scan followed no bind, so it proves nothing"
        );
    }
}

/// Production code is out of scope unless the file or an enclosing item is test code.
#[test]
fn only_test_code_is_scanned() {
    let release = r#"
        fn released() -> u16 {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = l.local_addr().unwrap().port();
            drop(l);
            port
        }"#;
    assert!(scan(release, false).expect("lexes").releases.is_empty());
    for wrapped in [
        format!("#[cfg(test)]\nmod tests {{\n{release}\n}}"),
        format!("#[cfg(all(test, unix))]\nmod tests {{\n{release}\n}}"),
        format!("#[cfg(test)]\n{release}"),
        format!("#[tokio::test(flavor = \"multi_thread\")]\nasync fn t() {{ {release} }}"),
        format!("#![cfg(test)]\n{release}"),
    ] {
        assert_eq!(
            scan(&wrapped, false).expect("lexes").releases.len(),
            1,
            "{wrapped}"
        );
    }
    let not_test = format!("#[cfg(any(test, feature = \"x\"))]\nmod tests {{\n{release}\n}}");
    assert!(scan(&not_test, false).expect("lexes").releases.is_empty());
}
