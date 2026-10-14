use super::*;
use std::net::TcpListener;
use std::thread;

/// A real answer of `GET /status HTTP/1.0` from a locked dev1 daemon (dantesync 1.17.0,
/// captured 14.10.2026 for issue 132).
const REAL_RESPONSE: &[u8] = include_bytes!("../../tests/fixtures/tray/status_response_200.http");

fn status(mode: &str, locked: bool, settled: bool, rate: f64) -> TrayStatus {
    TrayStatus {
        mode: mode.to_string(),
        is_locked: locked,
        settled,
        smoothed_rate_ppm: rate,
        clock_alarm_interval_s: 60,
        ..TrayStatus::default()
    }
}

fn pixel(rgba: &[u8], x: usize, y: usize) -> [u8; 4] {
    let i = (y * ICON_SIZE as usize + x) * 4;
    [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
}

// ---------------------------------------------------------------------------
// state + colour
// ---------------------------------------------------------------------------

#[test]
fn each_state_has_its_windows_colour() {
    let cases = [
        (
            status("NTP-only", false, true, 3.0),
            TrayState::NtpOnly,
            RGB_NTP_ONLY,
        ),
        (status("NANO", true, true, 0.0), TrayState::Nano, RGB_NANO),
        (
            status("LOCK", true, true, 0.0),
            TrayState::Locked,
            RGB_LOCKED,
        ),
        (
            status("PROD", false, true, 0.0),
            TrayState::Settled,
            RGB_ACQUIRING,
        ),
        (
            status("ACQ", false, false, 0.0),
            TrayState::Acquiring,
            RGB_ACQUIRING,
        ),
    ];
    for (s, state, rgb) in cases {
        assert_eq!(tray_state(Some(&s)), state, "{:?}", s.mode);
        assert_eq!(icon_spec(Some(&s), false).rgb, rgb, "{:?}", s.mode);
    }
    assert_eq!(tray_state(None), TrayState::Offline);
    assert_eq!(icon_spec(None, false).rgb, RGB_OFFLINE);
}

#[test]
fn offline_is_never_green_and_has_no_ring() {
    for badge in [false, true] {
        let spec = icon_spec(None, badge);
        assert_eq!(spec.rgb, [220, 53, 69]);
        assert_ne!(spec.rgb, RGB_LOCKED);
        assert_ne!(spec.rgb, RGB_NANO);
        assert_eq!(spec.pulse, 0.0);
        assert_eq!(spec.update_badge, badge);
    }
}

#[test]
fn ntp_only_and_nano_win_over_the_lock_flag() {
    // The mode decides first, exactly like the Windows tray always did.
    assert_eq!(
        tray_state(Some(&status("NTP-only", true, true, 0.0))),
        TrayState::NtpOnly
    );
    assert_eq!(
        tray_state(Some(&status("NANO", false, false, 0.0))),
        TrayState::Nano
    );
    // An empty mode (old daemon) falls back to the lock / settled flags.
    assert_eq!(
        tray_state(Some(&status("", true, false, 0.0))),
        TrayState::Locked
    );
    assert_eq!(
        tray_state(Some(&status("", false, true, 0.0))),
        TrayState::Settled
    );
}

#[test]
fn pulse_follows_the_drift_rate_per_state() {
    // Locked: |rate| / 20, capped at 1.
    assert_eq!(
        icon_spec(Some(&status("LOCK", true, true, -5.0)), false).pulse,
        0.25
    );
    assert_eq!(
        icon_spec(Some(&status("LOCK", true, true, 80.0)), false).pulse,
        1.0
    );
    // NANO: |rate| / 5.
    assert_eq!(
        icon_spec(Some(&status("NANO", true, true, 2.5)), false).pulse,
        0.5
    );
    assert_eq!(
        icon_spec(Some(&status("NANO", true, true, -9.0)), false).pulse,
        1.0
    );
    // Settled, not locked: at least 0.3.
    assert_eq!(
        icon_spec(Some(&status("PROD", false, true, 0.0)), false).pulse,
        0.3
    );
    assert_eq!(
        icon_spec(Some(&status("PROD", false, true, 10.0)), false).pulse,
        0.5
    );
    // NTP-only and acquiring: no ring.
    assert_eq!(
        icon_spec(Some(&status("NTP-only", false, true, 10.0)), false).pulse,
        0.0
    );
    assert_eq!(
        icon_spec(Some(&status("ACQ", false, false, 10.0)), false).pulse,
        0.0
    );
}

#[test]
fn update_badge_is_passed_through() {
    assert!(icon_spec(Some(&status("LOCK", true, true, 0.0)), true).update_badge);
    assert!(!icon_spec(Some(&status("LOCK", true, true, 0.0)), false).update_badge);
}

#[test]
fn state_labels_are_distinct() {
    let all = [
        TrayState::Offline,
        TrayState::NtpOnly,
        TrayState::Nano,
        TrayState::Locked,
        TrayState::Settled,
        TrayState::Acquiring,
    ];
    let labels: std::collections::HashSet<_> = all.iter().map(|s| s.label()).collect();
    assert_eq!(labels.len(), all.len());
    assert_eq!(TrayState::Locked.label(), "LOCK");
    assert_eq!(TrayState::Offline.label(), "OFFLINE");
}

#[test]
fn same_look_ignores_tiny_pulse_changes_only() {
    let a = IconSpec {
        rgb: RGB_LOCKED,
        pulse: 0.20,
        update_badge: false,
    };
    assert!(a.same_look(&IconSpec { pulse: 0.24, ..a }));
    assert!(!a.same_look(&IconSpec { pulse: 0.30, ..a }));
    assert!(!a.same_look(&IconSpec { rgb: RGB_NANO, ..a }));
    assert!(!a.same_look(&IconSpec {
        update_badge: true,
        ..a
    }));
    assert_eq!(IconSpec::connecting(), IconSpec::plain(RGB_ACQUIRING));
}

// ---------------------------------------------------------------------------
// pixels
// ---------------------------------------------------------------------------

#[test]
fn rendered_icon_is_32x32_rgba() {
    let rgba = render_icon_rgba(&IconSpec::plain(RGB_LOCKED));
    assert_eq!(rgba.len(), 32 * 32 * 4);
}

#[test]
fn rendered_icon_fill_corner_and_edge() {
    let rgba = render_icon_rgba(&IconSpec::plain(RGB_OFFLINE));
    // The centre is the solid fill.
    assert_eq!(pixel(&rgba, 16, 16), [220, 53, 69, 255]);
    // The corners are transparent.
    assert_eq!(pixel(&rgba, 0, 0), [0, 0, 0, 0]);
    assert_eq!(pixel(&rgba, 31, 31), [0, 0, 0, 0]);
    // Without a ring, the ring band is transparent too (x = 29: dist 13.5 from centre).
    assert_eq!(pixel(&rgba, 29, 15), [0, 0, 0, 0]);
    // The fill edge is anti-aliased: alpha strictly between 0 and 255 somewhere on row 15.
    let edge = (0..32)
        .map(|x| pixel(&rgba, x, 15)[3])
        .any(|a| a > 0 && a < 255);
    assert!(edge);
}

#[test]
fn rendered_ring_is_a_brighter_fading_tint() {
    let rgba = render_icon_rgba(&IconSpec {
        rgb: RGB_LOCKED,
        pulse: 1.0,
        update_badge: false,
    });
    // dist(x=27.5, y=15.5 → centre 16) = 11.5 → ring_pos 0.125 → alpha 157.
    let inner = pixel(&rgba, 27, 15);
    assert_eq!(&inner[..3], &[52, 217, 89]);
    assert_eq!(inner[3], 157);
    // Further out fades: dist 13.5 → ring_pos 0.625 → alpha 67.
    let outer = pixel(&rgba, 29, 15);
    assert_eq!(outer[3], 67);
    assert!(outer[3] < inner[3]);
    // A bright channel is clamped at 255.
    let yellow = render_icon_rgba(&IconSpec {
        rgb: RGB_ACQUIRING,
        pulse: 1.0,
        update_badge: false,
    });
    assert_eq!(&pixel(&yellow, 27, 15)[..3], &[255, 250, 9]);
}

#[test]
fn rendered_badge_is_orange_and_covers_the_fill() {
    let spec = IconSpec {
        rgb: RGB_LOCKED,
        pulse: 0.0,
        update_badge: true,
    };
    let rgba = render_icon_rgba(&spec);
    assert_eq!(pixel(&rgba, 24, 6), [255, 152, 0, 255]);
    // (21, 10) is inside both the badge and the fill: the badge wins.
    assert_eq!(&pixel(&rgba, 21, 10)[..3], &[255, 152, 0]);
    let plain = render_icon_rgba(&IconSpec::plain(RGB_LOCKED));
    assert_eq!(&pixel(&plain, 21, 10)[..3], &RGB_LOCKED);
    assert_eq!(pixel(&plain, 24, 6), [0, 0, 0, 0]);
}

// ---------------------------------------------------------------------------
// texts
// ---------------------------------------------------------------------------

#[test]
fn status_texts_match_the_windows_format() {
    let s = TrayStatus {
        mode: "LOCK".into(),
        is_locked: true,
        smoothed_rate_ppm: -1.94,
        drift_ppm: 41.99,
        ntp_offset_us: 17,
        accumulated_phase_us: 2527.4,
        gm_source_ip: Some("10.77.9.230".parse().unwrap()),
        ..TrayStatus::default()
    };
    let t = status_texts(&s, "1.18.0");
    assert_eq!(
        t.tooltip,
        "DanteSync v1.18.0\nMode: LOCK | Drift: -1.9us/s\nFreq Adj: +42.0ppm\n\
         NTP Offset: +17us\nAccum. Phase: +2527us\nPTP Master: 10.77.9.230"
    );
    assert_eq!(t.status_line, "LOCK | Drift: -1.9us/s");
    assert_eq!(t.mode_line, "Mode: LOCK | Adj: +42.0ppm");
    assert_eq!(t.gm_line, "PTP Master: 10.77.9.230");
}

#[test]
fn small_phase_and_missing_master_are_left_out() {
    let s = TrayStatus {
        mode: "NANO".into(),
        accumulated_phase_us: -9.9,
        ntp_offset_us: -3,
        ..TrayStatus::default()
    };
    let t = status_texts(&s, "1.0.0");
    assert_eq!(
        t.tooltip,
        "DanteSync v1.0.0\nMode: NANO | Drift: +0.0us/s\nFreq Adj: +0.0ppm\nNTP Offset: -3us"
    );
    assert_eq!(t.gm_line, "PTP Master: --");
}

#[test]
fn mode_name_falls_back_for_an_old_daemon() {
    assert_eq!(mode_name(&status("", true, true, 0.0)), "LOCK");
    assert_eq!(mode_name(&status("", false, true, 0.0)), "ACQ");
    assert_eq!(mode_name(&status("PROD", false, true, 0.0)), "PROD");
}

#[test]
fn offline_texts_are_the_windows_offline_texts() {
    let t = offline_texts("1.18.0");
    assert_eq!(t.tooltip, "DanteSync v1.18.0\nService Offline");
    assert_eq!(t.status_line, "Service Offline");
    assert_eq!(t.mode_line, "--");
    assert_eq!(t.gm_line, "PTP Master: --");
}

// ---------------------------------------------------------------------------
// clock alarm cadence
// ---------------------------------------------------------------------------

fn alarm(active: bool, since: Option<u64>) -> ClockAlarmStatus {
    ClockAlarmStatus {
        active,
        since,
        reason: if active {
            "PTP stale".to_string()
        } else {
            String::new()
        },
    }
}

#[test]
fn alarm_raises_once_per_interval() {
    let t0 = Instant::now();
    let mut c = ClockAlarmCadence::new();
    let a = alarm(true, Some(1_000));
    assert_eq!(
        c.decide(&a, 60, t0),
        AlarmAction::Raise(compose_message("PTP stale", Some(1_000)))
    );
    assert_eq!(
        c.decide(&a, 60, t0 + Duration::from_secs(1)),
        AlarmAction::Quiet
    );
    assert_eq!(
        c.decide(&a, 60, t0 + Duration::from_secs(59)),
        AlarmAction::Quiet
    );
    assert!(matches!(
        c.decide(&a, 60, t0 + Duration::from_secs(60)),
        AlarmAction::Raise(_)
    ));
    assert_eq!(
        c.decide(&a, 60, t0 + Duration::from_secs(61)),
        AlarmAction::Quiet
    );
}

#[test]
fn a_new_episode_raises_at_once() {
    let t0 = Instant::now();
    let mut c = ClockAlarmCadence::new();
    assert!(matches!(
        c.decide(&alarm(true, Some(1_000)), 60, t0),
        AlarmAction::Raise(_)
    ));
    assert!(matches!(
        c.decide(&alarm(true, Some(2_000)), 60, t0 + Duration::from_secs(2)),
        AlarmAction::Raise(_)
    ));
}

#[test]
fn the_interval_is_floored_at_ten_seconds() {
    let t0 = Instant::now();
    let mut c = ClockAlarmCadence::new();
    let a = alarm(true, Some(5));
    assert!(matches!(c.decide(&a, 0, t0), AlarmAction::Raise(_)));
    assert_eq!(
        c.decide(&a, 0, t0 + Duration::from_secs(9)),
        AlarmAction::Quiet
    );
    assert!(matches!(
        c.decide(&a, 0, t0 + Duration::from_secs(10)),
        AlarmAction::Raise(_)
    ));
}

#[test]
fn clearing_reports_once_then_stays_quiet() {
    let t0 = Instant::now();
    let mut c = ClockAlarmCadence::new();
    // Never active: quiet.
    assert_eq!(c.decide(&alarm(false, None), 60, t0), AlarmAction::Quiet);
    assert!(matches!(
        c.decide(&alarm(true, Some(7)), 60, t0),
        AlarmAction::Raise(_)
    ));
    assert_eq!(
        c.decide(&alarm(false, None), 60, t0 + Duration::from_secs(3)),
        AlarmAction::Cleared
    );
    assert_eq!(
        c.decide(&alarm(false, None), 60, t0 + Duration::from_secs(4)),
        AlarmAction::Quiet
    );
    // A later episode raises again at once.
    assert!(matches!(
        c.decide(&alarm(true, Some(9)), 60, t0 + Duration::from_secs(5)),
        AlarmAction::Raise(_)
    ));
}

// ---------------------------------------------------------------------------
// HTTP status
// ---------------------------------------------------------------------------

#[test]
fn the_real_daemon_answer_parses() {
    let s = parse_status_response(REAL_RESPONSE).expect("real answer parses");
    assert_eq!(s.mode, "LOCK");
    assert!(s.is_locked);
    assert!(s.settled);
    assert_eq!(s.gm_source_ip, Some("10.77.9.230".parse().unwrap()));
    assert_eq!(s._gm_uuid, Some([0, 29, 193, 8, 2, 20]));
    assert!(!s.clock_alarm.active);
    assert_eq!(tray_state(Some(&s)), TrayState::Locked);
}

#[test]
fn a_minimal_old_status_gets_the_defaults() {
    let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
        {\"offset_ns\":1,\"drift_ppm\":0.5,\"gm_uuid\":null,\"settled\":false,\"updated_ts\":9}";
    let s = parse_status_response(raw).expect("minimal status parses");
    assert_eq!(s.clock_alarm_interval_s, 60);
    assert_eq!(s.mode, "");
    assert!(!s.clock_alarm.active);
    assert_eq!(tray_state(Some(&s)), TrayState::Acquiring);
}

#[test]
fn anything_but_a_whole_200_status_is_offline() {
    let body_start = REAL_RESPONSE
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap()
        + 4;
    // Truncated body.
    assert!(parse_status_response(&REAL_RESPONSE[..body_start + 100]).is_none());
    // No header end.
    assert!(parse_status_response(b"HTTP/1.1 200 OK\r\nContent-Type: x").is_none());
    // Not 200, even with a valid status body.
    let body = &REAL_RESPONSE[body_start..];
    for head in [
        &b"HTTP/1.1 404 Not Found\r\n\r\n"[..],
        &b"HTTP/1.1 500 Internal Server Error\r\n\r\n"[..],
        &b"SPDY/3 200 OK\r\n\r\n"[..],
    ] {
        let mut raw = head.to_vec();
        raw.extend_from_slice(body);
        assert!(parse_status_response(&raw).is_none());
    }
    let mut whole = b"HTTP/1.0 200 OK\r\n\r\n".to_vec();
    whole.extend_from_slice(body);
    assert!(parse_status_response(&whole).is_some());
    // Not HTTP.
    assert!(parse_status_response(b"SSH-2.0-OpenSSH\r\n\r\n{}").is_none());
    // A required field missing (a foreign JSON).
    assert!(parse_status_response(b"HTTP/1.1 200 OK\r\n\r\n{\"mode\":\"LOCK\"}").is_none());
    // Empty.
    assert!(parse_status_response(b"").is_none());
}

/// Serve one connection with `answer`, after reading the request; return the request.
fn serve_once(answer: &'static [u8]) -> (SocketAddr, thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut buf = [0u8; 1024];
        let n = conn.read(&mut buf).unwrap();
        // The client may hang up early (oversized answer): a failed write is fine.
        let _ = conn.write_all(answer);
        buf[..n].to_vec()
    });
    (addr, handle)
}

#[test]
fn fetch_reads_a_live_answer() {
    let (addr, server) = serve_once(REAL_RESPONSE);
    let s = fetch_status(addr, Duration::from_secs(2)).expect("fetch succeeds");
    assert_eq!(s.mode, "LOCK");
    let request = server.join().unwrap();
    assert_eq!(request, STATUS_REQUEST);
}

#[test]
fn fetch_of_an_error_answer_is_offline() {
    let (addr, server) = serve_once(b"HTTP/1.1 503 Service Unavailable\r\n\r\n{}");
    assert!(fetch_status(addr, Duration::from_secs(2)).is_none());
    server.join().unwrap();
}

#[test]
fn fetch_of_a_closed_port_is_offline() {
    let addr = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    assert!(fetch_status(addr, Duration::from_secs(1)).is_none());
}

#[test]
fn fetch_of_a_silent_server_is_offline_within_the_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (conn, _) = listener.accept().unwrap();
        thread::sleep(Duration::from_millis(2000));
        drop(conn);
    });
    let start = Instant::now();
    assert!(fetch_status(addr, Duration::from_millis(200)).is_none());
    assert!(start.elapsed() < Duration::from_millis(1500));
    server.join().unwrap();
}

#[test]
fn fetch_refuses_an_oversized_answer() {
    static HUGE: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    let huge: &'static [u8] = HUGE.get_or_init(|| {
        let mut v = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        v.resize(MAX_RESPONSE_BYTES + 10_000, b' ');
        v
    });
    let (addr, server) = serve_once(huge);
    assert!(fetch_status(addr, Duration::from_secs(2)).is_none());
    server.join().unwrap();
}

#[test]
fn lock_file_prefers_the_runtime_dir() {
    assert_eq!(
        lock_file_path(Some("/run/user/1000")),
        PathBuf::from("/run/user/1000/dantesync-tray.lock")
    );
    assert_eq!(
        lock_file_path(Some("")),
        PathBuf::from("/tmp/dantesync-tray.lock")
    );
    assert_eq!(
        lock_file_path(None),
        PathBuf::from("/tmp/dantesync-tray.lock")
    );
}
