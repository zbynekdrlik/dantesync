//! Grandmaster-source allowlist (camera-box issue 1073).
//!
//! The PTP client historically had NO best-master election of any kind: every
//! Sync packet the capture saw was adopted last-writer-wins (see
//! `controller::PtpController::process_loop_iteration`), so a node that also sees
//! a FOREIGN subnet's PTP multicast (the live incident: the stream box on the
//! rig's `10.77.9.x` also seeing mbc's `10.77.7.x`) could silently lock onto a
//! foreign grandmaster (`10.77.7.109`) instead of the rig grandmaster
//! (`10.77.9.184`).
//!
//! This module is the pure, unit-testable decision layer for a **configurable
//! source allowlist**: the operator lists the trusted grandmaster source
//! IP(s)/subnet(s), and the controller drops any PTP packet whose source IP is
//! not permitted, *as-if it never arrived*. It answers the real question ("which
//! network/GM do we TRUST") deterministically, rather than "which advertises the
//! best clock" (which a foreign Dante GM can win).
//!
//! Backward compatibility: an EMPTY allowlist means UNRESTRICTED — accept any
//! source, exactly the historical last-writer-wins behavior — so a single-GM
//! network is unaffected and every existing config (which has no allowlist field)
//! keeps working unchanged.
//!
//! Fail-open: an allowlist whose entries ALL fail to parse is treated as
//! unrestricted (with the bad entries surfaced for a loud startup warning). A
//! config typo must never take the whole rig's clock offline.

use std::net::Ipv4Addr;

/// An IPv4 prefix (network base + prefix length) for source matching.
///
/// An exact address is just a `/32` prefix, so exact-IP and CIDR entries share
/// one code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ipv4Prefix {
    /// Network base as `u32`, already masked to `prefix_len` significant bits.
    base: u32,
    /// Number of significant leading bits, `0..=32`.
    prefix_len: u8,
}

impl Ipv4Prefix {
    /// The `u32` mask for a prefix length. `/0` is the all-zero mask (matches
    /// everything); `/32` is all-ones (exact match). `1u32 << 32` is UB in Rust
    /// (shift overflow), so `/0` is special-cased.
    fn mask(prefix_len: u8) -> u32 {
        if prefix_len == 0 {
            0
        } else {
            u32::MAX << (32 - prefix_len)
        }
    }

    fn contains(&self, ip: Ipv4Addr) -> bool {
        (u32::from(ip) & Self::mask(self.prefix_len)) == self.base
    }

    /// Parse `"a.b.c.d"` (exact, treated as `/32`) or `"a.b.c.d/N"` (CIDR).
    /// Surrounding whitespace is ignored. Returns a human-readable error for an
    /// invalid address or prefix length so the caller can surface it.
    fn parse(s: &str) -> Result<Ipv4Prefix, String> {
        let s = s.trim();
        let (addr_str, len) = match s.split_once('/') {
            Some((a, l)) => {
                let n: u8 = l
                    .trim()
                    .parse()
                    .map_err(|_| format!("invalid prefix length in '{s}'"))?;
                if n > 32 {
                    return Err(format!("prefix length >32 in '{s}'"));
                }
                (a.trim(), n)
            }
            None => (s, 32u8),
        };
        let addr: Ipv4Addr = addr_str
            .parse()
            .map_err(|_| format!("invalid IPv4 address in '{s}'"))?;
        Ok(Ipv4Prefix {
            base: u32::from(addr) & Self::mask(len),
            prefix_len: len,
        })
    }
}

/// A parsed grandmaster-source allowlist.
///
/// See the module docs for the empty-means-unrestricted and fail-open contracts.
#[derive(Debug, Clone, Default)]
pub struct GmAllowlist {
    prefixes: Vec<Ipv4Prefix>,
    /// Entries that failed to parse, kept verbatim for a loud startup warning.
    invalid: Vec<String>,
}

impl GmAllowlist {
    /// Parse a list of config entries. Blank entries are ignored; entries that
    /// fail to parse are collected in [`invalid_entries`](Self::invalid_entries)
    /// and otherwise skipped (fail-open — see the module docs).
    pub fn parse(entries: &[String]) -> GmAllowlist {
        let mut prefixes = Vec::new();
        let mut invalid = Vec::new();
        for e in entries {
            if e.trim().is_empty() {
                continue;
            }
            match Ipv4Prefix::parse(e) {
                Ok(p) => prefixes.push(p),
                Err(_) => invalid.push(e.clone()),
            }
        }
        GmAllowlist { prefixes, invalid }
    }

    /// True when NO restriction is in effect (no parseable prefixes) — every
    /// source is accepted, the historical last-writer-wins behavior.
    pub fn is_unrestricted(&self) -> bool {
        self.prefixes.is_empty()
    }

    /// True if `ip` is permitted as a grandmaster source. An unrestricted
    /// allowlist (empty, or all-entries-invalid) accepts everything.
    pub fn allows(&self, ip: Ipv4Addr) -> bool {
        self.prefixes.is_empty() || self.prefixes.iter().any(|p| p.contains(ip))
    }

    /// Entries that failed to parse, so the caller can warn loudly at startup.
    pub fn invalid_entries(&self) -> &[String] {
        &self.invalid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    #[test]
    fn empty_allowlist_is_unrestricted_and_accepts_any_source() {
        let a = GmAllowlist::parse(&[]);
        assert!(a.is_unrestricted());
        assert!(a.allows(ip("10.77.7.109")));
        assert!(a.allows(ip("10.77.9.184")));
        assert!(a.allows(ip("1.2.3.4")));
        assert!(a.invalid_entries().is_empty());
    }

    #[test]
    fn blank_and_whitespace_entries_are_ignored_and_stay_unrestricted() {
        let a = GmAllowlist::parse(&["".to_string(), "   ".to_string()]);
        assert!(a.is_unrestricted());
        assert!(a.allows(ip("10.77.7.109")));
        assert!(a.invalid_entries().is_empty());
    }

    #[test]
    fn cidr_subnet_rejects_foreign_and_accepts_rig_the_live_incident() {
        // The exact live values: rig subnet 10.77.9.0/24, foreign GM 10.77.7.109.
        let a = GmAllowlist::parse(&["10.77.9.0/24".to_string()]);
        assert!(!a.is_unrestricted());
        assert!(!a.allows(ip("10.77.7.109")), "foreign subnet must be rejected");
        assert!(a.allows(ip("10.77.9.184")), "rig grandmaster must be accepted");
        assert!(a.allows(ip("10.77.9.1")), "any host on the rig subnet is accepted");
        assert!(!a.allows(ip("10.77.8.184")), "an adjacent subnet is rejected");
    }

    #[test]
    fn exact_ip_entry_is_a_slash_32_match() {
        let a = GmAllowlist::parse(&["10.77.9.184".to_string()]);
        assert!(a.allows(ip("10.77.9.184")));
        assert!(!a.allows(ip("10.77.9.185")), "a different host is rejected");
        assert!(!a.allows(ip("10.77.7.109")));
    }

    #[test]
    fn explicit_slash_32_equals_exact_ip() {
        let exact = GmAllowlist::parse(&["10.77.9.184".to_string()]);
        let slash32 = GmAllowlist::parse(&["10.77.9.184/32".to_string()]);
        for probe in ["10.77.9.184", "10.77.9.185", "10.77.7.109"] {
            assert_eq!(exact.allows(ip(probe)), slash32.allows(ip(probe)));
        }
    }

    #[test]
    fn multiple_entries_are_ored_together() {
        let a = GmAllowlist::parse(&["10.77.9.184".to_string(), "10.77.10.0/24".to_string()]);
        assert!(a.allows(ip("10.77.9.184")));
        assert!(a.allows(ip("10.77.10.5")));
        assert!(!a.allows(ip("10.77.7.109")));
        assert!(!a.allows(ip("10.77.9.185")));
    }

    #[test]
    fn slash_zero_matches_everything() {
        let a = GmAllowlist::parse(&["0.0.0.0/0".to_string()]);
        assert!(!a.is_unrestricted(), "a /0 is a real (if permissive) restriction");
        assert!(a.allows(ip("10.77.7.109")));
        assert!(a.allows(ip("8.8.8.8")));
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        let a = GmAllowlist::parse(&["  10.77.9.0/24  ".to_string()]);
        assert!(a.allows(ip("10.77.9.184")));
        assert!(!a.allows(ip("10.77.7.109")));
        assert!(a.invalid_entries().is_empty());
    }

    #[test]
    fn invalid_entries_are_recorded_but_valid_ones_still_restrict() {
        let a = GmAllowlist::parse(&["not-an-ip".to_string(), "10.77.9.0/24".to_string()]);
        assert_eq!(a.invalid_entries(), &["not-an-ip".to_string()]);
        assert!(!a.is_unrestricted());
        assert!(a.allows(ip("10.77.9.184")));
        assert!(!a.allows(ip("10.77.7.109")));
    }

    #[test]
    fn all_entries_invalid_fails_open_to_unrestricted() {
        // A fully typo'd allowlist must never brick the clock: it degrades to
        // accept-any (with the bad entries surfaced for a warning).
        let a = GmAllowlist::parse(&["garbage".to_string(), "10.77.9.0/33".to_string()]);
        assert!(a.is_unrestricted(), "all-invalid must fail open");
        assert!(a.allows(ip("10.77.7.109")));
        assert_eq!(a.invalid_entries().len(), 2);
    }

    #[test]
    fn out_of_range_prefix_length_is_invalid() {
        let a = GmAllowlist::parse(&["10.77.9.0/33".to_string()]);
        assert_eq!(a.invalid_entries().len(), 1);
        assert!(a.is_unrestricted());
    }
}
