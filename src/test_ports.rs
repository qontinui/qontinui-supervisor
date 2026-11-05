//! Test-only loopback ports: one fixture per kind of consumer.
//!
//! A port a test lets go of is no longer the test's. Binding `127.0.0.1:0`,
//! reading the port and dropping the socket hands the port back to the kernel,
//! and any `bind(0)` or `connect()` on the box can be given it before the code
//! under test looks (coord finding `2d01deb3`, the `flywheel` flake; finding
//! `ed257f86`, the purge flake). So no fixture here releases a port. Pick by
//! what the code under test does with the port:
//!
//! * It only CONNECTS: [`reserve_refused_port`]. Bound, never listened on, and
//!   kept, so a connect is refused and nothing else can listen there.
//! * It BIND-PROBES the port ([`crate::process::port::is_port_listening`], a
//!   stop's `wait_for_port_free`, the purge's crash arm), or it can reach a kill
//!   of whatever holds the port (the stop ladder, the purge's Windows
//!   `kill_by_port`): [`unheld_port`]. Nothing holds it, and no kernel
//!   allocation hands it out.
//!
//! Plan `2026-11-05-supervisor-test-flakes-dead-ports-etxtbsy`, Phase 2.

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

/// Slot `i` hands out the fixture port `FIXTURE_BLOCK + i` and is claimed by
/// binding the claim port `CLAIM_BLOCK + i`, for `i` in `0..SLOTS`. Both blocks
/// lie below every default ephemeral range (Linux 32768, Windows and macOS
/// 49152, checked on this box by [`unheld_port`]), and clear of the temp-runner
/// ports 9875-9899.
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
}
