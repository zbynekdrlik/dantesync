//! DSCP (DiffServ) marking for dantesync's timesync UDP sockets (#52).
//!
//! # Why
//!
//! On a loaded venue LAN the switches queue best-effort UDP behind bulk
//! traffic, so dantesync's timesync packets pick up a variable, one-sided
//! queue delay — the measurement-path asymmetry the 2026-07-17/18 event
//! exhibited (masked in production by the step-agreement gate, but not
//! removed at the source). The venue MikroTik CRS switches honour DSCP in
//! hardware (TRUST-L3), and Dante already marks its own PTP clock traffic, so
//! stamping dantesync's timesync UDP with a high DiffServ code point lets the
//! switches prioritise it and removes the queue-delay bias at the source.
//!
//! # Coverage (honest split — see the module's tests + the ticket)
//!
//! setsockopt(IP_TOS) can only mark a socket dantesync owns a HANDLE to:
//!
//! | Path                                   | Platform | Marked?                                             |
//! |----------------------------------------|----------|-----------------------------------------------------|
//! | NTP server reply (`ntp_server`)        | Linux    | YES — effective (where server mode is enabled)      |
//! | NTP client request (`PcapNtpTransport`)| Windows  | Attempted; OS filters IP_TOS → needs a QoS policy   |
//! | NTP server reply                       | Windows  | Attempted; OS filters IP_TOS → needs a QoS policy   |
//! | NTP client request (`rsntp`)           | Linux    | NO handle (internal socket) → needs nftables mangle |
//! | PTP Sync/FollowUp                       | all      | inbound-only — nothing outbound to mark             |
//!
//! - **Linux NTP client** goes through `rsntp::SntpClient`, whose socket is
//!   created internally (no `setsockopt` handle) — the ticket's original
//!   blocker. That request direction is the residual gap and is meant to be
//!   complemented by an nftables mangle rule shipped at provisioning
//!   (`-p udp --dport 123 -j DSCP`).
//! - **Windows** filters a socket-set `IP_TOS` by default (the
//!   `DisableUserTOSSetting` registry gate, since XP SP2), so a socket-level
//!   mark is silently ignored by the stack. DSCP on the Windows boxes
//!   (strih/stream, NTP master + clients) requires a QoS policy
//!   (`New-NetQosPolicy` for UDP port 123) applied at provisioning. To avoid
//!   implying the socket mark works there, [`apply`] is a logged no-op on
//!   Windows rather than a call the OS discards.
//!
//! # TOS byte
//!
//! The IP header's TOS/DS octet is 8 bits: the DSCP code point occupies the
//! high 6 bits and ECN the low 2. So the byte programmed into `IP_TOS` is
//! `dscp << 2` with ECN left at 0. EF (46) → `0xB8`; CS7 (56) → `0xE0`.

use serde::{Deserialize, Serialize};

/// Default DSCP code point for timesync traffic: **EF (46)** — the DiffServ
/// "Expedited Forwarding" low-latency class. The ticket also proposes CS7 (56)
/// to co-locate timesync with Dante's own PTP clock queue; the value is a
/// config key (`dscp`) so a fleet can pick either without a code change.
pub const DEFAULT_DSCP: u8 = 46;

/// Largest valid DSCP code point — DSCP is a 6-bit field, so `0..=63`.
pub const MAX_DSCP: u8 = 63;

/// DSCP marking configuration (`system.dscp` in the JSON config).
///
/// Follows the `phase_slew` / `gm_allowlist` `#[serde(default)]` precedent: a
/// config that omits the whole object — or any field of it — still parses, and
/// marking is on-by-default with a per-box override. Marking is best-effort;
/// a bad value or a failing `set_tos` leaves the socket unmarked (exactly the
/// pre-#52 behaviour), so a config typo can never take the clock offline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DscpConfig {
    /// Mark timesync sockets with the DSCP code point below (default: true).
    #[serde(default = "default_dscp_enabled")]
    pub enabled: bool,
    /// DSCP code point to stamp (0..=63; default [`DEFAULT_DSCP`]). An
    /// out-of-range value fails open: marking is skipped with a warning.
    #[serde(default = "default_dscp_value")]
    pub dscp: u8,
}

fn default_dscp_enabled() -> bool {
    true
}

fn default_dscp_value() -> u8 {
    DEFAULT_DSCP
}

impl Default for DscpConfig {
    fn default() -> Self {
        Self {
            enabled: default_dscp_enabled(),
            dscp: default_dscp_value(),
        }
    }
}

/// The pure decision derived from a [`DscpConfig`] — what [`apply`] should do
/// with a socket. Separated from the syscall so the selection logic is
/// unit-testable without a socket or privileges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DscpDecision {
    /// Marking is disabled — leave the socket at best-effort.
    Disabled,
    /// The configured DSCP is out of the valid `0..=63` range — fail open:
    /// leave the socket unmarked and warn. Carries the offending value.
    Invalid(u8),
    /// Program `tos` (the `IP_TOS` byte) for code point `dscp`.
    Mark { dscp: u8, tos: u8 },
}

/// Convert a DSCP code point (`0..=63`) to the `IP_TOS` byte (`dscp << 2`,
/// ECN = 0). Returns `None` if `dscp` does not fit the 6-bit DSCP field.
pub fn dscp_to_tos(dscp: u8) -> Option<u8> {
    if dscp > MAX_DSCP {
        None
    } else {
        Some(dscp << 2)
    }
}

/// Pure: decide what to do with a socket for this config (no I/O).
pub fn decide(cfg: &DscpConfig) -> DscpDecision {
    if !cfg.enabled {
        return DscpDecision::Disabled;
    }
    match dscp_to_tos(cfg.dscp) {
        Some(tos) => DscpDecision::Mark {
            dscp: cfg.dscp,
            tos,
        },
        None => DscpDecision::Invalid(cfg.dscp),
    }
}

/// Apply the DSCP decision to a UDP socket (thin syscall seam). Never returns
/// an error: marking is best-effort — any failure is logged and the socket is
/// left unmarked, exactly the pre-#52 behaviour. `label` identifies the socket
/// in the log line (e.g. `"ntp-server reply"`).
pub fn apply(sock: &std::net::UdpSocket, cfg: &DscpConfig, label: &str) {
    match decide(cfg) {
        DscpDecision::Disabled => {
            log::info!("[DSCP] {}: marking disabled — best-effort", label);
        }
        DscpDecision::Invalid(v) => {
            log::warn!(
                "[DSCP] {}: invalid dscp {} (must be 0..={}) — sending unmarked",
                label,
                v,
                MAX_DSCP
            );
        }
        DscpDecision::Mark { dscp, tos } => apply_tos(sock, dscp, tos, label),
    }
}

#[cfg(unix)]
fn apply_tos(sock: &std::net::UdpSocket, dscp: u8, tos: u8, label: &str) {
    let sref = socket2::SockRef::from(sock);
    match sref.set_tos(tos as u32) {
        Ok(()) => log::info!(
            "[DSCP] {}: marked dscp={} (IP_TOS=0x{:02x})",
            label,
            dscp,
            tos
        ),
        Err(e) => log::warn!(
            "[DSCP] {}: set_tos(0x{:02x}) failed ({}) — sending unmarked",
            label,
            tos,
            e
        ),
    }
}

#[cfg(windows)]
fn apply_tos(_sock: &std::net::UdpSocket, dscp: u8, _tos: u8, label: &str) {
    // Windows filters a socket-set IP_TOS by default (DisableUserTOSSetting,
    // since XP SP2) — calling setsockopt(IP_TOS) here would return Ok yet mark
    // nothing, a silent lie. DSCP on Windows needs a QoS policy applied at
    // provisioning, so we log the guidance instead of pretending to mark.
    log::info!(
        "[DSCP] {}: socket-level marking not applied on Windows (IP_TOS is filtered by the OS); \
         configure a QoS policy instead (New-NetQosPolicy -IPProtocol UDP -IPDstPortStart 123 \
         -IPDstPortEnd 123 -DSCPAction {})",
        label,
        dscp
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dscp_to_tos_shifts_left_two_and_zeroes_ecn() {
        // EF (46) -> 46<<2 = 184 (0xB8); CS7 (56) -> 224 (0xE0); CS0 -> 0.
        assert_eq!(dscp_to_tos(46), Some(184));
        assert_eq!(dscp_to_tos(56), Some(224));
        assert_eq!(dscp_to_tos(0), Some(0));
        assert_eq!(dscp_to_tos(63), Some(252)); // max valid, 0xFC
    }

    #[test]
    fn dscp_to_tos_rejects_out_of_range() {
        assert_eq!(dscp_to_tos(64), None);
        assert_eq!(dscp_to_tos(255), None);
    }

    #[test]
    fn decide_disabled_when_off() {
        let cfg = DscpConfig {
            enabled: false,
            dscp: 46,
        };
        assert_eq!(decide(&cfg), DscpDecision::Disabled);
    }

    #[test]
    fn decide_marks_valid_enabled() {
        let cfg = DscpConfig {
            enabled: true,
            dscp: 46,
        };
        assert_eq!(decide(&cfg), DscpDecision::Mark { dscp: 46, tos: 184 });
    }

    #[test]
    fn decide_marks_cs7_alternative() {
        let cfg = DscpConfig {
            enabled: true,
            dscp: 56,
        };
        assert_eq!(decide(&cfg), DscpDecision::Mark { dscp: 56, tos: 224 });
    }

    #[test]
    fn decide_invalid_fails_open() {
        let cfg = DscpConfig {
            enabled: true,
            dscp: 200,
        };
        assert_eq!(decide(&cfg), DscpDecision::Invalid(200));
    }

    #[test]
    fn default_is_enabled_ef() {
        let cfg = DscpConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.dscp, DEFAULT_DSCP);
        assert_eq!(cfg.dscp, 46);
    }

    #[test]
    fn deserializes_from_empty_object_with_defaults() {
        // A config that names the object but no fields still parses to the
        // on-by-default EF marking (the #[serde(default)] partial-parse safety).
        let cfg: DscpConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg, DscpConfig::default());
    }

    #[test]
    fn deserializes_partial_enabled_only() {
        // Only `enabled` given → `dscp` defaults to EF.
        let cfg: DscpConfig = serde_json::from_str(r#"{"enabled": false}"#).unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.dscp, DEFAULT_DSCP);
    }

    #[test]
    fn deserializes_partial_dscp_only() {
        // Only `dscp` given (CS7) → `enabled` defaults to true.
        let cfg: DscpConfig = serde_json::from_str(r#"{"dscp": 56}"#).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.dscp, 56);
    }

    #[test]
    fn roundtrips_through_json() {
        let cfg = DscpConfig {
            enabled: true,
            dscp: 56,
        };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: DscpConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(cfg, back);
    }

    // The syscall seam itself: setting IP_TOS on an unprivileged ephemeral UDP
    // socket needs no privileges, so this verifies the real mark lands (Linux).
    #[cfg(unix)]
    #[test]
    fn apply_sets_ip_tos_on_a_real_socket() {
        use std::net::UdpSocket;
        let sock = UdpSocket::bind("127.0.0.1:0").expect("bind ephemeral udp");
        let cfg = DscpConfig {
            enabled: true,
            dscp: 46,
        };
        apply(&sock, &cfg, "test");
        let tos = socket2::SockRef::from(&sock).tos().expect("read back tos");
        assert_eq!(tos, 184); // 46 << 2
    }

    #[cfg(unix)]
    #[test]
    fn apply_disabled_leaves_socket_unmarked() {
        use std::net::UdpSocket;
        let sock = UdpSocket::bind("127.0.0.1:0").expect("bind ephemeral udp");
        let cfg = DscpConfig {
            enabled: false,
            dscp: 46,
        };
        apply(&sock, &cfg, "test");
        let tos = socket2::SockRef::from(&sock).tos().expect("read back tos");
        assert_eq!(tos, 0); // never marked
    }

    #[cfg(unix)]
    #[test]
    fn apply_invalid_dscp_leaves_socket_unmarked() {
        use std::net::UdpSocket;
        let sock = UdpSocket::bind("127.0.0.1:0").expect("bind ephemeral udp");
        let cfg = DscpConfig {
            enabled: true,
            dscp: 200,
        };
        apply(&sock, &cfg, "test");
        let tos = socket2::SockRef::from(&sock).tos().expect("read back tos");
        assert_eq!(tos, 0); // fail-open: unmarked
    }
}
