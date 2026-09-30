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
//! with `{"accepted":false,"reason":…}`. An older build ignores the route and answers `200` with the
//! status JSON, so a caller keys on `"accepted"`, never on the status code alone.

use serde_json::json;
use std::net::IpAddr;
use std::sync::mpsc;
use std::time::Duration;

/// The route.
pub const DATE_STEP_PATH: &str = "/date/step";

/// How long the HTTP thread waits for the sync loop's answer (the loop polls every 1 ms / 50 µs;
/// a loop blocked this long in an NTP query or a clock step is answered 503).
pub const DATE_STEP_REPLY_TIMEOUT: Duration = Duration::from_secs(3);

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

/// One request from the HTTP route to the sync loop, which answers on `reply`.
pub struct DateStepRequest {
    pub reply: mpsc::Sender<DateStepOutcome>,
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
    let _ = request;
    Route::Status // RED stub (#126): no step route yet
}

/// Only a loopback peer may request a step (`127.0.0.0/8`, `::1`, or an IPv4-mapped loopback).
pub fn peer_allowed(ip: IpAddr) -> bool {
    let _ = ip;
    false // RED stub (#126)
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
