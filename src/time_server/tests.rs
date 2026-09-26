use super::*;

#[test]
fn test_request_magic() {
    let bytes = REQUEST_MAGIC.to_be_bytes();
    assert_eq!(&bytes, b"DSYN");
}

#[test]
fn test_response_magic() {
    let bytes = RESPONSE_MAGIC.to_be_bytes();
    assert_eq!(&bytes, b"DSYR");
}

#[test]
fn test_build_response_format() {
    let status = SyncStatus::default();
    let request_id = 0x12345678u32;
    let response = build_response(request_id, &status);

    // Check magic
    let magic = u32::from_be_bytes([response[0], response[1], response[2], response[3]]);
    assert_eq!(magic, RESPONSE_MAGIC);

    // Check request ID echo
    let echo_id = u32::from_be_bytes([response[4], response[5], response[6], response[7]]);
    assert_eq!(echo_id, request_id);

    // Check system time is reasonable (after 2020)
    let system_ns = u64::from_be_bytes([
        response[8],
        response[9],
        response[10],
        response[11],
        response[12],
        response[13],
        response[14],
        response[15],
    ]);
    let year_2020_ns = 1577836800u64 * 1_000_000_000; // 2020-01-01 in ns
    assert!(system_ns > year_2020_ns, "System time should be after 2020");

    // Check monotonic counter is non-zero
    let mono = u64::from_be_bytes([
        response[16],
        response[17],
        response[18],
        response[19],
        response[20],
        response[21],
        response[22],
        response[23],
    ]);
    assert!(mono > 0, "Monotonic counter should be non-zero");

    // Check monotonic frequency is non-zero
    let freq = u64::from_be_bytes([
        response[48],
        response[49],
        response[50],
        response[51],
        response[52],
        response[53],
        response[54],
        response[55],
    ]);
    assert!(freq > 0, "Monotonic frequency should be non-zero");
}

#[test]
fn test_build_response_with_status() {
    let mut status = SyncStatus::default();
    status.offset_ns = -12345;
    status.smoothed_rate_ppm = 1.5;
    status.drift_ppm = -0.75;
    status.mode = "LOCK".to_string();
    status.is_locked = true;
    status.gm_uuid = Some([0x00, 0x1D, 0xC1, 0xAB, 0xCD, 0xEF]);

    let response = build_response(42, &status);

    // Check PTP offset
    let offset = i64::from_be_bytes([
        response[24],
        response[25],
        response[26],
        response[27],
        response[28],
        response[29],
        response[30],
        response[31],
    ]);
    assert_eq!(offset, -12345);

    // Check drift rate (1.5 * 1000 = 1500)
    let drift = i32::from_be_bytes([response[32], response[33], response[34], response[35]]);
    assert_eq!(drift, 1500);

    // Check frequency adjustment (-0.75 * 1000 = -750)
    let adj = i32::from_be_bytes([response[36], response[37], response[38], response[39]]);
    assert_eq!(adj, -750);

    // Check mode (LOCK = 3)
    assert_eq!(response[40], 3);

    // Check is_locked
    assert_eq!(response[41], 1);

    // Check GM UUID
    assert_eq!(&response[42..48], &[0x00, 0x1D, 0xC1, 0xAB, 0xCD, 0xEF]);
}

#[test]
fn test_mode_encoding() {
    let modes = [
        ("", 0),
        ("ACQ", 1),
        ("PROD", 2),
        ("LOCK", 3),
        ("NANO", 4),
        ("NTP-only", 5),
    ];

    for (mode_str, expected) in modes {
        let mut status = SyncStatus::default();
        status.mode = mode_str.to_string();
        let response = build_response(0, &status);
        assert_eq!(
            response[40], expected,
            "Mode '{}' should encode to {}",
            mode_str, expected
        );
    }
}

#[test]
fn test_monotonic_counter_increases() {
    let c1 = get_monotonic_counter();
    std::thread::sleep(std::time::Duration::from_millis(10));
    let c2 = get_monotonic_counter();
    assert!(c2 > c1, "Monotonic counter should increase over time");
}

#[test]
fn test_monotonic_frequency_valid() {
    let freq = get_monotonic_frequency();
    // Should be at least 1MHz (reasonable for any modern system)
    assert!(
        freq >= 1_000_000,
        "Monotonic frequency should be at least 1MHz"
    );
    // On Linux it's exactly 10^9 (nanoseconds)
    #[cfg(unix)]
    assert_eq!(freq, 1_000_000_000);
}

#[test]
fn test_response_size() {
    let status = SyncStatus::default();
    let response = build_response(0, &status);
    assert_eq!(response.len(), RESPONSE_SIZE);
}

#[test]
fn test_time_server_port_constant() {
    assert_eq!(TIME_SERVER_PORT, 31900);
}

#[test]
fn test_build_response_ntp_fields() {
    let mut status = SyncStatus::default();
    status.ntp_offset_us = 1234;
    status.accumulated_phase_us = -567.8;
    status.ntp_failed = false;
    status.settled = true;

    let response = build_response(0, &status);

    // [56-59] NTP offset (i32)
    let ntp_off = i32::from_be_bytes([response[56], response[57], response[58], response[59]]);
    assert_eq!(ntp_off, 1234);

    // [60-61] Accumulated phase (i16, rounded)
    let phase = i16::from_be_bytes([response[60], response[61]]);
    assert_eq!(phase, -568); // -567.8 rounds to -568

    // [62] Flags: bit 0 = ntp_failed (0), bit 1 = settled (1) = 0b10 = 2
    assert_eq!(response[62], 0x02);

    // [63] Reserved
    assert_eq!(response[63], 0);
}

#[test]
fn test_build_response_ntp_failed_flag() {
    let mut status = SyncStatus::default();
    status.ntp_failed = true;
    status.settled = false;

    let response = build_response(0, &status);
    // bit 0 = ntp_failed (1), bit 1 = settled (0) = 0b01 = 1
    assert_eq!(response[62], 0x01);
}

#[test]
fn test_build_response_both_flags() {
    let mut status = SyncStatus::default();
    status.ntp_failed = true;
    status.settled = true;

    let response = build_response(0, &status);
    // bit 0 = ntp_failed (1), bit 1 = settled (1) = 0b11 = 3
    assert_eq!(response[62], 0x03);
}

#[test]
fn test_build_response_ntp_offset_negative() {
    let mut status = SyncStatus::default();
    status.ntp_offset_us = -42000;

    let response = build_response(0, &status);
    let ntp_off = i32::from_be_bytes([response[56], response[57], response[58], response[59]]);
    assert_eq!(ntp_off, -42000);
}

// ---- dantesync#88: the versioned date-offset extension ------------------------------

fn master_status() -> SyncStatus {
    let mut status = SyncStatus::default();
    status.is_locked = true;
    status.mode = "LOCK".to_string();
    status.gm_uuid = Some([0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c]);
    status.ntp_offset_us = -1234;
    status.date_authority = "master".to_string();
    status.date_offset_ns = Some(1_790_000_000_000_000_000);
    status.date_offset_effective_ptp_ns = Some(12_345_000_000_000);
    status.date_offset_seq = Some(3);
    status.date_offset_gm_uuid = Some([0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c]);
    status
}

#[test]
fn a_dsyx_request_is_padded_to_the_base_reply_size_88() {
    // The reply is 112 bytes (104 before the #119 slew fields): a request of the base reply's
    // size keeps the reply-to-request ratio at 1.75 (an 8-byte request would make every spoofed
    // one a 14x amplifier).
    let req = build_ext_request(3);
    assert_eq!(req.len(), EXT_REQUEST_SIZE);
    assert_eq!(EXT_REQUEST_SIZE, RESPONSE_SIZE);
    assert!(req[8..].iter().all(|&b| b == 0), "zero padding");
}

#[test]
fn the_poller_backs_off_after_a_minute_of_silence_and_recovers_on_a_reply_88() {
    let mut b = PollBackoff::default();
    assert_eq!(b.interval(), AUTHORITY_POLL_INTERVAL);
    for _ in 0..AUTHORITY_SILENT_POLLS_BEFORE_BACKOFF - 1 {
        b.on_silence();
    }
    assert_eq!(
        b.interval(),
        AUTHORITY_POLL_INTERVAL,
        "a short silence (a master restart) keeps the 1 s cadence"
    );
    b.on_silence();
    assert_eq!(b.interval(), AUTHORITY_BACKOFF_INTERVAL);
    for _ in 0..10_000 {
        b.on_silence();
    }
    assert_eq!(b.interval(), AUTHORITY_BACKOFF_INTERVAL, "never slower");
    b.on_reply();
    assert_eq!(
        b.interval(),
        AUTHORITY_POLL_INTERVAL,
        "the first reply restores 1 s for good"
    );
}

#[test]
fn a_poller_whose_authority_ever_answered_never_backs_off_past_the_step_lead_88() {
    // A master that answered and then went quiet (a host reboot, a network blip) may announce a
    // step seconds after it is back: a follower polling every 30 s would hear it after its
    // instant and step late. Only a host that NEVER answered DSYX (an older dantesync, a public
    // NTP server) earns the slow cadence.
    let mut b = PollBackoff::default();
    b.on_reply();
    for _ in 0..10_000 {
        b.on_silence();
    }
    assert!(!b.backed_off());
    assert!(
        b.interval().as_nanos() as i64 <= crate::date_offset::MIN_STEP_LEAD_NS / 2,
        "polls at least twice per minimum announce lead, got {:?}",
        b.interval()
    );
}

#[test]
fn test_ext_request_magic() {
    assert_eq!(&REQUEST_MAGIC_EXT.to_be_bytes(), b"DSYX");
    let req = build_ext_request(0xDEADBEEF);
    assert_eq!(&req[0..4], b"DSYX");
    assert_eq!(&req[4..8], &0xDEADBEEFu32.to_be_bytes());
}

#[test]
fn the_extended_reply_is_the_same_base_plus_the_extension_88() {
    let status = master_status();
    let ext_reply = build_response_ext(7, &status);
    assert_eq!(
        ext_reply.len(),
        RESPONSE_SIZE + crate::date_offset::EXT_SIZE_V2
    );
    let base = build_response(7, &status);
    // Every base field an old client reads is identical in the extended reply, except the
    // two clock READINGS (system time [8-15], monotonic counter [16-23]) which are sampled
    // anew by each call.
    assert_eq!(&ext_reply[0..8], &base[0..8]);
    assert_eq!(&ext_reply[24..RESPONSE_SIZE], &base[24..RESPONSE_SIZE]);
    let ext = decode_extension(&ext_reply[RESPONSE_SIZE..]).expect("extension present");
    assert!(ext.authority);
    assert_eq!(ext.announce.date_offset_ns, 1_790_000_000_000_000_000);
    assert_eq!(ext.announce.effective_ptp_ns, 12_345_000_000_000);
    assert_eq!(ext.announce.seq, 3);
}

#[test]
fn an_old_client_reading_only_64_bytes_of_the_extended_reply_decodes_the_same_fields_88() {
    // Older-client compatibility of the payload itself: a parser that knows only the base
    // layout and reads the first 64 bytes sees exactly the fields it always saw.
    let status = master_status();
    let reply = build_response_ext(99, &status);
    let old_view = &reply[..RESPONSE_SIZE];
    assert_eq!(&old_view[0..4], b"DSYR");
    assert_eq!(
        u32::from_be_bytes([old_view[4], old_view[5], old_view[6], old_view[7]]),
        99
    );
    assert_eq!(old_view[40], 3, "mode LOCK");
    assert_eq!(old_view[41], 1, "is_locked");
    assert_eq!(&old_view[42..48], &[0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c]);
    assert_eq!(
        i32::from_be_bytes([old_view[56], old_view[57], old_view[58], old_view[59]]),
        -1234
    );
}

#[test]
fn a_pending_step_is_published_as_the_offset_it_will_take_88() {
    let mut status = master_status();
    status.date_step_pending_ns = Some(-51_000_000);
    status.date_offset_effective_ptp_ns = Some(12_350_000_000_000);
    status.date_offset_seq = Some(4);
    let ext = date_extension_from_status(&status, 1_790_000_100_000_000_000).unwrap();
    assert_eq!(
        ext.announce.date_offset_ns,
        1_790_000_000_000_000_000 - 51_000_000
    );
    assert_eq!(
        ext.now_ptp_ns, 100_000_000_000,
        "PTP now from the D in effect, not from the pending one"
    );
    assert_eq!(ext.announce.effective_ptp_ns, 12_350_000_000_000);
    assert_eq!(ext.announce.seq, 4);
}

#[test]
fn a_slew_is_published_as_the_slew_never_as_a_backward_step_119() {
    // A slewing master: D in effect is part-way down; the published change is the slew itself.
    let mut status = master_status();
    status.date_offset_ns = Some(1_790_000_000_000_000_000 - 10_000_000);
    status.date_slew_from_ns = Some(1_790_000_000_000_000_000);
    status.date_slew_to_ns = Some(1_790_000_000_000_000_000 - 51_000_000);
    status.date_slew_ppm = Some(100);
    status.date_offset_effective_ptp_ns = Some(12_350_000_000_000);
    status.date_offset_seq = Some(5);
    // Even a stale pending-step value must not turn it into a step.
    status.date_step_pending_ns = Some(-41_000_000);
    let reply = build_response_ext(11, &status);
    assert_eq!(reply.len(), RESPONSE_SIZE + crate::date_offset::EXT_SIZE_V2);
    let ext = parse_reply(&reply, 11, Instant::now(), 0)
        .unwrap()
        .ext
        .expect("extension");
    let sl = ext.announce.as_slew().expect("published as a slew");
    assert_eq!(sl.from_ns, 1_790_000_000_000_000_000);
    assert_eq!(sl.to_ns, 1_790_000_000_000_000_000 - 51_000_000);
    assert_eq!(sl.start_ptp_ns, 12_350_000_000_000);
    assert_eq!(sl.ppm, 100);
    assert_eq!(ext.announce.seq, 5);
    // The replier's PTP now comes from its D IN EFFECT.
    let e = date_extension_from_status(&status, 1_790_000_100_000_000_000).unwrap();
    assert_eq!(e.now_ptp_ns, 100_000_000_000 + 10_000_000);
    // Without every slew field it is the plain (step) form.
    status.date_slew_ppm = None;
    let e = date_extension_from_status(&status, 1_790_000_100_000_000_000).unwrap();
    assert_eq!(e.announce.slew, None);
}

#[test]
fn no_extension_is_published_without_the_anchor_grandmaster_88() {
    // The controller clears the anchor GM while a re-anchor is pending: no D may be
    // published in a time base nobody can identify.
    let mut status = master_status();
    status.date_offset_gm_uuid = None;
    assert_eq!(build_response_ext(1, &status).len(), RESPONSE_SIZE);
}

#[test]
fn a_node_without_date_state_answers_dsyx_with_the_base_only_88() {
    let reply = build_response_ext(1, &SyncStatus::default());
    assert_eq!(reply.len(), RESPONSE_SIZE);
    let parsed = parse_reply(&reply, 1, Instant::now(), 0).expect("valid base reply");
    assert_eq!(parsed.ext, None);
}

#[test]
fn a_follower_mirrors_its_state_without_the_authority_flag_88() {
    let mut status = master_status();
    status.date_authority = "follower".to_string();
    let reply = build_response_ext(1, &status);
    let parsed = parse_reply(&reply, 1, Instant::now(), 0).unwrap();
    assert!(
        !parsed.ext.unwrap().authority,
        "only the master is an authority"
    );
}

#[test]
fn parse_reply_reads_gm_lock_and_extension_and_rejects_strangers_88() {
    let status = master_status();
    let reply = build_response_ext(42, &status);
    let t = Instant::now();
    let parsed = parse_reply(&reply, 42, t, 0).expect("valid");
    assert_eq!(parsed.gm_uuid, Some([0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c]));
    assert!(parsed.is_locked);
    assert_eq!(parsed.ext.unwrap().announce.seq, 3);
    assert_eq!(parsed.received, t);
    let base_wall = i64::from_be_bytes(reply[8..16].try_into().unwrap());
    let now_ptp = parsed.ext.unwrap().now_ptp_ns;
    assert!(
        (now_ptp - (base_wall - 1_790_000_000_000_000_000)).abs() < 1_000_000_000,
        "the replier's PTP now = its wall − its D IN EFFECT"
    );
    assert_eq!(
        parsed.ext.unwrap().gm_uuid,
        [0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c],
        "the anchor's grandmaster rides in the extension"
    );

    assert_eq!(
        parse_reply(&reply, 41, t, 0),
        None,
        "a reply to another request id"
    );
    assert_eq!(parse_reply(&reply[..63], 42, t, 0), None, "short");
    let mut bad = reply.clone();
    bad[0] = b'X';
    assert_eq!(parse_reply(&bad, 42, t, 0), None, "wrong magic");
    // An older server's 64-byte reply: valid, no authority.
    let old = build_response(42, &status);
    assert_eq!(parse_reply(&old, 42, t, 0).unwrap().ext, None);
    // A node with no grandmaster reports None, not an all-zero UUID.
    let mut no_gm = master_status();
    no_gm.gm_uuid = None;
    assert_eq!(
        parse_reply(&build_response_ext(1, &no_gm), 1, t, 0)
            .unwrap()
            .gm_uuid,
        None
    );
}

/// Run the server's non-blocking dispatch until the client has its reply (bounded: 5 s).
fn serve_until_reply(
    server: &TimeServer,
    status: &Arc<RwLock<SyncStatus>>,
    client: &UdpSocket,
    buf: &mut [u8],
) -> usize {
    for _ in 0..250 {
        server.handle_requests(status);
        if let Ok((n, _)) = client.recv_from(buf) {
            return n;
        }
    }
    panic!("no reply from the time server within 5 s");
}

#[test]
fn the_server_answers_dsyn_with_64_bytes_and_dsyx_with_the_extension_over_real_udp_88() {
    // End-to-end over loopback through the real `handle_requests` dispatch. Bind the server
    // on an ephemeral port (not 31900, which a running daemon may hold).
    let server = TimeServer {
        socket: UdpSocket::bind("127.0.0.1:0").unwrap(),
    };
    server.socket.set_nonblocking(true).unwrap();
    let addr = server.socket.local_addr().unwrap();
    let status = Arc::new(RwLock::new(master_status()));
    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    client
        .set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();

    let mut old_req = [0u8; 8];
    old_req[0..4].copy_from_slice(b"DSYN");
    old_req[4..8].copy_from_slice(&5u32.to_be_bytes());
    client.send_to(&old_req, addr).unwrap();
    let mut buf = [0u8; 256];
    let n = serve_until_reply(&server, &status, &client, &mut buf);
    assert_eq!(
        n, RESPONSE_SIZE,
        "an old DSYN request gets exactly 64 bytes"
    );

    // A short (unpadded) DSYX is not answered at all: no amplification.
    let mut short = [0u8; 8];
    short[0..4].copy_from_slice(b"DSYX");
    short[4..8].copy_from_slice(&7u32.to_be_bytes());
    client.send_to(&short, addr).unwrap();
    for _ in 0..25 {
        server.handle_requests(&status);
        assert!(
            client.recv_from(&mut buf).is_err(),
            "an unpadded DSYX request got a reply"
        );
    }

    client.send_to(&build_ext_request(6), addr).unwrap();
    let n = serve_until_reply(&server, &status, &client, &mut buf);
    assert_eq!(n, RESPONSE_SIZE + crate::date_offset::EXT_SIZE_V2);
    let parsed = parse_reply(&buf[..n], 6, Instant::now(), 0).unwrap();
    assert_eq!(
        parsed.ext.unwrap().announce.date_offset_ns,
        1_790_000_000_000_000_000
    );
}

#[test]
fn test_build_response_phase_clamp() {
    let mut status = SyncStatus::default();
    // Exceeds i16 range — should be clamped to i16::MAX
    status.accumulated_phase_us = 50000.0;

    let response = build_response(0, &status);
    let phase = i16::from_be_bytes([response[60], response[61]]);
    assert_eq!(phase, i16::MAX); // 32767
}
