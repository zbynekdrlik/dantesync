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

    /// The most-significant `min(self.prefix_len, other.prefix_len)` bits of both
    /// prefixes agree — i.e. the two networks OVERLAP (one contains the other, or
    /// they are equal). Used to decide whether a local interface's subnet is on
    /// the same network as a trusted grandmaster prefix; the symmetric "shorter
    /// mask" test works for BOTH an exact-GM allowlist entry (`10.77.9.184/32`
    /// overlaps the rig `/24` interface) and a CIDR entry (`10.77.9.0/24` overlaps
    /// the rig interface whose own IP is inside it).
    fn overlaps(&self, other: &Ipv4Prefix) -> bool {
        let shorter = self.prefix_len.min(other.prefix_len);
        let m = Self::mask(shorter);
        (self.base & m) == (other.base & m)
    }

    /// Parse `"a.b.c.d"` (exact, treated as `/32`) or `"a.b.c.d/N"` (CIDR).
    /// Surrounding whitespace is ignored. Returns a human-readable error for an
    /// invalid address or prefix length so the caller can surface it.
    fn parse(s: &str) -> Result<Ipv4Prefix, String> {
        let s = s.trim();
        let (addr_str, len) = match s.split_once('/') {
            Some((a, l)) => {
                // Digit-only: `u8::from_str` otherwise accepts a leading `+`
                // (`"/+24"` → 24) and an empty string is a parse error we want a
                // clear message for.
                let l = l.trim();
                if l.is_empty() || !l.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(format!("invalid prefix length in '{s}'"));
                }
                let n: u8 = l
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

    /// Number of ACTIVE (successfully parsed) prefixes — for a startup log that
    /// reports the effective policy rather than the raw (possibly-typo'd) config.
    pub fn prefix_count(&self) -> usize {
        self.prefixes.len()
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

    /// camera-box issue 1073 (interface-selection half): pick which local
    /// interface a multi-homed box should attach its PTP capture / IGMP join to,
    /// by which one is on the SAME network as a trusted grandmaster prefix.
    ///
    /// `candidates` are `(interface_ip, interface_netmask)` pairs in the caller's
    /// enumeration order; the returned index is into that slice. Returns `None`
    /// when the allowlist gives no discriminating signal (unrestricted, or only
    /// `/0` entries) or no candidate is on a trusted subnet — the caller then
    /// keeps its existing default-interface behavior, so single-homed and
    /// no-allowlist boxes are byte-identical to before.
    ///
    /// This is the single-answer convenience wrapper over
    /// [`best_interface_matches`](Self::best_interface_matches): the first-listed
    /// of the best-scoring candidates, so an exact tie is resolved deterministically
    /// (first-listed) across restarts. A caller that must DETECT an ambiguous tie
    /// (more than one interface equally on a trusted network — an over-broad
    /// allowlist) uses `best_interface_matches` directly.
    pub fn select_interface(&self, candidates: &[(Ipv4Addr, Option<Ipv4Addr>)]) -> Option<usize> {
        self.best_interface_matches(candidates).first().copied()
    }

    /// The candidate indices (ascending) that ALL achieve the best interface
    /// match — the most-specific trusted prefix, then the longest interface
    /// prefix. Empty when no candidate is on a trusted subnet.
    ///
    /// Normally length 0 (no match) or 1 (a unique winner — the real fleet case,
    /// e.g. a `/24` allowlist matching exactly the rig NIC). Length ≥ 2 means the
    /// allowlist is too broad to disambiguate two distinct interfaces (e.g. a
    /// `/16` that spans both the rig and mbc subnets); `find_ptp_capture_device`
    /// treats that as ambiguous and keeps the default interface rather than let
    /// pcap enumeration order silently decide (review 🟡, camera-box issue 1073).
    pub fn best_interface_matches(
        &self,
        candidates: &[(Ipv4Addr, Option<Ipv4Addr>)],
    ) -> Vec<usize> {
        // (index, matched trusted-prefix len, interface prefix len) per match.
        let mut scored: Vec<(usize, u8, u8)> = Vec::new();
        for (i, (ip, netmask)) in candidates.iter().enumerate() {
            // A candidate without a netmask, or the degenerate `0.0.0.0` netmask
            // (junk/APIPA/misconfigured adapter — enumeration can genuinely
            // report this), carries no real subnet and is skipped, mirroring the
            // #53 `select_ntp_pcap_device` guard.
            let netmask = match netmask {
                Some(m) if *m != Ipv4Addr::UNSPECIFIED => *m,
                _ => continue,
            };
            // `prefix_len` from `count_ones()` and `base` from `ip & netmask`
            // agree for a CONTIGUOUS mask (every real NIC); a non-contiguous mask
            // is RFC-4632-invalid and the OS never produces one. If it somehow
            // occurred, `overlaps` masks to the shorter length either way, so the
            // effect is at worst a conservative false MISS (→ fallback), never a
            // false match.
            let iface = Ipv4Prefix {
                base: u32::from(*ip) & u32::from(netmask),
                prefix_len: u32::from(netmask).count_ones() as u8,
            };
            // The most specific TRUSTED prefix this interface is on. A `/0` entry
            // ("trust everything") gives no discriminating signal for interface
            // selection, so it is ignored here (unlike source filtering, where a
            // `/0` is a real, if permissive, restriction).
            let matched: Option<u8> = self
                .prefixes
                .iter()
                .filter(|p| p.prefix_len > 0 && p.overlaps(&iface))
                .map(|p| p.prefix_len)
                .max();
            if let Some(gm_len) = matched {
                scored.push((i, gm_len, iface.prefix_len));
            }
        }
        // Best key = (most specific trusted prefix, then longest interface
        // prefix). Every candidate sharing that exact key is returned in
        // ascending index order, so the caller sees ties AND the first-listed
        // stays first (deterministic across restarts).
        let best_key = scored.iter().map(|&(_, g, f)| (g, f)).max();
        match best_key {
            Some(bk) => scored
                .iter()
                .filter(|&&(_, g, f)| (g, f) == bk)
                .map(|&(i, _, _)| i)
                .collect(),
            None => Vec::new(),
        }
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
        assert!(
            !a.allows(ip("10.77.7.109")),
            "foreign subnet must be rejected"
        );
        assert!(
            a.allows(ip("10.77.9.184")),
            "rig grandmaster must be accepted"
        );
        assert!(
            a.allows(ip("10.77.9.1")),
            "any host on the rig subnet is accepted"
        );
        assert!(
            !a.allows(ip("10.77.8.184")),
            "an adjacent subnet is rejected"
        );
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
        assert!(
            !a.is_unrestricted(),
            "a /0 is a real (if permissive) restriction"
        );
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

    #[test]
    fn non_digit_or_empty_prefix_length_is_rejected() {
        // `u8::from_str` accepts a leading '+' ("/+24" would silently become /24);
        // an empty prefix ("10.0.0.0/") is a parse error we reject with a clear
        // message. Both must land in invalid_entries, not be silently applied.
        // (Surrounding whitespace like "/ 24" IS tolerated — it trims to "24".)
        for bad in ["10.77.9.0/+24", "10.77.9.0/", "10.77.9.0/2x"] {
            let a = GmAllowlist::parse(&[bad.to_string()]);
            assert_eq!(
                a.invalid_entries(),
                &[bad.to_string()],
                "'{bad}' must be rejected as an invalid prefix"
            );
            assert!(a.is_unrestricted());
        }
    }

    #[test]
    fn prefix_count_reports_only_active_parsed_prefixes() {
        let a = GmAllowlist::parse(&[
            "10.77.9.0/24".to_string(),
            "garbage".to_string(),
            "10.77.10.5".to_string(),
        ]);
        assert_eq!(a.prefix_count(), 2, "only the two valid entries are active");
        assert_eq!(a.invalid_entries().len(), 1);
    }

    fn nm(s: &str) -> Option<Ipv4Addr> {
        Some(s.parse().unwrap())
    }

    #[test]
    fn dual_homed_box_selects_the_rig_interface_over_mbc_camerabox_issue_1073() {
        // The live incident: the stream box is dual-homed — rig NIC 10.77.9.204/24
        // and mbc NIC 10.77.7.204/24 — and the PTP capture/IGMP join inherited the
        // mbc NIC, so the box only ever saw the foreign 10.77.7.x grandmaster.
        // With the rig subnet allowlisted, the capture interface MUST be the rig
        // NIC (index 0 here).
        let allow = GmAllowlist::parse(&["10.77.9.0/24".to_string()]);
        let candidates = [
            (ip("10.77.9.204"), nm("255.255.255.0")), // rig NIC
            (ip("10.77.7.204"), nm("255.255.255.0")), // mbc NIC
        ];
        assert_eq!(
            allow.select_interface(&candidates),
            Some(0),
            "the rig-subnet interface must be chosen on a dual-homed box"
        );
    }

    #[test]
    fn select_interface_is_order_independent_picks_rig_at_index_1() {
        // Same as above but the mbc NIC is enumerated FIRST — the pick must still
        // be the rig NIC, proving it is subnet-based, not "return the first".
        let allow = GmAllowlist::parse(&["10.77.9.0/24".to_string()]);
        let candidates = [
            (ip("10.77.7.204"), nm("255.255.255.0")), // mbc NIC
            (ip("10.77.9.204"), nm("255.255.255.0")), // rig NIC
        ];
        assert_eq!(allow.select_interface(&candidates), Some(1));
    }

    #[test]
    fn exact_ip_allowlist_selects_the_interface_on_the_gm_subnet() {
        // An exact-GM allowlist entry (10.77.9.184/32) must still select the rig
        // interface (10.77.9.204/24), whose /24 subnet CONTAINS the GM — the
        // symmetric-overlap test, not "interface IP inside the /32".
        let allow = GmAllowlist::parse(&["10.77.9.184".to_string()]);
        let candidates = [
            (ip("10.77.9.204"), nm("255.255.255.0")), // rig NIC
            (ip("10.77.7.204"), nm("255.255.255.0")), // mbc NIC
        ];
        assert_eq!(allow.select_interface(&candidates), Some(0));
    }

    #[test]
    fn no_interface_on_a_trusted_subnet_returns_none_falls_back_to_default() {
        // If NO candidate is on a trusted subnet, return None so the caller keeps
        // its existing default-interface behavior (never a wrong forced pick).
        let allow = GmAllowlist::parse(&["10.77.9.0/24".to_string()]);
        let candidates = [
            (ip("10.77.7.204"), nm("255.255.255.0")),
            (ip("10.77.8.204"), nm("255.255.255.0")),
        ];
        assert_eq!(allow.select_interface(&candidates), None);
    }

    #[test]
    fn empty_allowlist_returns_none_backward_compatible() {
        // Unrestricted allowlist → no signal → None → default-interface behavior
        // (single-homed and no-allowlist boxes stay byte-identical to before).
        let allow = GmAllowlist::parse(&[]);
        let candidates = [
            (ip("10.77.9.204"), nm("255.255.255.0")),
            (ip("10.77.7.204"), nm("255.255.255.0")),
        ];
        assert_eq!(allow.select_interface(&candidates), None);
    }

    #[test]
    fn slash_zero_allowlist_gives_no_interface_signal_returns_none() {
        // A /0 ("trust everything") is a real restriction for SOURCE filtering
        // but useless for interface selection — it must not force a pick.
        let allow = GmAllowlist::parse(&["0.0.0.0/0".to_string()]);
        let candidates = [
            (ip("10.77.9.204"), nm("255.255.255.0")),
            (ip("10.77.7.204"), nm("255.255.255.0")),
        ];
        assert_eq!(allow.select_interface(&candidates), None);
    }

    #[test]
    fn zero_netmask_candidate_is_skipped_even_when_only_candidate() {
        // A 0.0.0.0 netmask (junk/APIPA adapter) would vacuously "overlap" any
        // prefix — it must be excluded, not selected (the #53 guard).
        let allow = GmAllowlist::parse(&["10.77.9.0/24".to_string()]);
        let candidates = [(ip("10.77.9.204"), nm("0.0.0.0"))];
        assert_eq!(allow.select_interface(&candidates), None);
        // A candidate with no netmask at all is likewise skipped.
        let candidates_no_mask = [(ip("10.77.9.204"), None)];
        assert_eq!(allow.select_interface(&candidates_no_mask), None);
    }

    #[test]
    fn most_specific_trusted_prefix_wins_across_interfaces() {
        // Two trusted prefixes overlap two different interfaces; the interface on
        // the MORE SPECIFIC trusted prefix (the /24) wins over the one that only
        // matches the broad /16.
        let allow = GmAllowlist::parse(&["10.77.0.0/16".to_string(), "10.77.9.0/24".to_string()]);
        let candidates = [
            (ip("10.77.8.5"), nm("255.255.255.0")),   // matches /16 only
            (ip("10.77.9.204"), nm("255.255.255.0")), // matches /24 AND /16
        ];
        assert_eq!(allow.select_interface(&candidates), Some(1));
    }

    #[test]
    fn single_homed_box_with_matching_allowlist_selects_its_only_interface() {
        // Single-homed box whose one NIC is on the trusted subnet — selected
        // (the same NIC the default path would pick anyway; byte-identical net).
        let allow = GmAllowlist::parse(&["10.77.9.0/24".to_string()]);
        let candidates = [(ip("10.77.9.202"), nm("255.255.255.0"))];
        assert_eq!(allow.select_interface(&candidates), Some(0));
    }

    #[test]
    fn exact_tie_keeps_the_first_listed_candidate_deterministically() {
        // Review 🟡: the "deterministic across restarts" tie-break was untested,
        // and a `>` -> `>=` mutant in the selection would survive. Two interfaces
        // with the SAME (trusted-prefix, interface-prefix) key -> the first-listed
        // MUST win, and both must be reported as an ambiguous tie.
        let allow = GmAllowlist::parse(&["10.77.0.0/16".to_string()]);
        let candidates = [
            (ip("10.77.9.204"), nm("255.255.255.0")), // both inside 10.77.0.0/16
            (ip("10.77.7.204"), nm("255.255.255.0")), // same (gm_len=16, if_len=24)
        ];
        assert_eq!(
            allow.select_interface(&candidates),
            Some(0),
            "an exact tie must keep the first-listed candidate"
        );
        assert_eq!(
            allow.best_interface_matches(&candidates),
            vec![0, 1],
            "both tied interfaces must be reported so the caller can detect ambiguity"
        );
    }

    #[test]
    fn interface_prefix_secondary_tiebreak_beats_a_wide_mask_nic() {
        // Review 🟡: the SECONDARY interface-prefix-length tie-break is load-bearing.
        // With an exact-GM /32 entry, a stray wide-mask NIC (10.1.2.3/8) also
        // "overlaps" at gm_len=32 (top 8 bits agree), so ONLY the longer interface
        // prefix keeps the rig /24 NIC winning. This is what a `>` -> `>=` or a
        // dropped secondary key would break.
        let allow = GmAllowlist::parse(&["10.77.9.184".to_string()]); // exact GM /32
        let candidates = [
            (ip("10.1.2.3"), nm("255.0.0.0")), // /8 — overlaps at gm_len=32, if_len=8
            (ip("10.77.9.204"), nm("255.255.255.0")), // rig /24 — gm_len=32, if_len=24
        ];
        assert_eq!(
            allow.select_interface(&candidates),
            Some(1),
            "the longer interface prefix (rig /24) must beat the wide /8 NIC"
        );
        assert_eq!(
            allow.best_interface_matches(&candidates),
            vec![1],
            "the rig /24 is a UNIQUE winner — no ambiguity"
        );
    }

    #[test]
    fn broad_allowlist_spanning_two_subnets_is_reported_as_ambiguous() {
        // Review 🟡: an over-broad /16 that spans BOTH the rig and mbc subnets can
        // no longer silently let pcap enumeration order decide — best_interface_matches
        // returns BOTH so find_ptp_capture_device keeps the default interface.
        let allow = GmAllowlist::parse(&["10.77.0.0/16".to_string()]);
        let candidates = [
            (ip("10.77.7.204"), nm("255.255.255.0")), // mbc
            (ip("10.77.9.204"), nm("255.255.255.0")), // rig
        ];
        let matches = allow.best_interface_matches(&candidates);
        assert_eq!(
            matches.len(),
            2,
            "an ambiguous broad allowlist reports both"
        );
    }
}
