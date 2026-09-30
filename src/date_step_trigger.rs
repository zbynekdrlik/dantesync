//! dantesync#126 — the coordinated date step ON REQUEST, for acceptance tests.
//!
//! During the day the fleet date could only be stepped by restarting the NTP master — which was
//! exactly the UNCOORDINATED path this issue removes. So the master takes a request: `POST
//! /date/step` on its :8898 status server (`crate::http_status`), from LOOPBACK only (the server
//! binds 0.0.0.0 for the read-only status; a step is an action on the whole fleet). The HTTP thread
//! hands the request to the sync loop over a channel ([`DateStepRequest`]); the loop's date
//! authority announces the current UTC error as ONE coordinated step, `2 × step_lead_ms` ahead,
//! exactly like the nightly window (`crate::date_offset::DateAuthority::step_now`), and answers
//! with a [`DateStepOutcome`].
//!
//! The answer is `202` + `{"accepted":true,…}` only when a step was announced; a refusal is a `4xx`
//! with `{"accepted":false,"reason":…}`, and a `503` (no answer in time) guarantees nothing was
//! announced: the request is CLAIMED once, either by the loop (which then acts and answers, however
//! long it takes) or by the HTTP side giving up (the loop then never acts on it). The request must
//! carry the `X-DanteSync-Step` header, which no web page can send cross-origin without a preflight
//! this server never answers, and a loopback `Host` (a DNS-rebound page names its own host). An
//! older build ignores the route and answers `200` with the status JSON, so a caller keys on
//! `"accepted"`, never on the status code alone.

use serde_json::json;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// The route.
pub const DATE_STEP_PATH: &str = "/date/step";

/// How long the HTTP side waits for the sync loop to take a request (it polls every 1 ms / 50 µs;
/// an NTP burst or a clock step can block it for seconds). Not taken by then, the request is
/// abandoned — the loop never acts on it — and the answer is `503`.
pub const DATE_STEP_REPLY_TIMEOUT: Duration = Duration::from_secs(3);

/// The header a step request must carry (any value), lower case.
pub const DATE_STEP_HEADER: &str = "x-dantesync-step";

/// What the sync loop did with a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DateStepOutcome {
    /// ONE coordinated step of `amount_ns` (forward) is announced, taking effect at the PTP
    /// instant `land_ptp_ns`, `due_in_ms` from now, as the authority's `seq`.
    Accepted {
        amount_ns: i64,
        land_ptp_ns: i64,
        due_in_ms: i64,
        seq: u32,
    },
    /// Nothing announced, and why.
    Refused { reason: String },
}

const CLAIM_PENDING: u8 = 0;
const CLAIM_TAKEN: u8 = 1;
const CLAIM_ABANDONED: u8 = 2;

/// One request from the HTTP route to the sync loop, which answers on `reply`. It is claimed
/// exactly once: [`take`](Self::take) by the loop (it acts, then answers) or
/// [`abandon`](Self::abandon) by the HTTP side (then nothing is ever announced for it) — so a 503
/// always means "nothing announced", whatever the loop's timing (review rounds 1-2).
pub struct DateStepRequest {
    pub reply: mpsc::Sender<DateStepOutcome>,
    claim: Arc<AtomicU8>,
}

impl DateStepRequest {
    /// A pending request, and the HTTP side's handle on its claim.
    pub fn new(reply: mpsc::Sender<DateStepOutcome>) -> (Self, Arc<AtomicU8>) {
        let claim = Arc::new(AtomicU8::new(CLAIM_PENDING));
        (
            DateStepRequest {
                reply,
                claim: claim.clone(),
            },
            claim,
        )
    }

    /// The loop claims the request: true = act on it (and answer); false = the HTTP side
    /// abandoned it (answered 503), so nothing may be announced.
    pub fn take(&self) -> bool {
        self.claim
            .compare_exchange(
                CLAIM_PENDING,
                CLAIM_TAKEN,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// The HTTP side gives up on `claim`: true = abandoned, nothing will be announced (answer
    /// 503); false = the loop has already taken it, so its answer must be waited for.
    pub fn abandon(claim: &AtomicU8) -> bool {
        claim
            .compare_exchange(
                CLAIM_PENDING,
                CLAIM_ABANDONED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }
}

/// The channel from the HTTP route (the sender, one clone per connection) to the sync loop.
pub fn channel() -> (
    mpsc::Sender<DateStepRequest>,
    mpsc::Receiver<DateStepRequest>,
) {
    mpsc::channel()
}

/// Which handler a request goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// Everything but the step route: the status JSON, as before 1.15 (any method, any path).
    Status,
    /// `POST /date/step`.
    DateStep,
    /// Another method on `/date/step` (405).
    DateStepWrongMethod,
}

/// Route a raw request by its request line (`METHOD PATH HTTP/x`); a query string is ignored.
/// Anything unreadable is the status route, as it always was.
pub fn route(request: &[u8]) -> Route {
    let text = String::from_utf8_lossy(request);
    let line = text.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Route::Status;
    };
    let path = target.split('?').next().unwrap_or(target);
    if path.trim_end_matches('/') != DATE_STEP_PATH {
        return Route::Status;
    }
    if method.eq_ignore_ascii_case("POST") {
        Route::DateStep
    } else {
        Route::DateStepWrongMethod
    }
}

/// The request's header lines (between the request line and the blank line) as (name, value);
/// a folded continuation line (leading whitespace) is not a header of its own.
fn headers(request: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(request);
    text.lines()
        .skip(1)
        .take_while(|l| !l.is_empty())
        .filter(|l| !l.starts_with([' ', '\t']))
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect()
}

/// Does the request carry the [`DATE_STEP_HEADER`] header (the name case-insensitive)?
pub fn has_step_header(request: &[u8]) -> bool {
    headers(request).iter().any(|(n, _)| n == DATE_STEP_HEADER)
}

/// Is the request's `Host` a loopback name (`127.0.0.1`, `localhost`, `[::1]`, with or without a
/// port)? A page served through a DNS-rebound name sends that name, so it is refused (review
/// round 2). No `Host` at all is refused too.
pub fn has_loopback_host(request: &[u8]) -> bool {
    let hs = headers(request);
    let Some((_, host)) = hs.iter().find(|(n, _)| n == "host") else {
        return false;
    };
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.split(':').next().unwrap_or("")
    };
    name.eq_ignore_ascii_case("localhost") || name == "::1" || name == "127.0.0.1"
}

/// Only a loopback peer may request a step (`127.0.0.0/8`, `::1`, or an IPv4-mapped loopback).
pub fn peer_allowed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// The HTTP status code and the JSON body of an outcome: `202` when announced, `409` refused.
pub fn outcome_response(outcome: &DateStepOutcome) -> (u16, String) {
    match outcome {
        DateStepOutcome::Accepted {
            amount_ns,
            land_ptp_ns,
            due_in_ms,
            seq,
        } => (
            202,
            json!({
                "accepted": true,
                "amount_ns": amount_ns,
                "land_ptp_ns": land_ptp_ns,
                "due_in_ms": due_in_ms,
                "seq": seq,
            })
            .to_string(),
        ),
        DateStepOutcome::Refused { reason } => (409, refusal_body(reason)),
    }
}

/// `{"accepted":false,"reason":…}` — the body of every refusal (403, 405, 409, 503).
pub fn refusal_body(reason: &str) -> String {
    json!({ "accepted": false, "reason": reason }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn only_post_date_step_is_the_step_route_126() {
        let cases: [(&[u8], Route); 9] = [
            (
                b"POST /date/step HTTP/1.1\r\nHost: x\r\n\r\n",
                Route::DateStep,
            ),
            (b"post /date/step/ HTTP/1.0\r\n\r\n", Route::DateStep),
            (b"POST /date/step?now=1 HTTP/1.1\r\n\r\n", Route::DateStep),
            (
                b"GET /date/step HTTP/1.1\r\n\r\n",
                Route::DateStepWrongMethod,
            ),
            (
                b"PUT /date/step HTTP/1.1\r\n\r\n",
                Route::DateStepWrongMethod,
            ),
            (b"GET /status HTTP/1.1\r\n\r\n", Route::Status),
            (b"POST /status HTTP/1.1\r\n\r\n", Route::Status),
            (b"POST /date/stepper HTTP/1.1\r\n\r\n", Route::Status),
            (b"", Route::Status),
        ];
        for (raw, want) in cases {
            assert_eq!(route(raw), want, "{:?}", String::from_utf8_lossy(raw));
        }
        // Garbage (not UTF-8, no request line) stays the status route.
        assert_eq!(route(&[0xff, 0xfe, 0x00]), Route::Status);
    }

    #[test]
    fn a_step_request_must_carry_the_step_header_126() {
        assert!(has_step_header(
            b"POST /date/step HTTP/1.1\r\nHost: x\r\nX-DanteSync-Step: 1\r\n\r\n"
        ));
        assert!(has_step_header(
            b"POST /date/step HTTP/1.1\r\nx-dantesync-step:yes\r\n\r\n"
        ));
        // Not a header: absent, only in the request line, or after the headers end.
        assert!(!has_step_header(
            b"POST /date/step HTTP/1.1\r\nHost: x\r\n\r\n"
        ));
        assert!(!has_step_header(
            b"POST /x-dantesync-step:1 HTTP/1.1\r\nHost: x\r\n\r\n"
        ));
        assert!(!has_step_header(
            b"POST /date/step HTTP/1.1\r\nHost: x\r\n\r\nX-DanteSync-Step: 1"
        ));
        assert!(!has_step_header(b""));
    }

    #[test]
    fn a_step_request_must_name_a_loopback_host_126() {
        for host in [
            "127.0.0.1:8898",
            "127.0.0.1",
            "localhost:8898",
            "LOCALHOST",
            "[::1]:8898",
        ] {
            let raw = format!("POST /date/step HTTP/1.1\r\nHost: {host}\r\n\r\n");
            assert!(has_loopback_host(raw.as_bytes()), "{host}");
        }
        for host in [
            "strih.lan:8898",
            "10.77.9.202",
            "evil.example:8898",
            "127.0.0.1.nip.io",
        ] {
            let raw = format!("POST /date/step HTTP/1.1\r\nHost: {host}\r\n\r\n");
            assert!(!has_loopback_host(raw.as_bytes()), "{host}");
        }
        assert!(
            !has_loopback_host(b"POST /date/step HTTP/1.0\r\n\r\n"),
            "no Host"
        );
        // A folded continuation line is not a header of its own.
        assert!(!has_step_header(
            b"POST /date/step HTTP/1.1\r\nX-Other: a\r\n X-DanteSync-Step: 1\r\n\r\n"
        ));
    }

    #[test]
    fn a_request_is_claimed_once_by_the_loop_or_by_the_http_side_giving_up_126() {
        let (tx, _rx) = mpsc::channel();
        let (req, claim) = DateStepRequest::new(tx.clone());
        assert!(req.take(), "the loop takes a pending request");
        assert!(
            !DateStepRequest::abandon(&claim),
            "taken: its answer must be awaited"
        );
        assert!(!req.take(), "claimed once");
        let (req, claim) = DateStepRequest::new(tx);
        assert!(
            DateStepRequest::abandon(&claim),
            "the HTTP side gives up first"
        );
        assert!(!req.take(), "abandoned: the loop never acts on it");
    }

    #[test]
    fn only_a_loopback_peer_may_request_a_step_126() {
        assert!(peer_allowed(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(peer_allowed(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2))));
        assert!(peer_allowed(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(peer_allowed(IpAddr::V6(
            Ipv4Addr::LOCALHOST.to_ipv6_mapped()
        )));
        for ip in [
            IpAddr::V4(Ipv4Addr::new(10, 77, 9, 202)),
            IpAddr::V4(Ipv4Addr::new(100, 104, 8, 125)),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(Ipv4Addr::new(10, 77, 9, 202).to_ipv6_mapped()),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        ] {
            assert!(!peer_allowed(ip), "{ip}");
        }
    }

    #[test]
    fn accepted_is_202_with_accepted_true_and_a_refusal_409_with_the_reason_126() {
        let (code, body) = outcome_response(&DateStepOutcome::Accepted {
            amount_ns: 247_297_000,
            land_ptp_ns: 1_234_567_890,
            due_in_ms: 9_998,
            seq: 5,
        });
        assert_eq!(code, 202);
        let v: serde_json::Value = serde_json::from_str(&body).expect("JSON");
        assert_eq!(v["accepted"], true);
        assert_eq!(v["amount_ns"], 247_297_000);
        assert_eq!(v["land_ptp_ns"], 1_234_567_890_i64);
        assert_eq!(v["due_in_ms"], 9_998);
        assert_eq!(v["seq"], 5);

        let (code, body) = outcome_response(&DateStepOutcome::Refused {
            reason: "a \"quoted\" reason".to_string(),
        });
        assert_eq!(code, 409);
        let v: serde_json::Value = serde_json::from_str(&body).expect("JSON, escaped");
        assert_eq!(v["accepted"], false);
        assert_eq!(v["reason"], "a \"quoted\" reason");
    }
}
