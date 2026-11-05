//! Test-only loopback ports: the one place test code binds port 0, and one
//! fixture per kind of consumer.
//!
//! A port a test lets go of is no longer the test's. Binding `127.0.0.1:0`,
//! reading the port and dropping the socket hands the port back to the kernel,
//! and any `bind(0)` or `connect()` on the box can be given it before the code
//! under test looks (coord finding `2d01deb3`, the `flywheel` flake; finding
//! `ed257f86`, the purge flake). So a kernel-picked port leaves this module only
//! together with the socket that holds it, and
//! [`tests::no_test_code_binds_port_0_outside_test_ports`] fails test code that
//! binds port 0 anywhere else. Pick by what the test does with the port:
//!
//! * It SERVES or listens on it: [`held_ephemeral_listener`], or
//!   [`held_ephemeral_tokio_listener`] to serve it with tokio. The port is the
//!   test's while the listener lives, and a server that shuts down drops its
//!   listener. So when the code under test still probes or kills the port after
//!   the test's server stops, the test listens on [`unheld_port`] instead.
//! * The code under test only CONNECTS to a port nothing answers:
//!   [`reserve_refused_port`]. Bound, never listened on, and kept, so a connect
//!   is refused and nothing else can listen there.
//! * The code under test BIND-PROBES a port nothing holds
//!   ([`crate::process::port::is_port_listening`], a stop's
//!   `wait_for_port_free`, the purge's crash arm), or it can reach a kill of
//!   whatever holds the port (the stop ladder, the purge's Windows
//!   `kill_by_port`): [`unheld_port`]. Nothing holds it, and no kernel
//!   allocation hands it out.
//!
//! Plan `2026-11-05-supervisor-test-flakes-dead-ports-etxtbsy`, Phase 2.

mod bind_scan;
mod ephemeral_ranges;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

/// Reserve a refused loopback port for the rest of the test process. The
/// socket is deliberately leaked: callers get a bare port number with no
/// lifetime to hold, and the process exits when the suite does.
///
/// **For a consumer that only connects.** A socket that is bound but never
/// listened on refuses connects, and keeping it keeps the port taken. It is NOT
/// for code whose consumer bind-probes the port: the held socket reads HELD to a
/// bind probe on every OS (on Unix the probe's `SO_REUSEADDR` bind conflicts with
/// a holder that lacks it, and on Windows a default bind conflicts with any bound
/// socket), so a probe that asks "is the runner gone?" is told it is alive, and a
/// stop's confirmation never comes. Use [`unheld_port`] there. A purge of a
/// placeholder that is not running never bind-probes its port, so it may keep
/// this one.
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

/// A loopback listener on a port the kernel picked, and that port, for a test
/// that listens or serves on it.
///
/// The port is the test's for as long as it holds the listener: dropping the
/// listener hands the port back to the kernel, and any `bind(0)` or `connect()`
/// on the box can be given it. So hold it past the port's last use. A test that
/// wants a port NOTHING listens on takes [`reserve_refused_port`] or
/// [`unheld_port`]; only a test whose subject is a released port drops this
/// one on purpose (`process::port`'s probe tests).
pub fn held_ephemeral_listener() -> (std::net::TcpListener, u16) {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral loopback port");
    let port = listener.local_addr().expect("local addr").port();
    (listener, port)
}

/// [`held_ephemeral_listener`] as a tokio listener, for a test that serves it
/// (`axum::serve`, an accept loop). The same rule: the port is the test's while
/// the listener lives, and a server keeps it only while it serves. A server that
/// stops drops its listener (axum's graceful shutdown drops it as soon as the
/// shutdown is signalled), so a test whose code under test still reads the port
/// once the server has stopped (a stop's port probe, a kill of the port's
/// listeners) binds [`unheld_port`] itself instead.
pub async fn held_ephemeral_tokio_listener() -> (tokio::net::TcpListener, u16) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral loopback port");
    let port = listener.local_addr().expect("local addr").port();
    (listener, port)
}

/// Slot `i` hands out the fixture port `FIXTURE_BLOCK + i` and is claimed by
/// binding the claim port `CLAIM_BLOCK + i`, for `i` in `0..SLOTS`. Both blocks
/// lie below Linux's default ephemeral range (32768-60999, read on nilan) and
/// below the IANA dynamic range (49152-65535, RFC 6335), which Windows uses by
/// default (Microsoft KB 929851). They are clear of the temp-runner ports
/// 9875-9899. The defaults are not relied on: [`unheld_port`] reads each box's
/// own ranges and refuses a box where they reach a block.
const FIXTURE_BLOCK: u16 = 27_000;
const CLAIM_BLOCK: u16 = 28_000;
const SLOTS: u16 = 1_000;

/// The next slot this process tries. Slots are never reused: a claimed slot stays
/// claimed until the process exits.
static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);

/// What this process's [`unheld_port`] calls met, for the message when the
/// slots run out.
static TALLY: Tally = Tally::new();

/// Whether the blocks lie outside every port range this box hands out or
/// reserves, decided once per process.
static BLOCKS_CLEAR: OnceLock<Result<(), String>> = OnceLock::new();

/// A loopback port that no socket holds and no kernel allocation hands out, for
/// the rest of the test process. Distinct on every call, and across every test
/// process on the box.
///
/// **For a consumer that bind-probes the port, or that can kill what holds it.**
/// The port reads free to [`crate::process::port::is_port_listening`] on every
/// OS, so "the runner is gone" is true and a stop confirms at once; and since
/// nothing holds it, a stop ladder or a purge that kills a port's listeners finds
/// none. A test may listen on it itself.
///
/// How it stays unheld without being held:
///
/// * The port is below every ephemeral range, so the kernel never hands it to a
///   `bind(0)` or a `connect()`. That is checked once per process against the
///   box's own ranges (Linux `ip_local_port_range`; Windows' dynamic and
///   excluded port ranges; macOS `net.inet.ip.portrange`), and a box whose
///   ranges reach a block panics here, naming them, rather than hand out a port
///   the guarantee does not cover.
/// * Uniqueness is the OS's, not a registry's: a slot is taken by binding its
///   claim port without `SO_REUSEADDR` and never listening on it, so a second
///   process (another test binary, a concurrent suite, a nextest process per
///   test) cannot take the same slot. The claim is kept, held or not, so no
///   other process retries a slot; a slot whose fixture port some socket holds
///   is skipped.
///
/// What it cannot stop: a foreign process that binds a fixture port explicitly
/// after it was handed out (on Windows, one that sets `SO_REUSEADDR` can even
/// share a claim port). Nothing in the fleet does either.
///
/// # Panics
///
/// When this box's port ranges reach into a block, or when every slot is
/// claimed or held. The message says which, and names both blocks.
pub fn unheld_port() -> u16 {
    if let Err(why) = BLOCKS_CLEAR.get_or_init(blocks_clear_on_this_box) {
        panic!("{why}");
    }
    let slots = std::iter::from_fn(|| {
        let slot = NEXT_SLOT.fetch_add(1, Ordering::Relaxed);
        let slot = u16::try_from(slot).ok().filter(|s| *s < SLOTS)?;
        Some((FIXTURE_BLOCK + slot, CLAIM_BLOCK + slot))
    });
    match first_unheld(slots, &TALLY) {
        Some(port) => port,
        None => panic!(
            "test_ports::unheld_port: all {SLOTS} slots are used (fixture ports {}-{}, claim \
             ports {}-{}): this process handed out {}, {} were claimed by another process, and \
             {} fixture ports were held by a socket",
            FIXTURE_BLOCK,
            FIXTURE_BLOCK + SLOTS - 1,
            CLAIM_BLOCK,
            CLAIM_BLOCK + SLOTS - 1,
            TALLY.handed_out.load(Ordering::Relaxed),
            TALLY.claimed_elsewhere.load(Ordering::Relaxed),
            TALLY.held.load(Ordering::Relaxed),
        ),
    }
}

/// How the slots [`first_unheld`] tried turned out.
struct Tally {
    handed_out: AtomicUsize,
    claimed_elsewhere: AtomicUsize,
    held: AtomicUsize,
}

impl Tally {
    const fn new() -> Self {
        Tally {
            handed_out: AtomicUsize::new(0),
            claimed_elsewhere: AtomicUsize::new(0),
            held: AtomicUsize::new(0),
        }
    }
}

/// The first `(fixture, claim)` slot whose claim this process can take and whose
/// fixture port no socket holds. A taken claim is kept whether or not the slot
/// is handed out.
fn first_unheld(slots: impl Iterator<Item = (u16, u16)>, tally: &Tally) -> Option<u16> {
    for (fixture, claim) in slots {
        let Some(claim_socket) = claim_slot(claim) else {
            tally.claimed_elsewhere.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        std::mem::forget(claim_socket);
        if crate::process::port::is_port_listening(fixture) {
            tally.held.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        tally.handed_out.fetch_add(1, Ordering::Relaxed);
        return Some(fixture);
    }
    None
}

/// Bind `127.0.0.1:port` without `SO_REUSEADDR` and never listen: the slot's
/// claim. `None` when some socket already holds the port.
fn claim_slot(port: u16) -> Option<socket2::Socket> {
    let sock = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )
    .ok()?;
    sock.bind(&std::net::SocketAddr::from(([127, 0, 0, 1], port)).into())
        .ok()?;
    Some(sock)
}

/// Read this box's ranges, say what was read on stderr (once per process, so the
/// reading reaches every lane's log), and check both blocks against them.
fn blocks_clear_on_this_box() -> Result<(), String> {
    let ranges = ephemeral_ranges::read().map_err(|why| {
        format!(
            "test_ports::unheld_port: could not read this box's ephemeral port ranges ({why}), \
             so it cannot show that no kernel allocation hands out the fixture ports"
        )
    })?;
    let verdict = blocks_clear(&ranges);
    let read: Vec<String> = ranges.iter().map(ToString::to_string).collect();
    let line = format!(
        "test_ports: port ranges read on this box: {}; fixture block {}-{}, claim block {}-{}: {}\n",
        read.join(", "),
        FIXTURE_BLOCK,
        FIXTURE_BLOCK + SLOTS - 1,
        CLAIM_BLOCK,
        CLAIM_BLOCK + SLOTS - 1,
        if verdict.is_ok() { "clear" } else { "OVERLAP" },
    );
    // Written to the process's stderr rather than through `eprintln!`, which a
    // test's output capture would swallow on a pass.
    let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
    verdict
}

/// Whether the fixture and claim blocks lie outside every one of `ranges`.
fn blocks_clear(ranges: &[ephemeral_ranges::PortRange]) -> Result<(), String> {
    let (low, high) = (FIXTURE_BLOCK, CLAIM_BLOCK + SLOTS - 1);
    match ranges.iter().find(|r| r.first <= high && r.last >= low) {
        None => Ok(()),
        Some(range) => Err(format!(
            "test_ports::unheld_port: this box's {range} reaches into the fixture block {}-{} or \
             the claim block {}-{}, so a kernel allocation can hand those ports out here and a \
             port the fixture hands out is not guaranteed unheld. Narrow the range, or move \
             the blocks below it.",
            FIXTURE_BLOCK,
            FIXTURE_BLOCK + SLOTS - 1,
            CLAIM_BLOCK,
            CLAIM_BLOCK + SLOTS - 1,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ephemeral_ranges::PortRange;
    use std::collections::BTreeMap;
    use std::path::Path;

    #[test]
    fn an_unheld_port_reads_free_and_can_be_listened_on() {
        let port = unheld_port();
        assert!(
            (FIXTURE_BLOCK..FIXTURE_BLOCK + SLOTS).contains(&port),
            "{port} is outside the fixture block"
        );
        assert!(
            !crate::process::port::is_port_listening(port),
            "the bind probe must read an unheld port free"
        );
        let listener = std::net::TcpListener::bind(("127.0.0.1", port))
            .expect("a test can listen on its unheld port");
        assert!(crate::process::port::is_port_listening(port));
        drop(listener);
    }

    /// Two calls never share a slot, and each slot's claim stays taken, which is
    /// what keeps another process off it.
    #[test]
    fn unheld_ports_are_distinct_and_their_claims_stay_taken() {
        let ports: Vec<u16> = (0..4).map(|_| unheld_port()).collect();
        let mut distinct = ports.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(
            distinct.len(),
            ports.len(),
            "a slot was handed out twice: {ports:?}"
        );
        for port in ports {
            let claim = CLAIM_BLOCK + (port - FIXTURE_BLOCK);
            assert!(
                claim_slot(claim).is_none(),
                "the claim on {claim} for {port} was let go"
            );
        }
    }

    /// The two ways a slot is skipped, and a kept claim, over slots built from
    /// ports this process owns (six unheld ports, used here as three slots'
    /// fixture and claim ports), so nothing else on the box can disturb them.
    #[test]
    fn a_claimed_or_held_slot_is_skipped_and_a_kept_claim_is_never_reused() {
        let owned: Vec<u16> = (0..6).map(|_| unheld_port()).collect();
        let slots = [
            (owned[0], owned[1]),
            (owned[2], owned[3]),
            (owned[4], owned[5]),
        ];
        let _listener = std::net::TcpListener::bind(("127.0.0.1", owned[0]))
            .expect("hold the first slot's fixture port");
        let _claim = claim_slot(owned[3]).expect("take the second slot's claim first");
        let tally = Tally::new();

        assert_eq!(first_unheld(slots.into_iter(), &tally), Some(owned[4]));
        assert_eq!(
            tally.held.load(Ordering::Relaxed),
            1,
            "the held fixture port"
        );
        assert_eq!(
            tally.claimed_elsewhere.load(Ordering::Relaxed),
            1,
            "the slot whose claim was taken"
        );
        assert_eq!(tally.handed_out.load(Ordering::Relaxed), 1);

        // Every claim the first pass took is still held, so a second pass over
        // the same slots hands nothing out.
        assert_eq!(first_unheld(slots.into_iter(), &tally), None);
        assert_eq!(tally.claimed_elsewhere.load(Ordering::Relaxed), 4);
        assert_eq!(tally.handed_out.load(Ordering::Relaxed), 1);
    }

    /// The verdict every [`unheld_port`] call in this process reads.
    #[test]
    fn the_blocks_lie_outside_this_boxs_port_ranges() {
        if let Err(why) = BLOCKS_CLEAR.get_or_init(blocks_clear_on_this_box) {
            panic!("{why}");
        }
    }

    #[test]
    fn a_range_reaching_a_block_is_refused_and_one_beside_them_is_not() {
        let range = |first, last| PortRange {
            first,
            last,
            source: "test range",
        };
        assert!(blocks_clear(&[range(32_768, 60_999), range(49_152, 65_535)]).is_ok());
        assert!(blocks_clear(&[range(29_000, 65_535), range(1, 26_999)]).is_ok());
        for overlapping in [
            range(20_000, 27_000),
            range(28_999, 40_000),
            range(27_500, 27_600),
            range(10_000, 60_000),
        ] {
            let why = blocks_clear(&[range(49_152, 65_535), overlapping.clone()])
                .expect_err("an overlapping range");
            assert!(why.contains(&overlapping.to_string()), "{why}");
            assert!(
                why.contains("27000-27999") && why.contains("28000-28999"),
                "{why}"
            );
        }
    }

    /// Test code binds port 0 here and nowhere else (plan
    /// `2026-11-05-supervisor-test-flakes-dead-ports-etxtbsy`, Phase 2). A port
    /// the kernel picked is the test's only while the socket that holds it
    /// lives, and the helpers here hand one out only with that socket. A bind
    /// elsewhere is how test code came to fabricate a "dead" port by letting
    /// the socket go: a bind probe can then read the port held, and a stop or a
    /// purge can kill whatever took it (the purge flake, finding `ed257f86`).
    ///
    /// It covers all test code under `src/` and `tests/` ([`bind_scan`] says
    /// what a port-0 bind and test code are), and it proves it is reading the
    /// tree: each of this module's own three binds, one per spelling the
    /// helpers use, must be found where it is.
    #[test]
    fn no_test_code_binds_port_0_outside_test_ports() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let scans = bind_scan::scan_crate(root);
        let mut unread = Vec::new();
        let mut outside = Vec::new();
        let mut own: BTreeMap<String, usize> = BTreeMap::new();
        for (path, binds) in &scans {
            let Some(binds) = binds else {
                unread.push(path.clone());
                continue;
            };
            let in_test_ports = path == "src/test_ports.rs" || path.starts_with("src/test_ports/");
            for bind in binds {
                if in_test_ports {
                    *own.entry(format!("{path} fn {}", bind.function))
                        .or_default() += 1;
                } else {
                    outside.push(format!("{path}:{} fn {}", bind.line, bind.function));
                }
            }
        }
        assert!(
            unread.is_empty(),
            "files the scan could not read: {unread:?}"
        );
        assert!(
            outside.is_empty(),
            "test code binds port 0 outside test_ports; take the listener from \
             test_ports::held_ephemeral_listener() or held_ephemeral_tokio_listener() and hold \
             it past the port's last use, or a port nothing listens on from unheld_port() (code \
             that bind-probes the port or can kill its holder) or reserve_refused_port() (code \
             that only connects). An integration test under tests/ cannot reach test_ports: give \
             this rule an exception, with its reason, before it binds port 0 there:\n{}",
            outside.join("\n")
        );
        let expected: BTreeMap<String, usize> = [
            "src/test_ports.rs fn reserve_refused_port",
            "src/test_ports.rs fn held_ephemeral_listener",
            "src/test_ports.rs fn held_ephemeral_tokio_listener",
        ]
        .into_iter()
        .map(|site| (site.to_string(), 1))
        .collect();
        assert_eq!(
            own, expected,
            "the scan must find each of test_ports' own port-0 binds once; a scan that misses \
             one is not reading the tree"
        );
    }
}
        assert!(
            scans
                .iter()
                .any(|(path, binds)| path.starts_with("tests/") && binds.is_some()),
            "the scan read no file under tests/, so it shows nothing about integration tests"
        );
