//! Locale-independent parsing of `netstat -ano` output.
//!
//! **Cross-platform on purpose**, exactly like [`super::slot_territory`]: the
//! caller (`super::windows::find_pids_on_port`) is
//! `#[cfg(target_os = "windows")]` and CI is `ubuntu-latest` only, so anything
//! decided inside that module is decided in code the merge gate can never
//! execute. The predicate that says "this PID is LISTENING on this port" is
//! the one that must not be able to rot silently, so it lives here as a pure
//! function over captured text and is unit-tested on the gate.
//!
//! # The defect this module exists to prevent
//!
//! The previous implementation shelled out to
//! `cmd /C "netstat -ano | findstr :<port> | findstr LISTENING"` and treated an
//! empty result as "the port is idle".
//!
//! **`netstat` localizes the state column.** On a German Windows install the
//! listening state prints as `ABHÖREN`, not `LISTENING` (French `À L'ÉCOUTE`,
//! Spanish `ESCUCHAR`, …). `findstr LISTENING` therefore matched **zero rows on
//! every non-English Windows**, and the function returned `Ok(vec![])` — which
//! its own callers are documented to read as "the probe RAN and the port is
//! genuinely idle".
//!
//! That is precisely the silent-empty-treated-as-NO class the `Ok` / `Err`
//! split in `super::windows::find_pids_on_port` was introduced to close,
//! reintroduced one layer further down where the type system could not see it.
//! Observed 2026-07-31 on a German-locale box: `POST /runners/primary/stop`
//! failed with "port 9876 is still in use after PID kill, tree-kill,
//! kill-by-port" while the runner (PID 8872) sat plainly in `netstat` output on
//! an `ABHÖREN` row — **not one of those three strategies had a target to run
//! against**, because every PID-discovery path funnels through here. The same
//! blindness left the primary permanently at `pid: null` in the registry, since
//! the health cache's PID-recovery tick is gated on this probe too.
//!
//! # Why the state column is not consulted at all
//!
//! Rather than teach the parser every translation of "LISTENING" (an unbounded,
//! silently-incomplete list), we key on the **structure** of the row, which
//! Windows does not localize:
//!
//! ```text
//! Proto  Local Address        Foreign Address      State       PID
//! TCP    127.0.0.1:9876       0.0.0.0:0            ABHÖREN     8872
//! TCP    127.0.0.1:9876       127.0.0.1:53716      HERGESTELLT 8872
//! TCP    127.0.0.1:57700      127.0.0.1:9876       WARTEND     0
//! ```
//!
//! A TCP socket with **no peer** is the only kind whose foreign address is the
//! wildcard (`0.0.0.0:0` for IPv4, `[::]:0` for IPv6). Every *connected* state
//! — established, time-wait, syn-sent, close-wait — names a real remote
//! endpoint. So "row is TCP, local port is ours, foreign endpoint is the
//! wildcard" identifies peerless sockets without reading a single localized
//! word.
//!
//! **Precision limit, stated honestly:** that rule matches LISTENING *and* the
//! rarer peerless states Windows also prints with a wildcard peer — `BOUND` (a
//! socket `bind()`-ed but never `listen()`-ed) and `CLOSED`. netstat gives no
//! structural way to tell those apart from a listener; the only discriminator
//! is the localized state word, which is the thing that broke. The rule is
//! therefore "peerless TCP socket on this port", which is a **superset** of
//! listeners.
//!
//! That superset is safe for the "is this port occupied / who has it" question
//! every caller actually asks — a bound socket does hold the port. It is NOT
//! by itself sufficient authority to kill: a foreign process that merely bound
//! our port would be named here. So `manager::stop_runner_by_id` re-verifies a
//! probe-derived PID against the runner's own image path before killing it
//! (`stop_ledger::verify_target_image`), and port-range exposure is small
//! anyway (9875-9899 sits below Windows' ephemeral range, so a collision takes
//! a deliberate low-port bind rather than an ephemeral allocation).
//!
//! This also fixes a latent mis-match in the old `findstr :<port>` filter: it
//! was a plain substring search over the whole row, so a row whose *foreign*
//! address carried our port (the `WARTEND` row above) matched too. Requiring
//! the port to be on the **local** endpoint removes that.
//!
//! # Unrecognized output is UNKNOWN, not "idle"
//!
//! If the captured text yields **no TCP rows at all**, the parser reports
//! [`ListenerScan::Unrecognized`] rather than an empty PID list. `netstat -ano`
//! on any live Windows box always prints TCP rows (RPC endpoint mapper, SMB,
//! …), so zero rows means the output shape is not what we think it is — a
//! future format change, a truncated capture, a wrapper that printed an error.
//! Callers map that to `Err` and must not conclude the port is free. Getting
//! this wrong in the *other* direction is what this whole module is about.

/// Outcome of scanning captured `netstat -ano` text for listeners on a port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenerScan {
    /// The text parsed as netstat table output. Carries every distinct PID
    /// found LISTENING on the requested port — possibly empty, which here
    /// genuinely means "the port is idle".
    Parsed(Vec<u32>),
    /// The text could not be recognized as netstat output: not one TCP row was
    /// found. The port's state is **UNKNOWN**; no caller may treat this as
    /// idle and no caller may kill on it. `sample` carries a short excerpt for
    /// the diagnostic so the operator can see what we actually got.
    Unrecognized { sample: String },
}

/// Longest excerpt of unrecognized output carried into the diagnostic,
/// in CHARACTERS (it is consumed with `chars().take(..)`).
const UNRECOGNIZED_SAMPLE_CHARS: usize = 400;

/// Extract the port from a `netstat` endpoint token.
///
/// Handles IPv4 (`127.0.0.1:9876`, `0.0.0.0:0`), IPv6 (`[::]:9876`,
/// `[::1]:9876`) and the UDP wildcard (`*:*`, which yields `None`). The port is
/// always after the LAST colon, which is what makes the bracketed IPv6 form
/// fall out for free.
fn endpoint_port(endpoint: &str) -> Option<u16> {
    endpoint.rsplit_once(':')?.1.parse::<u16>().ok()
}

/// True when a foreign-address token is the "no peer" wildcard that Windows
/// prints for a listening socket: `0.0.0.0:0` (IPv4) or `[::]:0` (IPv6).
///
/// Keyed on the port being 0 rather than on the address text, so an
/// unanticipated wildcard spelling still classifies correctly — no live TCP
/// peer is ever on port 0.
fn is_wildcard_peer(endpoint: &str) -> bool {
    endpoint_port(endpoint) == Some(0)
}

/// Scan captured `netstat -ano` output for every distinct PID LISTENING on
/// `port`.
///
/// Pure, locale-independent, and allocation-light. See the module docs for the
/// structural rule and for why unrecognized input is a distinct outcome.
pub fn scan_listeners(netstat_stdout: &str, port: u16) -> ListenerScan {
    let mut pids: Vec<u32> = Vec::new();
    let mut saw_tcp_row = false;

    for line in netstat_stdout.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // Proto, local, foreign, [state...], pid. The state column may be
        // multiple words in some locales, so the layout is anchored from BOTH
        // ends rather than by a fixed field count.
        if fields.len() < 4 || !fields[0].eq_ignore_ascii_case("TCP") {
            continue;
        }
        // The trailing field is the PID. A row whose last field is not a
        // number is not a data row (a wrapped header, a banner) — skip it
        // without counting it as a TCP row.
        let Ok(pid) = fields[fields.len() - 1].parse::<u32>() else {
            continue;
        };
        saw_tcp_row = true;

        let (local, foreign) = (fields[1], fields[2]);
        // Listening rows only, identified structurally: the peer endpoint is
        // the wildcard. This is what replaces `findstr LISTENING`.
        if !is_wildcard_peer(foreign) {
            continue;
        }
        // The port must be OURS and on the LOCAL endpoint — a row that merely
        // mentions the port somewhere is not a listener on it.
        if endpoint_port(local) != Some(port) {
            continue;
        }
        // PID 0 is the kernel's "no owning process" placeholder; it is never a
        // kill target and never a runner.
        if pid > 0 && !pids.contains(&pid) {
            pids.push(pid);
        }
    }

    if !saw_tcp_row {
        let sample: String = netstat_stdout
            .chars()
            .take(UNRECOGNIZED_SAMPLE_CHARS)
            .collect();
        return ListenerScan::Unrecognized {
            sample: sample.trim().to_string(),
        };
    }

    ListenerScan::Parsed(pids)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim capture from the German-locale Windows 11 box (10.0.26200)
    /// where `POST /runners/primary/stop` failed on 2026-07-31. PID 8872 is
    /// the primary runner; the state column reads `ABHÖREN`, so the old
    /// `findstr LISTENING` filter matched nothing at all.
    const GERMAN_NETSTAT: &str = "\r
Aktive Verbindungen\r
\r
  Proto  Lokale Adresse         Remoteadresse          Status           PID\r
  TCP    0.0.0.0:135            0.0.0.0:0              ABHÖREN         1288\r
  TCP    0.0.0.0:445            0.0.0.0:0              ABHÖREN         4\r
  TCP    127.0.0.1:9876         0.0.0.0:0              ABHÖREN         8872\r
  TCP    127.0.0.1:9876         127.0.0.1:53716        HERGESTELLT     8872\r
  TCP    127.0.0.1:53716        127.0.0.1:9876         HERGESTELLT     33812\r
  TCP    127.0.0.1:57700        127.0.0.1:9876         WARTEND         0\r
  TCP    [::]:445               [::]:0                 ABHÖREN         4\r
  UDP    0.0.0.0:5353           *:*                                    3120\r
";

    const ENGLISH_NETSTAT: &str = "
Active Connections

  Proto  Local Address          Foreign Address        State           PID
  TCP    0.0.0.0:135            0.0.0.0:0              LISTENING       1288
  TCP    127.0.0.1:9876         0.0.0.0:0              LISTENING       8872
  TCP    127.0.0.1:9876         127.0.0.1:53716        ESTABLISHED     8872
  TCP    127.0.0.1:57700        127.0.0.1:9876         TIME_WAIT       0
";

    /// THE regression. A German-locale capture must yield the listener PID.
    /// Before the fix this whole path produced an empty list, which callers
    /// read as "the port is idle" — so the stop path had nothing to kill.
    #[test]
    fn german_locale_listener_is_found() {
        assert_eq!(
            scan_listeners(GERMAN_NETSTAT, 9876),
            ListenerScan::Parsed(vec![8872])
        );
    }

    /// The English capture must behave identically — the fix must not trade
    /// one locale for another.
    #[test]
    fn english_locale_listener_is_found() {
        assert_eq!(
            scan_listeners(ENGLISH_NETSTAT, 9876),
            ListenerScan::Parsed(vec![8872])
        );
    }

    /// Both locales must agree on every port, not just the one we debugged.
    #[test]
    fn locales_agree_on_a_shared_port() {
        assert_eq!(
            scan_listeners(GERMAN_NETSTAT, 135),
            scan_listeners(ENGLISH_NETSTAT, 135)
        );
    }

    /// An IPv6 listener (`[::]:445` → `[::]:0`) must parse: the port is after
    /// the last colon, and the wildcard peer is recognized by port 0.
    #[test]
    fn ipv6_listener_is_found() {
        assert_eq!(
            scan_listeners(GERMAN_NETSTAT, 445),
            ListenerScan::Parsed(vec![4])
        );
    }

    /// A row whose *foreign* address carries our port is NOT a listener on it.
    /// The old `findstr :<port>` substring filter matched such rows; only the
    /// (locale-fragile) state filter kept them out.
    #[test]
    fn foreign_side_port_match_is_not_a_listener() {
        let text =
            "  TCP    127.0.0.1:57700        127.0.0.1:9876         WARTEND         33812\r\n";
        assert_eq!(scan_listeners(text, 9876), ListenerScan::Parsed(vec![]));
    }

    /// An established connection on our port is not a listener either — only
    /// the wildcard-peer row is.
    #[test]
    fn established_row_on_our_port_is_not_a_listener() {
        let text =
            "  TCP    127.0.0.1:9876         127.0.0.1:53716        HERGESTELLT     8872\r\n";
        assert_eq!(scan_listeners(text, 9876), ListenerScan::Parsed(vec![]));
    }

    /// PID 0 (kernel placeholder) is never returned — it must never become a
    /// kill target.
    #[test]
    fn pid_zero_is_never_returned() {
        let text = "  TCP    0.0.0.0:9876           0.0.0.0:0              ABHÖREN         0\r\n";
        assert_eq!(scan_listeners(text, 9876), ListenerScan::Parsed(vec![]));
    }

    /// A port with real TCP traffic present but no listener parses as an empty
    /// list — this is the one case that legitimately means "idle".
    #[test]
    fn genuinely_idle_port_parses_as_empty() {
        assert_eq!(
            scan_listeners(GERMAN_NETSTAT, 9999),
            ListenerScan::Parsed(vec![])
        );
    }

    /// Two processes bound to the same port (dual-stack, or a leaked socket)
    /// must both come back, de-duplicated.
    #[test]
    fn multiple_distinct_listeners_are_all_returned() {
        let text = "\
  TCP    0.0.0.0:9876           0.0.0.0:0              ABHÖREN         111\r
  TCP    [::]:9876              [::]:0                 ABHÖREN         222\r
  TCP    127.0.0.1:9876         0.0.0.0:0              ABHÖREN         111\r
";
        assert_eq!(
            scan_listeners(text, 9876),
            ListenerScan::Parsed(vec![111, 222])
        );
    }

    /// Output that is not netstat table output at all must be UNKNOWN, never
    /// an empty (= "idle") list. This is the guard against the next silent
    /// format/locale drift.
    #[test]
    fn unrecognized_output_is_not_reported_as_idle() {
        let scan = scan_listeners("Der Befehl \"netstat\" ist falsch geschrieben.", 9876);
        match scan {
            ListenerScan::Unrecognized { sample } => {
                assert!(sample.contains("netstat"), "sample must quote the output");
            }
            other => panic!("expected Unrecognized, got {other:?}"),
        }
    }

    /// Empty output is the degenerate case of the same rule — a probe that
    /// produced nothing has told us nothing.
    #[test]
    fn empty_output_is_unrecognized() {
        assert!(matches!(
            scan_listeners("", 9876),
            ListenerScan::Unrecognized { .. }
        ));
    }

    /// A header-only capture (no data rows) is likewise UNKNOWN: `netstat` on
    /// a live Windows box always prints TCP rows.
    #[test]
    fn header_only_output_is_unrecognized() {
        let text = "\nAktive Verbindungen\n\n  Proto  Lokale Adresse  Remoteadresse  Status  PID\n";
        assert!(matches!(
            scan_listeners(text, 9876),
            ListenerScan::Unrecognized { .. }
        ));
    }

    /// A locale whose state column is TWO words must still parse — the row
    /// layout is anchored from both ends, not by a fixed field count.
    #[test]
    fn multi_word_state_column_still_parses() {
        let text =
            "  TCP    127.0.0.1:9876         0.0.0.0:0              A L'ECOUTE      8872\r\n";
        assert_eq!(scan_listeners(text, 9876), ListenerScan::Parsed(vec![8872]));
    }

    /// UDP rows are never listeners for our purposes (the runner API is TCP),
    /// and their 4-column layout must not be mis-parsed into one.
    #[test]
    fn udp_rows_are_ignored() {
        let text = "\
  TCP    0.0.0.0:135            0.0.0.0:0              ABHÖREN         1288\r
  UDP    0.0.0.0:9876           *:*                                    3120\r
";
        assert_eq!(scan_listeners(text, 9876), ListenerScan::Parsed(vec![]));
    }

    #[test]
    fn endpoint_port_handles_ipv4_ipv6_and_wildcard() {
        assert_eq!(endpoint_port("127.0.0.1:9876"), Some(9876));
        assert_eq!(endpoint_port("[::1]:9876"), Some(9876));
        assert_eq!(endpoint_port("[::]:0"), Some(0));
        assert_eq!(endpoint_port("0.0.0.0:0"), Some(0));
        assert_eq!(endpoint_port("*:*"), None);
        assert_eq!(endpoint_port("nonsense"), None);
    }

    #[test]
    fn wildcard_peer_recognizes_both_families() {
        assert!(is_wildcard_peer("0.0.0.0:0"));
        assert!(is_wildcard_peer("[::]:0"));
        assert!(!is_wildcard_peer("127.0.0.1:53716"));
        assert!(!is_wildcard_peer("*:*"));
    }
}
