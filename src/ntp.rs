use anyhow::{anyhow, Result};
use rsntp::SntpClient;
use std::time::Duration;

// ============================================================================
// #53 — NTP measurement burst-filtering (pure selection/summary functions)
// ============================================================================
// A SINGLE raw NTP round trip is contaminated by userspace scheduling jitter
// (a delayed thread wakeup on either end stretches the measured offset). On a
// heavily-loaded Windows box this scatters the reported offset by tens of
// milliseconds even while the PTP servo reports locked — see dantesync#53.
//
// Fix (wired into `NtpClient::get_offset()` in the GREEN commit): take a
// BURST of raw measurements per check, keep only the lowest-round-trip-delay
// subset (least likely to have been stretched by a scheduling stall), and
// publish the MEDIAN of that accepted subset instead of the last raw sample.
// The spread across the accepted subset — and how many samples backed the
// median — are surfaced alongside the value so a badly-measured node stays
// visibly bad even after filtering (never smoothed into looking good).
//
// These are pure functions on already-collected samples, deliberately kept
// free of any I/O so they are unit-testable with fixture data and never need
// a live box or a real NTP server.
// ============================================================================

/// Number of raw NTP measurements taken per check (the "burst").
///
/// Configurable by editing this constant — matching this file's existing
/// tunable-const convention elsewhere in the repo (controller.rs's
/// `NTP_CHECK_INTERVAL_SECS` / `NTP_SAMPLE_COUNT`) rather than adding a new JSON
/// config surface for a single value (MVP philosophy: no complexity for an
/// unused knob). Five sequential blocking UDP round trips (each typically
/// single-digit-to-low-tens-of-ms on a LAN) inside a check that already only
/// runs every 10-30s (`calculate_adaptive_ntp_interval` in controller.rs) is
/// negligible added cost.
pub const NTP_BURST_SIZE: usize = 5;

/// How many of the burst's lowest-round-trip-delay samples are "accepted" and
/// fed into the published median. 3-of-5 mirrors controller.rs's existing
/// `NTP_SAMPLE_COUNT = 5` "samples needed for a reliable median" convention (a
/// different ring, same principle).
pub const NTP_BURST_ACCEPT_N: usize = 3;

/// One raw NTP measurement from a single blocking round trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawSample {
    /// Signed clock offset (local + offset = true time), whole microseconds.
    pub offset_us: i64,
    /// Round-trip delay for this measurement, whole microseconds (always >= 0).
    pub rtt_us: u64,
}

/// The published result of filtering a burst of `RawSample`s down to one value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilteredOffset {
    /// Median offset (microseconds) over the accepted (lowest-RTT) samples.
    pub offset_us: i64,
    /// Spread (max - min, microseconds) across the SAME accepted samples. This
    /// is the quality signal: a genuinely well-measured node has a small spread
    /// even after RTT selection; a badly-measured one still shows a large
    /// spread here even though `offset_us` is a single defined number. NEVER
    /// read `offset_us` alone as "the node is fine" — check `spread_us` too
    /// (dantesync#53 — filtering must never hide instability).
    pub spread_us: u64,
    /// How many samples fed the median (<= `NTP_BURST_ACCEPT_N`; lower after a
    /// partial burst failure — also a reason to trust the value less).
    pub sample_count: usize,
}

/// Select the `n` samples with the lowest round-trip delay (pure, no I/O).
///
/// The lowest-RTT samples are the least likely to have been stretched by a
/// scheduling stall on either end — the standard NTP "minimum delay" selection
/// technique. Returns fewer than `n` if `samples` is shorter.
pub fn select_lowest_rtt(samples: &[RawSample], n: usize) -> Vec<RawSample> {
    // #53 GREEN: a real RTT-ascending sort, then keep the lowest n. `sort_by_key`
    // is stable, so equal-RTT samples keep their original relative order.
    let mut sorted: Vec<RawSample> = samples.to_vec();
    sorted.sort_by_key(|s| s.rtt_us);
    sorted.truncate(n);
    sorted
}

/// Median + spread over a slice of ALREADY-SELECTED ("accepted") offsets (pure).
/// Returns `None` for an empty slice.
pub fn summarize_offsets(offsets_us: &[i64]) -> Option<FilteredOffset> {
    if offsets_us.is_empty() {
        return None;
    }
    // #53 GREEN: a real median (sorted[len/2] — same "upper of two" convention
    // as controller.rs's calculate_ntp_adaptive_threshold, for consistency) and
    // a real max-min spread across the SAME sorted slice. The spread is what
    // keeps this honest: a wide spread survives the filtering and is reported
    // as-is, never collapsed toward 0 just because a single median exists.
    let mut sorted: Vec<i64> = offsets_us.to_vec();
    sorted.sort();
    let median = sorted[sorted.len() / 2];
    let spread = (sorted[sorted.len() - 1] - sorted[0]).unsigned_abs();

    Some(FilteredOffset {
        offset_us: median,
        spread_us: spread,
        sample_count: offsets_us.len(),
    })
}

/// RTT-select the lowest `accept_n` of `samples`, then median+spread them (pure).
pub fn filter_offset(samples: &[RawSample], accept_n: usize) -> Option<FilteredOffset> {
    let accepted = select_lowest_rtt(samples, accept_n);
    let offsets: Vec<i64> = accepted.iter().map(|s| s.offset_us).collect();
    summarize_offsets(&offsets)
}

/// A published NTP measurement: the offset/sign pair `SystemClock::step_clock`
/// and the #50 step-agreement gate need, PLUS the quality fields (dantesync#53)
/// that let a consumer tell a well-measured node from a badly-measured one
/// instead of the filtering silently hiding instability.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NtpMeasurement {
    pub offset: Duration,
    pub sign: i8,
    /// See `FilteredOffset::spread_us`.
    pub spread_us: u64,
    /// See `FilteredOffset::sample_count`.
    pub sample_count: usize,
    /// dantesync#53 continuation: whether the kernel-timestamped Npcap
    /// transport was used for EVERY sample in this burst (never smoothed —
    /// `false` the instant even one sample fell back to userspace rsntp).
    /// Surfaces the silent-degradation failure mode confirmed live on the
    /// stream box: the transport can fail at construction (dual-homed host,
    /// NTP server unreachable from the PTP capture NIC) and nothing after
    /// startup ever showed it again.
    pub pcap_active: bool,
}

pub struct NtpClient {
    server: String,
    // dantesync#53: on Windows, a persistent kernel-timestamped (Npcap)
    // transport, opened once at construction. `None` means either this
    // isn't Windows, or Npcap init failed (loudly logged in `new()`) — in
    // both cases `measure_once()` falls back to the `rsntp` path below.
    #[cfg(windows)]
    pcap_transport: std::sync::Mutex<Option<crate::net_pcap::PcapNtpTransport>>,
}

impl NtpClient {
    /// dantesync#53 continuation: no longer takes an `interface_name` — the
    /// Npcap NTP capture device (Windows only) is now selected by which
    /// interface can actually REACH `server`, not by inheriting the PTP
    /// capture interface (which fails outright on a dual-homed host where
    /// PTP and NTP live on different subnets — confirmed live on the stream
    /// box). See `net_pcap::find_device_for_ntp_server`.
    pub fn new(server: &str) -> Self {
        #[cfg(windows)]
        {
            let pcap_transport = match Self::init_pcap_transport(server) {
                Ok(t) => Some(t),
                Err(e) => {
                    log::warn!(
                        "[NTP] Npcap kernel-timestamped transport unavailable ({}), falling back \
                         to userspace rsntp for ALL samples — offset scatter is the KNOWN defect \
                         this transport exists to fix (dantesync#53)",
                        e
                    );
                    None
                }
            };
            NtpClient {
                server: server.to_string(),
                pcap_transport: std::sync::Mutex::new(pcap_transport),
            }
        }
        #[cfg(not(windows))]
        {
            NtpClient {
                server: server.to_string(),
            }
        }
    }

    #[cfg(windows)]
    fn init_pcap_transport(server: &str) -> Result<crate::net_pcap::PcapNtpTransport> {
        let server_ip = Self::resolve_ipv4(server)?;
        crate::net_pcap::PcapNtpTransport::new(server_ip)
    }

    #[cfg(windows)]
    fn resolve_ipv4(server: &str) -> Result<std::net::Ipv4Addr> {
        use std::net::ToSocketAddrs;
        (server, 123u16)
            .to_socket_addrs()?
            .find_map(|a| match a {
                std::net::SocketAddr::V4(v4) => Some(*v4.ip()),
                _ => None,
            })
            .ok_or_else(|| anyhow!("could not resolve '{}' to an IPv4 address", server))
    }

    /// One measurement, preferring the kernel-timestamped Npcap transport on
    /// Windows (dantesync#53) and falling back to the userspace `rsntp` path
    /// — on Linux always, on Windows only when Npcap init failed at
    /// construction or a single round trip fails transiently (logged either
    /// way; never a silent fallback). The `bool` reports whether THIS sample
    /// was satisfied by the pcap transport (dantesync#53 continuation:
    /// `pcap_ntp_active` status signal) — always `false` on non-Windows,
    /// where no such transport exists.
    fn measure_once(&self) -> (Result<RawSample>, bool) {
        #[cfg(windows)]
        {
            let mut guard = self
                .pcap_transport
                .lock()
                .expect("ntp pcap transport mutex poisoned");
            if let Some(transport) = guard.as_mut() {
                match transport.measure_once() {
                    Ok(sample) => return (Ok(sample), true),
                    Err(e) => {
                        log::warn!(
                            "[NTP] Npcap round trip failed ({}), falling back to rsntp for this \
                             sample",
                            e
                        );
                    }
                }
            }
        }
        (self.measure_once_rsntp(), false)
    }

    /// One raw blocking SNTP round trip. `rsntp` 4.1.2's `SynchronizationResult`
    /// exposes only the DERIVED `clock_offset()`/`round_trip_delay()` — no raw
    /// t1..t4 accessors (confirmed by reading its source, `result.rs`) — so
    /// those two derived quantities are what get measured/logged per sample.
    ///
    /// This is the ORIGINAL measurement path (unchanged since before #53's
    /// kernel-timestamp work) — still used on every platform when the
    /// Npcap transport above isn't available, and always on Linux, where
    /// this path is already precise (5-32us spread observed live).
    fn measure_once_rsntp(&self) -> Result<RawSample> {
        let client = SntpClient::new();
        let result = client.synchronize(&self.server)?;

        let offset_us = (result.clock_offset().as_secs_f64() * 1_000_000.0).round() as i64;
        let rtt_us = (result.round_trip_delay().as_secs_f64() * 1_000_000.0)
            .abs()
            .round() as u64;

        log::debug!("[NTP] raw sample offset:{:+}us rtt:{}us", offset_us, rtt_us);

        Ok(RawSample { offset_us, rtt_us })
    }

    /// Fetches the current offset from the NTP server as a burst-filtered,
    /// RTT-selected measurement (dantesync#53) instead of a single unfiltered
    /// round trip.
    ///
    /// Returns the offset required to apply to the local system time
    /// (Local + Offset = True Time). Positive offset means local clock is
    /// behind (needs to step forward).
    pub fn get_offset(&self) -> Result<NtpMeasurement> {
        let mut raw = Vec::with_capacity(NTP_BURST_SIZE);
        let mut via_pcap_flags = Vec::with_capacity(NTP_BURST_SIZE);
        let mut last_err = None;
        for _ in 0..NTP_BURST_SIZE {
            let (result, via_pcap) = self.measure_once();
            match result {
                Ok(sample) => {
                    raw.push(sample);
                    via_pcap_flags.push(via_pcap);
                }
                Err(e) => last_err = Some(e),
            }
        }

        let discarded = raw.len().saturating_sub(NTP_BURST_ACCEPT_N.min(raw.len()));
        let filtered = filter_offset(&raw, NTP_BURST_ACCEPT_N)
            .ok_or_else(|| last_err.unwrap_or_else(|| anyhow!("NTP burst: no server response")))?;

        // dantesync#53 continuation: pure, Linux-testable aggregation --
        // `true` only when EVERY sample in the burst used the kernel-
        // timestamped pcap transport (never smoothed away by a partial
        // fallback).
        let pcap_active = crate::ntp_packet::burst_used_pcap_throughout(&via_pcap_flags);

        log::info!(
            "[NTP] burst offset:{:+}us spread:{}us samples:{}/{} (discarded {}) pcap_active:{}",
            filtered.offset_us,
            filtered.spread_us,
            filtered.sample_count,
            NTP_BURST_SIZE,
            discarded,
            pcap_active,
        );

        let sign: i8 = if filtered.offset_us < 0 { -1 } else { 1 };
        let offset = Duration::from_micros(filtered.offset_us.unsigned_abs());

        Ok(NtpMeasurement {
            offset,
            sign,
            spread_us: filtered.spread_us,
            sample_count: filtered.sample_count,
            pcap_active,
        })
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::{
        filter_offset, select_lowest_rtt, summarize_offsets, NtpClient, RawSample,
        NTP_BURST_ACCEPT_N,
    };

    // Note: the old `test_offset_to_duration_*` tests (dropped here, #53) only
    // mirrored the sign/Duration-split arithmetic inline — they never called a
    // real production function, and that arithmetic no longer exists in
    // `NtpClient::get_offset()` (replaced by the burst+filter pipeline tested
    // below, which converts the already-filtered `offset_us` i64 directly via
    // `Duration::from_micros`). Superseded by the fixture tests on the real
    // pure functions, which is strictly more coverage of real behavior.

    #[test]
    fn test_ntp_client_new() {
        let client = NtpClient::new("pool.ntp.org");
        assert_eq!(client.server, "pool.ntp.org");
    }

    /// #53 RED: `select_lowest_rtt` must pick the samples with the SMALLEST
    /// round-trip delay, regardless of what order they arrived in the burst.
    /// The placeholder body just truncates in arrival order, so this fails
    /// against a fixture where the low-RTT samples are NOT the first ones.
    #[test]
    fn select_lowest_rtt_picks_the_n_lowest_rtt_samples() {
        let samples = [
            RawSample {
                offset_us: 9000,
                rtt_us: 200,
            }, // high-RTT outlier, arrives FIRST
            RawSample {
                offset_us: 100,
                rtt_us: 5,
            },
            RawSample {
                offset_us: 105,
                rtt_us: 6,
            },
            RawSample {
                offset_us: 98,
                rtt_us: 4,
            },
            RawSample {
                offset_us: -8000,
                rtt_us: 250,
            }, // high-RTT outlier
        ];

        let selected = select_lowest_rtt(&samples, 3);

        let mut offsets: Vec<i64> = selected.iter().map(|s| s.offset_us).collect();
        offsets.sort();
        assert_eq!(
            offsets,
            vec![98, 100, 105],
            "must keep the 3 lowest-RTT samples (rtt 4,5,6us), not the first 3 in arrival order"
        );
    }

    /// #53 RED: `summarize_offsets` must return the TRUE median plus the honest
    /// spread, not the last raw sample with a hardcoded zero spread.
    #[test]
    fn summarize_offsets_returns_true_median_and_honest_spread() {
        let offsets = [10, -5, 1000];

        let summary = summarize_offsets(&offsets).expect("non-empty input");

        assert_eq!(summary.offset_us, 10, "median of [-5, 10, 1000] is 10");
        assert_eq!(
            summary.spread_us, 1005,
            "spread must be max - min = 1000 - (-5)"
        );
        assert_eq!(summary.sample_count, 3);
    }

    /// #53 RED: the pathological 22-sample scatter FROM THE ISSUE ITSELF
    /// (-18750..+22860us, taken as consecutive raw `ntp_offset_us` readings)
    /// must stay HONEST: the published median is a single determinate value,
    /// but `spread_us` must still report the full ~41.6ms disagreement —
    /// filtering a bad node must never make it LOOK good.
    #[test]
    fn summarize_offsets_pathological_scatter_from_53_stays_honest() {
        let offsets = [
            -15913, -18750, 3982, 19344, 632, 21205, 7737, 10481, 2014, 2014, 22860, 4784, 19515,
            4223, 7482, 7482, 22421, 5404, 1998, 10040, 14862, 14862,
        ];

        let summary = summarize_offsets(&offsets).expect("non-empty input");

        assert_eq!(summary.sample_count, 22);
        assert_eq!(
            summary.offset_us, 7482,
            "median of the sorted 22-sample fixture"
        );
        assert_eq!(
            summary.spread_us, 41610,
            "spread must expose the full -18750..+22860 scatter (41610us), never hide it as 0"
        );
    }

    /// #53 RED: the full pipeline — RTT-select then median+spread — must
    /// discard the high-RTT outliers before computing the published value.
    #[test]
    fn filter_offset_discards_high_rtt_outliers_before_summarizing() {
        let samples = [
            RawSample {
                offset_us: 9000,
                rtt_us: 200,
            },
            RawSample {
                offset_us: 100,
                rtt_us: 5,
            },
            RawSample {
                offset_us: 105,
                rtt_us: 6,
            },
            RawSample {
                offset_us: 98,
                rtt_us: 4,
            },
            RawSample {
                offset_us: -8000,
                rtt_us: 250,
            },
        ];

        let filtered = filter_offset(&samples, 3).expect("non-empty input");

        assert_eq!(
            filtered.offset_us, 100,
            "median of the accepted [98,100,105]"
        );
        assert_eq!(filtered.spread_us, 7, "spread of the accepted subset only");
        assert_eq!(filtered.sample_count, 3);
    }

    #[test]
    fn summarize_offsets_empty_is_none() {
        assert!(summarize_offsets(&[]).is_none());
    }

    #[test]
    fn filter_offset_empty_samples_is_none() {
        assert!(filter_offset(&[], NTP_BURST_ACCEPT_N).is_none());
    }

    /// #53 GREEN: end-to-end honesty check from RAW t1..t4 timestamps (the
    /// kernel-timestamped `PcapNtpTransport` transport's own output shape)
    /// through to the published quality fields.
    ///
    /// The earlier pathological-scatter test above starts from
    /// already-known offsets; this one starts one layer further back — from
    /// synthetic (t1,t2,t3,t4) quadruples run through the SAME
    /// `compute_offset_rtt_us` formula `PcapNtpTransport::measure_once()`
    /// uses — to prove the honesty guarantee survives the new measurement
    /// path too: even samples that are genuinely computed from 4 real
    /// timestamps (not just handed a pre-baked offset), all with EQUAL
    /// round-trip delay (so RTT-selection cannot distinguish or discard any
    /// of them), still surface the full ~41.6ms scatter from the issue's own
    /// extremes (-18750us and +22860us) rather than being smoothed into a
    /// falsely-clean median. Kernel timestamps fix WHERE the noise comes
    /// from (userspace scheduling jitter); they do not, and must not,
    /// change whether genuine remaining disagreement gets hidden.
    #[test]
    fn pcap_style_t1_to_t4_measurements_stay_honest_through_the_existing_filter() {
        use crate::ntp_packet::compute_offset_rtt_us;

        // Every quadruple shares t1=0, t4=2000 (rtt=2000us, server dwell=0)
        // so all 5 samples tie on round-trip delay -- `select_lowest_rtt`'s
        // stable sort keeps arrival order on a tie, so the first 3 in this
        // list are exactly the "accepted" subset.
        let target_offsets_us = [-18750i64, 22860, 2014, 100, 100];
        let raw: Vec<RawSample> = target_offsets_us
            .iter()
            .map(|&offset_us| {
                let t1 = 0i64;
                let t4 = 2000i64;
                // offset = ((t2-t1)+(t3-t4))/2 with t2==t3 (zero server dwell)
                // solves to t2 = t3 = offset_us + (t4 - t1) / 2.
                let t2 = offset_us + (t4 - t1) / 2;
                let t3 = t2;
                let (offset_us_computed, rtt_us) = compute_offset_rtt_us(t1, t2, t3, t4);
                assert_eq!(
                    offset_us_computed, offset_us,
                    "fixture construction sanity check"
                );
                RawSample {
                    offset_us: offset_us_computed,
                    rtt_us: rtt_us.max(0) as u64,
                }
            })
            .collect();

        let filtered = filter_offset(&raw, NTP_BURST_ACCEPT_N).expect("non-empty input");

        assert_eq!(
            filtered.sample_count, NTP_BURST_ACCEPT_N,
            "3 of 5 tied-RTT samples must be accepted"
        );
        assert_eq!(
            filtered.offset_us, 2014,
            "median of the accepted [-18750, 22860, 2014] is 2014"
        );
        assert_eq!(
            filtered.spread_us, 41610,
            "spread must expose the full -18750..+22860 scatter (41610us) computed from raw \
             t1..t4 timestamps, never hidden by a clean-looking median"
        );
    }
}
