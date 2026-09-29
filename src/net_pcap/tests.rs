use super::*;

/// Test PTP constants
#[test]
fn test_ptp_constants() {
    assert_eq!(PTP_EVENT_PORT, 319);
    assert_eq!(PTP_GENERAL_PORT, 320);
    assert_eq!(PTP_MULTICAST, Ipv4Addr::new(224, 0, 1, 129));
    assert!(PTP_MULTICAST.is_multicast());
}

/// Test pcap timestamp to SystemTime conversion
#[test]
fn test_pcap_ts_to_systemtime() {
    // Unix epoch (1970-01-01 00:00:00)
    let ts = pcap_ts_to_systemtime(0, 0);
    assert_eq!(ts, UNIX_EPOCH);

    // 1 second after epoch
    let ts = pcap_ts_to_systemtime(1, 0);
    assert_eq!(ts, UNIX_EPOCH + Duration::from_secs(1));

    // 1.5 seconds after epoch (with microseconds)
    let ts = pcap_ts_to_systemtime(1, 500_000);
    assert_eq!(ts, UNIX_EPOCH + Duration::from_micros(1_500_000));

    // Realistic timestamp (2024-01-01 00:00:00 UTC = 1704067200)
    let ts = pcap_ts_to_systemtime(1704067200, 0);
    assert_eq!(ts, UNIX_EPOCH + Duration::from_secs(1704067200));
}

/// Test that microseconds are correctly converted to nanoseconds
#[test]
fn test_pcap_ts_microsecond_precision() {
    // 123.456789 seconds - but pcap only has microsecond precision
    let ts = pcap_ts_to_systemtime(123, 456_789);

    // Should be 123 seconds + 456789 microseconds = 456789000 nanoseconds
    let expected = UNIX_EPOCH + Duration::new(123, 456_789_000);
    assert_eq!(ts, expected);
}

/// Test Ethernet/IP/UDP header constant
#[test]
fn test_ethernet_ip_udp_header_size() {
    // Ethernet header: 14 bytes
    // IP header: 20 bytes (minimum)
    // UDP header: 8 bytes
    // Total: 42 bytes
    const ETH_IP_UDP_HEADER: usize = 42;
    assert_eq!(ETH_IP_UDP_HEADER, 14 + 20 + 8);
}

/// Test EtherType detection for IPv4
#[test]
fn test_ethertype_ipv4() {
    // IPv4 EtherType is 0x0800
    let data: [u8; 14] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x08, 0x00];
    assert_eq!(data[12], 0x08);
    assert_eq!(data[13], 0x00);
}

/// Test UDP protocol number in IP header
#[test]
fn test_ip_protocol_udp() {
    // UDP is protocol number 17
    // In IP header, protocol is at byte offset 9 (0-indexed)
    // In full frame, that's offset 14 (ethernet) + 9 = 23
    let protocol_byte = 17u8;
    assert_eq!(protocol_byte, 17);
}

/// Test PTP port detection from UDP header
#[test]
fn test_ptp_port_extraction() {
    // UDP destination port is at bytes 2-3 of UDP header (big-endian)
    // In full frame: offset 14 (eth) + 20 (ip) + 2 = 36, 37

    // Port 319 = 0x013F
    let port_319_bytes: [u8; 2] = [0x01, 0x3F];
    let port = ((port_319_bytes[0] as u16) << 8) | port_319_bytes[1] as u16;
    assert_eq!(port, 319);

    // Port 320 = 0x0140
    let port_320_bytes: [u8; 2] = [0x01, 0x40];
    let port = ((port_320_bytes[0] as u16) << 8) | port_320_bytes[1] as u16;
    assert_eq!(port, 320);
}

/// Test simulated PTP packet validation
#[test]
fn test_simulated_ptp_packet_structure() {
    // Minimum valid PTP-carrying Ethernet frame
    // Ethernet (14) + IP (20) + UDP (8) + PTP Sync (44) = 86 bytes
    const MIN_PTP_FRAME: usize = 42 + 44;
    assert_eq!(MIN_PTP_FRAME, 86);

    // Create a simulated frame
    let mut frame = vec![0u8; MIN_PTP_FRAME];

    // Set EtherType to IPv4 (0x0800) at bytes 12-13
    frame[12] = 0x08;
    frame[13] = 0x00;

    // Set IP protocol to UDP (17) at byte 23
    frame[23] = 17;

    // Set UDP destination port to 319 at bytes 36-37
    frame[36] = 0x01;
    frame[37] = 0x3F;

    // Verify parsing would succeed
    assert!(frame[12] == 0x08 && frame[13] == 0x00, "Should be IPv4");
    assert!(frame[23] == 17, "Should be UDP");
    let dst_port = ((frame[36] as u16) << 8) | frame[37] as u16;
    assert!(dst_port == 319 || dst_port == 320, "Should be PTP port");
}

/// #58 RED->GREEN: `wpcap_runtime_available()` must never panic/crash --
/// it's the guard that replaces a delay-load crash with a plain bool.
#[test]
fn test_wpcap_runtime_available_never_panics() {
    let _ = wpcap_runtime_available();
}

/// #58 regression: on a machine with only the Npcap SDK (every
/// `windows-latest` CI runner -- confirmed absent by
/// `wpcap_runtime_available()` returning `false` there), constructing
/// either capture path must return a graceful `Err`, never crash the
/// process. Before this fix, `find_device()` called `Device::list()`
/// unconditionally, which triggered the delay-loaded `wpcap.dll` symbol
/// resolution and aborted the whole test binary with `0xc06d007e`
/// (observed live via `NtpClient::new()` in run 30337735289 -- that is
/// the RED this test proves GREEN). On a real box where Npcap IS
/// installed this test is a no-op (skipped) -- it is specifically about
/// the "runtime missing" degradation path, not normal capture behavior.
#[test]
fn test_find_device_gracefully_errors_without_npcap_runtime() {
    if wpcap_runtime_available() {
        eprintln!(
            "skipping test_find_device_gracefully_errors_without_npcap_runtime: \
             Npcap runtime IS installed on this machine"
        );
        return;
    }
    let result = find_device("eth0");
    assert!(
        result.is_err(),
        "expected a graceful Err when the Npcap runtime is missing, got Ok -- \
         this used to crash the whole process (#58)"
    );
}

/// #58 regression: same guard, exercised through the public
/// `PcapNtpTransport::new()` entry point (the exact call chain that
/// crashed via `NtpClient::new()` in ntp.rs's own `test_ntp_client_new`).
#[test]
fn test_pcap_ntp_transport_new_gracefully_errors_without_npcap_runtime() {
    if wpcap_runtime_available() {
        eprintln!(
            "skipping test_pcap_ntp_transport_new_gracefully_errors_without_npcap_runtime: \
             Npcap runtime IS installed on this machine"
        );
        return;
    }
    let result = PcapNtpTransport::new(
        Ipv4Addr::new(127, 0, 0, 1),
        &crate::dscp::DscpConfig::default(),
    );
    assert!(
        result.is_err(),
        "expected a graceful Err when the Npcap runtime is missing, got Ok -- \
         this used to crash the whole process (#58)"
    );
}

/// #58 regression: the PTP capture path (`NpcapPtpNetwork::new`) goes through
/// `find_ptp_capture_device` (camera-box issue 1073), which shares the same
/// `list_devices_guarded()` #58 guard as `PcapNtpTransport::new` (reaching the
/// name-based `find_device()` only on the fallback path) -- same graceful-Err
/// expectation when the Npcap runtime is missing.
#[test]
fn test_npcap_ptp_network_new_gracefully_errors_without_npcap_runtime() {
    if wpcap_runtime_available() {
        eprintln!(
            "skipping test_npcap_ptp_network_new_gracefully_errors_without_npcap_runtime: \
             Npcap runtime IS installed on this machine"
        );
        return;
    }
    let result = NpcapPtpNetwork::new("eth0", &crate::gm_filter::GmAllowlist::default());
    assert!(
        result.is_err(),
        "expected a graceful Err when the Npcap runtime is missing, got Ok -- \
         this used to crash the whole process (#58)"
    );
}

/// camera-box issue 1073: the new gm_allowlist-aware capture-device selector
/// shares the SAME #58 runtime guard (it calls `list_devices_guarded` before
/// any real `pcap::` call), so on a runtime-less machine it returns a graceful
/// `Err` rather than crashing — with an empty (unrestricted) allowlist, which
/// is the byte-identical fallback path.
#[test]
fn test_find_ptp_capture_device_gracefully_errors_without_npcap_runtime() {
    if wpcap_runtime_available() {
        eprintln!(
            "skipping test_find_ptp_capture_device_gracefully_errors_without_npcap_runtime: \
             Npcap runtime IS installed on this machine"
        );
        return;
    }
    let result = find_ptp_capture_device(&crate::gm_filter::GmAllowlist::default(), "eth0");
    assert!(
        result.is_err(),
        "expected a graceful Err when the Npcap runtime is missing, got Ok"
    );
}

/// #53 continuation regression: `find_device_for_ntp_server` -- the NEW
/// NTP-server-reachability selection path -- goes through the SAME #58
/// guard as `find_device`. On a runtime-less machine it must return a
/// graceful `Err`, never crash, exactly like the name-based path above.
#[test]
fn test_find_device_for_ntp_server_gracefully_errors_without_npcap_runtime() {
    if wpcap_runtime_available() {
        eprintln!(
            "skipping test_find_device_for_ntp_server_gracefully_errors_without_npcap_runtime: \
             Npcap runtime IS installed on this machine"
        );
        return;
    }
    let result = find_device_for_ntp_server(Ipv4Addr::new(10, 77, 9, 202));
    assert!(
        result.is_err(),
        "expected a graceful Err when the Npcap runtime is missing, got Ok -- \
         same #58 guard as find_device()"
    );
}

/// Adversarial-review regression: `list_npcap_devices()` used to call
/// `Device::list()` directly, bypassing `list_devices_guarded()` -- so on
/// a runtime-less machine it would have crashed the process exactly like
/// the pre-#58 `find_device()` used to, instead of returning a graceful
/// `Err`. Now routed through the same guard as every other pcap:: entry
/// point.
#[test]
fn test_list_npcap_devices_gracefully_errors_without_npcap_runtime() {
    if wpcap_runtime_available() {
        eprintln!(
            "skipping test_list_npcap_devices_gracefully_errors_without_npcap_runtime: \
             Npcap runtime IS installed on this machine"
        );
        return;
    }
    let result = list_npcap_devices();
    assert!(
        result.is_err(),
        "expected a graceful Err when the Npcap runtime is missing, got Ok -- \
         list_npcap_devices() must go through the same #58 guard as every other pcap:: entry \
         point"
    );
}
