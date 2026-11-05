#![cfg(test)]
//! The port-0 bind scan over in-memory sources: each spelling it must flag, the
//! look-alikes it must not, and what it counts as test code.

use super::*;

/// `(line, item)` of each bind the scan finds in `src`, read as a whole test file.
fn found(src: &str) -> Vec<(usize, String)> {
    found_in(src, true)
}

fn found_in(src: &str, whole_file_is_test: bool) -> Vec<(usize, String)> {
    scan(src, whole_file_is_test)
        .expect("the fixture lexes")
        .binds
        .into_iter()
        .map(|b| (b.line, b.function))
        .collect()
}

#[test]
fn every_spelling_of_a_port_0_bind_is_flagged_once() {
    let spellings = [
        r#"std::net::TcpListener::bind("127.0.0.1:0")"#,
        r#"tokio::net::TcpListener::bind("[::1]:0").await"#,
        r#"TcpListener::bind("0.0.0.0:0")"#,
        r#"TcpListener::bind("localhost:0")"#,
        r#"TcpListener::bind(r"127.0.0.1:0")"#,
        r#"TcpListener::bind(("127.0.0.1", 0))"#,
        r#"TcpListener::bind((Ipv4Addr::LOCALHOST, 0u16))"#,
        r#"TcpListener::bind((Ipv6Addr::LOCALHOST, 0_u16))"#,
        "sock.bind(&std::net::SocketAddr::from(([127, 0, 0, 1], 0)).into())",
        "TcpListener::bind(SocketAddr::new(ip, 0))",
        "TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))",
        "TcpListener::bind(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 0))",
        r#"UdpSocket::bind("127.0.0.1:0")"#,
    ];
    for spelling in spellings {
        let src = format!("fn fixture() {{\n    let s = {spelling};\n}}\n");
        assert_eq!(found(&src), vec![(2, "fixture".to_string())], "{spelling}");
    }
}

#[test]
fn a_bind_that_is_not_to_port_0_is_not_flagged() {
    for spelling in [
        r#"TcpListener::bind("127.0.0.1:9876")"#,
        r#"TcpListener::bind("127.0.0.1:10")"#,
        r#"TcpListener::bind(("127.0.0.1", port))"#,
        r#"TcpListener::bind(("127.0.0.1", 10))"#,
        "sock.bind(&SocketAddr::from(([127, 0, 0, 1], port)).into())",
        "TcpListener::bind(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 9875, 0, 0))",
        "TcpListener::bind(SocketAddr::new(ip, port_of(0)))",
        // An address built outside the call is not followed: the stated limit.
        "TcpListener::bind(addr)",
        // A port-0 literal with no bind around it, as netstat parsing has.
        r#"endpoint_port("0.0.0.0:0")"#,
        r#"format!("http://localhost:0/{}", id)"#,
        "row(1, None, name, exe, 0)",
        r#"b"127.0.0.1:0""#,
    ] {
        let src = format!("fn fixture() {{\n    let s = {spelling};\n}}\n");
        assert_eq!(found(&src), Vec::<(usize, String)>::new(), "{spelling}");
    }
    let defined = "fn bind(addr: &str) -> u16 { 0 }\nfn f() { bind(x) }\n";
    assert_eq!(found(defined), Vec::<(usize, String)>::new());
    let mentioned = r#"
// TcpListener::bind("127.0.0.1:0")
/* sock.bind(("127.0.0.1", 0)) */
fn f() { let s = "TcpListener::bind(\"127.0.0.1:0\")"; }
"#;
    assert_eq!(found(mentioned), Vec::<(usize, String)>::new());
}

/// In a file that is not test code as a whole, only test items are scanned:
/// `#[test]` and `#[tokio::test]` functions and everything under a `cfg` that
/// requires `test`, a module's or an `impl`'s whole body included.
#[test]
fn only_test_items_of_a_production_file_are_scanned() {
    let src = r#"
fn production() { TcpListener::bind("127.0.0.1:0"); }

#[cfg(any(test, feature = "x"))]
fn also_built_without_test() { TcpListener::bind("127.0.0.1:0"); }

#[test]
fn a_test() { TcpListener::bind("127.0.0.1:0"); }

#[tokio::test(flavor = "multi_thread")]
async fn an_async_test() { tokio::net::TcpListener::bind("127.0.0.1:0").await; }

#[cfg(all(test, unix))]
static HELD: Lazy = Lazy::new(|| TcpListener::bind("127.0.0.1:0"));

#[cfg(test)]
mod tests {
    fn helper() { TcpListener::bind("127.0.0.1:0"); }
    impl Fixture {
        pub(crate) async fn serve() { TcpListener::bind("127.0.0.1:0"); }
    }
}

mod production_too {
    fn nested() { TcpListener::bind("127.0.0.1:0"); }
}
"#;
    assert_eq!(
        found_in(src, false),
        vec![
            (8, "a_test".to_string()),
            (11, "an_async_test".to_string()),
            (14, "HELD".to_string()),
            (18, "helper".to_string()),
            (20, "serve".to_string()),
        ]
    );
    // The same file as a whole test file: every bind, production's included.
    assert_eq!(found_in(src, true).len(), 8);
}

/// An inner `#![cfg(test)]` makes the rest of its item list test code.
#[test]
fn an_inner_cfg_test_makes_the_rest_test_code() {
    let src = "#![cfg(test)]\nfn f() { TcpListener::bind(\"127.0.0.1:0\"); }\n";
    assert_eq!(found_in(src, false), vec![(2, "f".to_string())]);
}

/// A file's `mod NAME;` declarations are reported, with whether each is
/// compiled only for tests, so the crate scan can follow them to their files.
#[test]
fn module_declarations_carry_their_test_gate() {
    let src = "mod a;\n#[cfg(test)]\nmod b;\nmod outer {\n    #[cfg(test)]\n    mod c;\n}\n#[cfg(test)]\nmod d {\n    mod e;\n}\n";
    let decls = scan(src, false).expect("lexes").mod_decls;
    let got: Vec<(Vec<&str>, bool)> = decls
        .iter()
        .map(|d| (d.path.iter().map(String::as_str).collect(), d.test))
        .collect();
    assert_eq!(
        got,
        vec![
            (vec!["a"], false),
            (vec!["b"], true),
            (vec!["outer", "c"], true),
            (vec!["d", "e"], true),
        ]
    );
}

/// What the crate scan cannot read is reported as unread, never as a file with
/// no binds: a file that cannot be read (a link to nothing) and a directory that
/// cannot be listed (`tests/`, absent here).
#[cfg(unix)]
#[test]
fn what_the_crate_scan_cannot_read_is_reported_unread() {
    let root = tempfile::tempdir().expect("tempdir");
    let src = root.path().join("src");
    std::fs::create_dir_all(&src).expect("create src");
    std::fs::write(src.join("lib.rs"), "fn f() {}\n").expect("write lib.rs");
    std::os::unix::fs::symlink(root.path().join("absent.rs"), src.join("gone.rs"))
        .expect("link gone.rs to nothing");
    let scans = scan_crate(root.path());
    assert_eq!(
        scans.get("src/lib.rs"),
        Some(&Some(Vec::new())),
        "a file read and scanned: {scans:?}"
    );
    assert_eq!(
        scans.get("src/gone.rs"),
        Some(&None),
        "a file that cannot be read: {scans:?}"
    );
    assert_eq!(
        scans.get("tests"),
        Some(&None),
        "a directory that cannot be listed: {scans:?}"
    );
}

#[test]
fn a_file_that_does_not_lex_or_balance_is_none() {
    assert_eq!(scan("fn f() { let s = \"open; }", true), None);
    assert_eq!(scan("fn f() { (] }", true), None);
}

#[test]
fn a_declaration_names_both_file_layouts() {
    let lib = Path::new("/c/src/lib.rs");
    assert_eq!(
        declared_files(lib, &["a".to_string()]),
        [
            PathBuf::from("/c/src/a.rs"),
            PathBuf::from("/c/src/a/mod.rs")
        ]
    );
    let nested = Path::new("/c/src/process/manager.rs");
    assert_eq!(
        declared_files(nested, &["inline".to_string(), "tests".to_string()]),
        [
            PathBuf::from("/c/src/process/manager/inline/tests.rs"),
            PathBuf::from("/c/src/process/manager/inline/tests/mod.rs")
        ]
    );
}
