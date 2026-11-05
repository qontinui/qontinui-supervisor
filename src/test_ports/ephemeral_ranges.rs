//! The port ranges this box's kernel hands out, or reserves for a service, read
//! so [`super::unheld_port`] can show its blocks lie outside them.
//!
//! * Linux: `/proc/sys/net/ipv4/ip_local_port_range`, the range `bind(0)` and
//!   `connect()` draw from (IPv4 and IPv6 alike).
//! * Windows: `netsh int ipv4 show dynamicport tcp` (the dynamic range), and
//!   `netsh int ipv4 show excludedportrange protocol=tcp` (ranges Hyper-V, WinNAT
//!   or an administrator reserved, which a service may bind at any time).
//! * macOS: `sysctl net.inet.ip.portrange.first` and `.last`.
//!
//! The parsers read numbers, not labels, so a localised `netsh` reads the same.

use std::fmt;

/// An inclusive port range, and what it was read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PortRange {
    pub(super) first: u16,
    pub(super) last: u16,
    pub(super) source: &'static str,
}

impl fmt::Display for PortRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}-{}", self.source, self.first, self.last)
    }
}

/// Every range this box hands out or reserves. `Err` names what could not be read.
pub(super) fn read() -> Result<Vec<PortRange>, String> {
    #[cfg(target_os = "linux")]
    {
        let path = "/proc/sys/net/ipv4/ip_local_port_range";
        let text = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
        parse_two_ports(&text, "ip_local_port_range")
            .map(|range| vec![range])
            .ok_or_else(|| format!("{path} did not read as two ports: {text:?}"))
    }
    #[cfg(windows)]
    {
        let dynamic = command_output("netsh", &["int", "ipv4", "show", "dynamicport", "tcp"])?;
        let excluded = command_output(
            "netsh",
            &["int", "ipv4", "show", "excludedportrange", "protocol=tcp"],
        )?;
        let mut ranges = vec![parse_netsh_dynamic(&dynamic).ok_or_else(|| {
            format!("`netsh int ipv4 show dynamicport tcp` named no start and count: {dynamic:?}")
        })?];
        ranges.extend(parse_netsh_excluded(&excluded));
        Ok(ranges)
    }
    #[cfg(target_os = "macos")]
    {
        let text = command_output(
            "sysctl",
            &[
                "-n",
                "net.inet.ip.portrange.first",
                "net.inet.ip.portrange.last",
            ],
        )?;
        parse_two_ports(&text, "net.inet.ip.portrange")
            .map(|range| vec![range])
            .ok_or_else(|| {
                format!("sysctl net.inet.ip.portrange did not read as two ports: {text:?}")
            })
    }
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    {
        Err(format!(
            "no reader for {}'s ephemeral port range",
            std::env::consts::OS
        ))
    }
}

/// A command's stdout, or why it could not be had.
#[cfg(any(windows, target_os = "macos"))]
fn command_output(program: &str, args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("run {program}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} {} exited {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Two whitespace-separated ports, first then last: the Linux sysctl file, and
/// `sysctl -n`'s two lines.
fn parse_two_ports(text: &str, source: &'static str) -> Option<PortRange> {
    let mut ports = text.split_whitespace().map(str::parse::<u16>);
    let (first, last) = (ports.next()?.ok()?, ports.next()?.ok()?);
    (ports.next().is_none() && first <= last).then_some(PortRange {
        first,
        last,
        source,
    })
}

/// `netsh int ipv4 show dynamicport tcp`: the numbers after the first two colons
/// are the start port and the number of ports.
fn parse_netsh_dynamic(text: &str) -> Option<PortRange> {
    let mut values = text
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter_map(|(_, value)| value.trim().parse::<u32>().ok());
    let (start, count) = (values.next()?, values.next()?);
    let last = start.checked_add(count.checked_sub(1)?)?;
    Some(PortRange {
        first: u16::try_from(start).ok()?,
        last: u16::try_from(last).ok()?,
        source: "dynamicport tcp",
    })
}

/// `netsh int ipv4 show excludedportrange protocol=tcp`: every line that is a
/// start and an end port, an administered exclusion's trailing `*` included.
fn parse_netsh_excluded(text: &str) -> Vec<PortRange> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim().trim_end_matches('*').trim();
            let mut ports = line.split_whitespace().map(str::parse::<u16>);
            let (first, last) = (ports.next()?.ok()?, ports.next()?.ok()?);
            (ports.next().is_none() && first <= last).then_some(PortRange {
                first,
                last,
                source: "excludedportrange tcp",
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_linux_and_sysctl_shapes_read_as_one_range() {
        let linux = parse_two_ports("32768\t60999\n", "ip_local_port_range").expect("parses");
        assert_eq!((linux.first, linux.last), (32_768, 60_999));
        let macos = parse_two_ports("49152\n65535\n", "net.inet.ip.portrange").expect("parses");
        assert_eq!((macos.first, macos.last), (49_152, 65_535));
        assert_eq!(parse_two_ports("", "x"), None);
        assert_eq!(parse_two_ports("40000", "x"), None);
        assert_eq!(parse_two_ports("60999 32768", "x"), None, "reversed");
        assert_eq!(parse_two_ports("1 2 3", "x"), None, "three numbers");
    }

    #[test]
    fn netsh_dynamicport_reads_in_any_display_language() {
        let english =
            "\r\nProtocol tcp Dynamic Port Range\r\n---------------------------------\r\n\
                       Start Port      : 49152\r\nNumber of Ports : 16384\r\n";
        let german = "\r\nDynamischer Portbereich für tcp-Protokoll\r\n\
                      ---------------------------------\r\nStartport       : 49152\r\n\
                      Anzahl der Ports : 16384\r\n";
        for text in [english, german] {
            let range = parse_netsh_dynamic(text).expect("parses");
            assert_eq!((range.first, range.last), (49_152, 65_535), "{text}");
        }
        assert_eq!(
            parse_netsh_dynamic("Start Port : 1024\r\nNumber of Ports : 0\r\n"),
            None
        );
        assert_eq!(
            parse_netsh_dynamic("Start Port : 65000\r\nNumber of Ports : 1000\r\n"),
            None
        );
    }

    #[test]
    fn netsh_excludedportrange_reads_every_range_and_skips_the_rest() {
        let text = "\r\nProtocol tcp Port Exclusion Ranges\r\n\r\n\
                    Start Port    End Port\r\n----------    --------\r\n\
                    \x20     5357        5357\r\n     50000       50059     *\r\n\r\n\
                    * - Administered port exclusions.\r\n";
        let ranges = parse_netsh_excluded(text);
        assert_eq!(
            ranges.iter().map(|r| (r.first, r.last)).collect::<Vec<_>>(),
            [(5_357, 5_357), (50_000, 50_059)]
        );
        assert!(parse_netsh_excluded("Start Port    End Port\r\n").is_empty());
    }

    #[test]
    fn this_box_reads_at_least_one_range() {
        let ranges = read().unwrap_or_else(|why| panic!("{why}"));
        assert!(!ranges.is_empty());
    }
}
