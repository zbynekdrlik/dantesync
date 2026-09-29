//! Npcap-based PTP network implementation for Windows.
//!
//! Uses Npcap for packet capture with HIGH PRECISION timestamps that are
//! synchronized with system time. This uses KeQuerySystemTimePrecise() which
//! provides microsecond-level precision AND tracks system clock adjustments.
//!
//! Key: We use TimestampType::HostHighPrec which maps to PCAP_TSTAMP_HOST_HIPREC
//! and uses KeQuerySystemTimePrecise() internally - NOT the default UNSYNCED mode.

use anyhow::{anyhow, Result};
use log::{debug, info, warn};
use pcap::{Active, Capture, Device, TimestampType};
use std::net::{Ipv4Addr, UdpSocket};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PTP_EVENT_PORT: u16 = 319;
const PTP_GENERAL_PORT: u16 = 320;
const PTP_MULTICAST: Ipv4Addr = Ipv4Addr::new(224, 0, 1, 129);

/// Create ONE socket that joins the PTP multicast group `224.0.1.129` on
/// `iface_ip`, purely to hold the IGMP membership for the process lifetime
/// (membership is released when the socket is closed).
///
/// dantesync#109: this socket binds an EPHEMERAL port (`net::igmp_join_bind_addr`),
/// NOT the PTP ports 319/320. IGMP membership is per interface+group, not per
/// port, and pcap's own BPF filter (not this socket) selects the captured PTP
/// traffic — so binding 319/320 was a pointless exclusive claim that collided
/// with a Dante Virtual Soundcard `ptp.exe` on the same host (dantesync, a
/// boot-time service, bound first; `ptp.exe`, started later without
/// `SO_REUSEADDR`, lost both ports with WSAEADDRINUSE). One socket for the
/// group is sufficient; the old per-port pair (319 AND 320) and the now-pointless
/// `SO_REUSEADDR` (it existed only to re-bind the fixed PTP ports) are both gone.
fn join_multicast(iface_ip: Ipv4Addr) -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};

    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;

    let addr = crate::net::igmp_join_bind_addr();
    socket.bind(&addr.into())?;

    socket.join_multicast_v4(&PTP_MULTICAST, &iface_ip)?;
    socket.set_multicast_loop_v4(false)?;
    socket.set_nonblocking(true)?;

    Ok(socket.into())
}

/// Probe whether the Npcap RUNTIME (`wpcap.dll`) is actually loadable,
/// WITHOUT ever making a delay-loaded `pcap::` call.
///
/// #58: `build.rs` delay-loads `wpcap.dll` (`/DELAYLOAD:wpcap.dll`) so the
/// process can still START on a machine that has only the Npcap SDK
/// (link-time `.lib` stubs, e.g. every `windows-latest` CI runner) and not
/// the runtime -- but delay-load only defers WHEN the DLL is resolved, not
/// WHETHER a missing DLL is recoverable: MSVC's default delay-load failure
/// hook raises an unrecoverable structured exception the moment a
/// delay-loaded symbol is first called and the DLL can't be found (observed
/// live: `NtpClient::new()` -> `PcapNtpTransport::new()` -> `find_device()`
/// -> `Device::list()` crashed the whole test binary with `0xc06d007e` on a
/// runtime-less CI runner, run 30337735289). `LoadLibraryW`/`FreeLibrary`
/// live in `kernel32.dll`, which every Windows process implicitly and
/// STATICALLY imports (never delay-loaded) -- probing with them lets us
/// detect a missing runtime BEFORE the first real pcap:: call, so we return
/// a normal `Err` (exactly like any other pcap failure -- `NtpClient::new`
/// already logs and falls back to userspace `rsntp` for this case) instead
/// of crashing the process. This also hardens a real deployed box: if
/// Npcap's runtime is ever missing/corrupted there, dantesync now degrades
/// gracefully instead of crashing outright.
pub(crate) fn wpcap_runtime_available() -> bool {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryW(lp_lib_file_name: *const u16) -> *mut c_void;
        fn FreeLibrary(h_lib_module: *mut c_void) -> i32;
    }

    let wide_name: Vec<u16> = std::ffi::OsStr::new("wpcap.dll")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: `wide_name` is a valid NUL-terminated UTF-16 string that
    // outlives this call; `LoadLibraryW`/`FreeLibrary` are the standard
    // Win32 module-loading APIs used exactly per their documented contract
    // (probe-then-release, never retaining the handle).
    unsafe {
        let handle = LoadLibraryW(wide_name.as_ptr());
        if handle.is_null() {
            false
        } else {
            FreeLibrary(handle);
            true
        }
    }
}

/// Guarded device enumeration — the SINGLE choke point before any
/// `pcap::Device::list()` call (the #58 delay-load crash guard). BOTH
/// name-based selection (`find_device`, PTP) and NTP-server-reachability
/// selection (`find_device_for_ntp_server`, dantesync#53 continuation) go
/// through this, so neither path can bypass the guard.
fn list_devices_guarded() -> Result<Vec<Device>> {
    // #58: check BEFORE the first delay-loaded pcap:: call (Device::list(),
    // right below), not after -- see `wpcap_runtime_available()`.
    if !wpcap_runtime_available() {
        return Err(anyhow!(
            "Npcap runtime (wpcap.dll) is not installed/loadable -- only the \
             Npcap SDK's link-time stubs are present. Install the Npcap \
             runtime (https://npcap.com) to use Npcap-based capture."
        ));
    }
    Ok(Device::list()?)
}

/// Find the Npcap device matching `interface_name` by name, description, or
/// an IP-address substring in its address list.
///
/// Shared by the PTP capture path (below) — the NTP kernel-timestamped
/// transport (dantesync#53) uses `find_device_for_ntp_server` instead
/// (selection by NTP-server reachability, not by inherited PTP interface
/// name — see that function's doc comment for why).
pub(crate) fn find_device(interface_name: &str) -> Result<Device> {
    let devices = list_devices_guarded()?;
    devices
        .iter()
        .find(|d| {
            d.name.contains(interface_name)
                || d.desc
                    .as_ref()
                    .map(|desc| desc.contains(interface_name))
                    .unwrap_or(false)
        })
        .or_else(|| {
            // Try matching by IP address in description
            devices.iter().find(|d| {
                d.addresses
                    .iter()
                    .any(|addr| format!("{:?}", addr.addr).contains(interface_name))
            })
        })
        .cloned()
        .ok_or_else(|| {
            let available: Vec<String> = devices
                .iter()
                .map(|d| format!("{} ({:?})", d.name, d.desc))
                .collect();
            anyhow!(
                "Interface '{}' not found. Available: {:?}",
                interface_name,
                available
            )
        })
}

/// Select the Npcap device whose IPv4 subnet can actually reach
/// `server_ip` — NOT the PTP capture interface (dantesync#53 continuation).
///
/// Confirmed live on the stream box: `PcapNtpTransport` used to inherit the
/// PTP capture device (`find_device(interface_name)`), which fails outright
/// on a dual-homed host where PTP (Dante) and NTP (LAN) live on different
/// subnets — the NTP server is genuinely unreachable from that device
/// (WSAENETUNREACH / os error 10051). This instead enumerates EVERY Npcap
/// device's IPv4 addresses and picks the one whose subnet contains the
/// (already-resolved) `server_ip`, via the pure, unit-tested
/// `ntp_packet::select_ntp_pcap_device` — the same decision a manual
/// `Find-NetRoute` makes, made automatically.
pub(crate) fn find_device_for_ntp_server(server_ip: Ipv4Addr) -> Result<Device> {
    let devices = list_devices_guarded()?;

    let mut candidates = Vec::new();
    let mut candidate_device_idx = Vec::new();
    for (idx, d) in devices.iter().enumerate() {
        for a in &d.addresses {
            if let (std::net::IpAddr::V4(ip), Some(std::net::IpAddr::V4(netmask))) =
                (a.addr, a.netmask)
            {
                if !ip.is_loopback() {
                    candidates.push(crate::ntp_packet::CandidateInterface {
                        name: d.name.clone(),
                        ip,
                        netmask: Some(netmask),
                    });
                    candidate_device_idx.push(idx);
                }
            }
        }
    }

    match crate::ntp_packet::select_ntp_pcap_device(server_ip, &candidates) {
        Some(ci) => Ok(devices[candidate_device_idx[ci]].clone()),
        None => {
            let considered: Vec<String> = candidates
                .iter()
                .map(|c| format!("{} ({})", c.name, c.ip))
                .collect();
            Err(anyhow!(
                "No Npcap-capturable interface can reach NTP server {} -- considered: [{}] \
                 (dantesync#53: this host may be dual-homed with PTP and NTP on separate \
                 networks)",
                server_ip,
                considered.join(", ")
            ))
        }
    }
}

/// Select which Npcap device to capture PTP on (camera-box issue 1073).
///
/// On a MULTI-HOMED box a restricting `gm_allowlist` names the trusted
/// grandmaster network; this picks the device whose OWN subnet is on that
/// network, via the pure, unit-tested `GmAllowlist::select_interface`. Because
/// `NpcapPtpNetwork::new` drives BOTH the IGMP membership join AND the pcap
/// capture off the chosen device, both then attach to the NIC that actually
/// reaches the rig grandmaster — not whichever NIC `net::get_default_interface`
/// happened to enumerate first.
///
/// Falls back to the historical name-based `find_device(fallback_name)` when the
/// allowlist gives no discriminating signal (unrestricted, or no device on a
/// trusted subnet), so single-homed and no-allowlist boxes are byte-identical to
/// before. This mirrors the dual-homed selection already proven for the NTP
/// transport (`find_device_for_ntp_server`, dantesync#53).
///
/// Returns `(device, Some(matched_ip))` when the allowlist uniquely selects a
/// trusted-subnet interface — `matched_ip` is the EXACT address that matched, so
/// the caller joins the multicast group on it (review 🔵: on a multi-IP NIC the
/// device's first IPv4 could differ from the trusted one). `(device, None)` for
/// the fallback path, where the caller joins on the device's first IPv4.
pub(crate) fn find_ptp_capture_device(
    gm_allowlist: &crate::gm_filter::GmAllowlist,
    fallback_name: &str,
) -> Result<(Device, Option<Ipv4Addr>)> {
    let devices = list_devices_guarded()?;

    // One (ip, netmask) candidate per device IPv4 address, remembering which
    // device each came from (a device can carry several addresses).
    let mut candidates: Vec<(Ipv4Addr, Option<Ipv4Addr>)> = Vec::new();
    let mut candidate_device_idx: Vec<usize> = Vec::new();
    for (idx, d) in devices.iter().enumerate() {
        for a in &d.addresses {
            if let std::net::IpAddr::V4(ip) = a.addr {
                if !ip.is_loopback() {
                    let netmask = match a.netmask {
                        Some(std::net::IpAddr::V4(nm)) => Some(nm),
                        _ => None,
                    };
                    candidates.push((ip, netmask));
                    candidate_device_idx.push(idx);
                }
            }
        }
    }

    let matches = gm_allowlist.best_interface_matches(&candidates);
    // Distinct DEVICES among the best-scoring candidates. A single device with
    // several matching addresses is NOT ambiguity; two different NICs are.
    let mut matched_devices: Vec<usize> =
        matches.iter().map(|&ci| candidate_device_idx[ci]).collect();
    matched_devices.sort_unstable();
    matched_devices.dedup();

    match matched_devices.len() {
        1 => {
            let ci = matches[0];
            let device = devices[candidate_device_idx[ci]].clone();
            let matched_ip = candidates[ci].0;
            info!(
                "camera-box issue 1073: selected PTP capture interface {} ({}) — on the trusted \
                 grandmaster subnet per gm_allowlist (dual-homed-safe)",
                device.name, matched_ip
            );
            return Ok((device, Some(matched_ip)));
        }
        n if n >= 2 => {
            // Review 🟡: an over-broad allowlist (e.g. a /16 spanning both the rig
            // and mbc subnets) matches several distinct NICs equally. Rather than
            // let pcap enumeration order silently decide — and possibly flip a
            // previously-working box to the wrong NIC — keep the OS default
            // interface (the pre-change behavior, never WORSE than before) and
            // warn loudly to narrow the allowlist.
            warn!(
                "camera-box issue 1073: gm_allowlist matches {} distinct interfaces — too broad to \
                 disambiguate the PTP capture interface; keeping the default interface. Narrow the \
                 allowlist to the grandmaster's subnet (e.g. a /24) or its exact IP (/32).",
                n
            );
        }
        _ => {} // 0 — no interface on a trusted subnet; fall through to default.
    }

    let device = find_device(fallback_name)?;
    info!(
        "PTP capture interface by default enumeration: {} ({:?}) — gm_allowlist gave no unambiguous \
         subnet preference (unrestricted, no interface on a trusted subnet, or too broad)",
        device.name, device.desc
    );
    Ok((device, None))
}

/// The device's first non-loopback IPv4 address.
///
/// Shared by the PTP capture path (below) and the NTP kernel-timestamped
/// transport (dantesync#53).
pub(crate) fn device_ipv4(device: &Device) -> Result<Ipv4Addr> {
    device
        .addresses
        .iter()
        .find_map(|a| {
            if let std::net::IpAddr::V4(ip) = a.addr {
                if !ip.is_loopback() {
                    return Some(ip);
                }
            }
            None
        })
        .ok_or_else(|| anyhow!("No IPv4 address found on device"))
}

/// Open an Npcap capture with `HostHighPrec` (`KeQuerySystemTimePrecise()`)
/// timestamps and the given BPF filter applied.
///
/// Shared by the PTP capture path (below) and the NTP kernel-timestamped
/// transport (dantesync#53) — both need the same precise-timestamp capture
/// setup, differing only in which traffic the filter selects.
pub(crate) fn open_hiprec_capture(device: &Device, bpf_filter: &str) -> Result<Capture<Active>> {
    info!("[TS] Requesting HostHighPrec timestamps (KeQuerySystemTimePrecise)");

    let mut capture = Capture::from_device(device.clone())?
        .promisc(false) // Don't use promiscuous - rely on the traffic actually reaching this NIC
        .immediate_mode(true) // Critical: disable buffering for lowest latency
        .snaplen(256) // PTP/NTP packets are both small
        .timeout(1) // 1ms timeout for responsiveness
        .tstamp_type(TimestampType::HostHighPrec)
        .open()?;

    capture.filter(bpf_filter, true)?;
    info!("[Filter] Applied BPF: {}", bpf_filter);
    info!("[TS] Using HostHighPrec timestamps (KeQuerySystemTimePrecise)");

    Ok(capture)
}

/// Convert a pcap capture timestamp (seconds, microseconds) to `SystemTime`.
///
/// Shared by the PTP capture path (below) and the NTP kernel-timestamped
/// transport (dantesync#53).
pub(crate) fn pcap_ts_to_systemtime(ts_sec: i64, ts_usec: i64) -> SystemTime {
    let duration = Duration::new(ts_sec as u64, (ts_usec * 1000) as u32);
    UNIX_EPOCH + duration
}

/// One open PTP capture: the pcap handle, the socket that holds the IGMP membership, and where
/// they were opened.
struct PtpCapture {
    capture: Capture<Active>,
    // Keep the socket alive for IGMP multicast membership (dropped on close);
    // ONE ephemeral-port socket holds the 224.0.1.129 membership (dantesync#109).
    _igmp_sock: UdpSocket,
    /// The Npcap device name (a replaced NIC is another device) and the IPv4 joined on.
    device_name: String,
    iface_ip: Ipv4Addr,
}

/// Select the PTP capture device and open the capture and the IGMP membership on it. The startup
/// and the re-join (dantesync#112) both go through here, so both select the same way.
fn open_ptp_capture(
    interface_name: &str,
    gm_allowlist: &crate::gm_filter::GmAllowlist,
) -> Result<PtpCapture> {
    // camera-box issue 1073: on a multi-homed box prefer the interface on the
    // trusted grandmaster subnet (gm_allowlist); otherwise the historical
    // name-based selection. Both the IGMP join and the capture below use the
    // chosen device, so they land on the NIC that reaches the rig GM.
    let (device, matched_ip) = find_ptp_capture_device(gm_allowlist, interface_name)?;
    info!("Found device: {} ({:?})", device.name, device.desc);

    // Extract interface IP for the multicast join. Prefer the allowlist-MATCHED
    // address (review 🔵: on a multi-IP NIC device_ipv4's first address could
    // differ from the trusted one we selected on); fall back to the device's
    // first IPv4 on the name-based path.
    let iface_ip = match matched_ip {
        Some(ip) => ip,
        None => device_ipv4(&device)?,
    };
    info!("Using interface IP {} for multicast join", iface_ip);

    // CRITICAL: Join the multicast group via ONE ephemeral-port socket to
    // trigger IGMP membership (dantesync#109: NOT bound to 319/320, so a
    // Dante Virtual Soundcard ptp.exe on the same host keeps both PTP ports).
    let igmp_sock = join_multicast(iface_ip)?;
    info!(
        "Joined PTP multicast group 224.0.1.129 on {} via an ephemeral-port IGMP socket \
         (ports 319/320 left free for a DVS ptp.exe on the same host — dantesync#109)",
        iface_ip
    );

    // Apply BPF filter to only capture PTP multicast. The IGMP-join socket
    // above binds an ephemeral port, not 319/320, so DVS keeps exclusive
    // ownership of both PTP ports (dantesync#109); this filter only scopes
    // which packets pcap decodes and never claims a port.
    let ptp_filter = "udp and dst host 224.0.1.129 and (dst port 319 or dst port 320)";
    let capture = open_hiprec_capture(&device, ptp_filter)?;

    Ok(PtpCapture {
        capture,
        _igmp_sock: igmp_sock,
        device_name: device.name,
        iface_ip,
    })
}

/// PTP network using Npcap with HostHighPrec timestamps
pub struct NpcapPtpNetwork {
    /// dantesync#112: `None` after a re-join could not open a capture; nothing is received until
    /// the next attempt.
    open: Option<PtpCapture>,
    using_hiprec: bool,
    /// The default-interface hint the device is selected with (the name-based fallback).
    hint: String,
    /// The trusted-source allowlist the device is selected by (camera-box issue 1073).
    gm_allowlist: crate::gm_filter::GmAllowlist,
    /// `(device, IPv4)` of the last capture that opened: what a re-join's `changed` compares with.
    last_join: (String, Ipv4Addr),
}

impl NpcapPtpNetwork {
    pub fn new(interface_name: &str, gm_allowlist: &crate::gm_filter::GmAllowlist) -> Result<Self> {
        info!(
            "Initializing Npcap capture (default-interface hint: {})",
            interface_name
        );
        let open = open_ptp_capture(interface_name, gm_allowlist)?;

        // Assume HostHighPrec is available on modern Npcap (1.20+)
        let using_hiprec = true;

        if using_hiprec {
            info!("Npcap capture initialized with HIGH PRECISION synchronized timestamps");
        } else {
            warn!("Npcap capture using default timestamps (may drift from system time)");
        }

        let last_join = (open.device_name.clone(), open.iface_ip);
        Ok(NpcapPtpNetwork {
            open: Some(open),
            using_hiprec,
            hint: interface_name.to_string(),
            gm_allowlist: gm_allowlist.clone(),
            last_join,
        })
    }
}

impl crate::traits::PtpNetwork for NpcapPtpNetwork {
    fn recv_packet(&mut self) -> Result<Option<(Vec<u8>, usize, SystemTime, Option<Ipv4Addr>)>> {
        let using_hiprec = self.using_hiprec;
        let Some(open) = self.open.as_mut() else {
            return Ok(None);
        };
        match open.capture.next_packet() {
            Ok(packet) => {
                let data = packet.data;

                // Use Npcap's HostHighPrec timestamps - these are both precise AND synced
                // with system time (using KeQuerySystemTimePrecise on Windows 8+)
                let header = packet.header;
                let ts = if using_hiprec {
                    // Npcap provides high-precision timestamps synced with system time
                    let ts =
                        pcap_ts_to_systemtime(header.ts.tv_sec as i64, header.ts.tv_usec as i64);
                    debug!(
                        "[TS] Npcap HostHighPrec: {}.{:06}",
                        header.ts.tv_sec, header.ts.tv_usec
                    );
                    ts
                } else {
                    // Fallback to SystemTime::now() if HostHighPrec not available
                    SystemTime::now()
                };

                // Extract source IP / dest port / UDP payload from the
                // Ethernet+IPv4+UDP frame (dantesync#53: shared pure parser,
                // also used by the NTP kernel-timestamped transport).
                let Some((source_ip, dst_port, payload)) = crate::ntp_packet::parse_udp_frame(data)
                else {
                    return Ok(None);
                };

                // Check destination port for PTP (319 or 320)
                if dst_port != PTP_EVENT_PORT && dst_port != PTP_GENERAL_PORT {
                    return Ok(None);
                }

                let payload_len = payload.len();

                if payload_len > 0 {
                    let mut result = vec![0u8; payload_len];
                    result.copy_from_slice(payload);

                    debug!(
                        "[Npcap] PTP payload {} bytes from {}",
                        payload_len, source_ip
                    );
                    Ok(Some((result, payload_len, ts, Some(source_ip))))
                } else {
                    Ok(None)
                }
            }
            Err(pcap::Error::TimeoutExpired) => {
                // Normal timeout - no packet available
                Ok(None)
            }
            Err(e) => {
                warn!("Npcap recv error: {} ({:?})", e, e);
                Err(e.into())
            }
        }
    }

    fn reset(&mut self) -> Result<()> {
        // Npcap doesn't need explicit reset
        Ok(())
    }

    /// dantesync#112 — re-open the capture after a NIC swap (a dead handle keeps failing with
    /// ERROR_DEVICE_REMOVED) or any other silence: the same selection as the startup, on the
    /// default interface and the allowlist resolved NOW.
    fn rejoin(&mut self) -> Result<crate::traits::RejoinOutcome> {
        // A replaced NIC is another adapter under another name, so the name-based fallback
        // re-reads the default interface; the old hint stays when none resolves.
        match crate::net::get_default_interface() {
            Ok((name, _)) => self.hint = name,
            Err(e) => warn!(
                "[NET] no default interface ({}); re-joining with the hint {}",
                e, self.hint
            ),
        }
        // Hostname allowlist entries are resolved again, like at startup, so the capture NIC is
        // chosen on the grandmaster's CURRENT subnet.
        if self.gm_allowlist.has_hostnames() {
            let outcome = self.gm_allowlist.resolve(&crate::gm_filter::StdResolver);
            info!(
                "gm_allowlist: re-join hostname resolution {:?} (unresolved: {:?})",
                outcome.new_resolved,
                self.gm_allowlist.unresolved_hostnames()
            );
        }
        // Drop the old (possibly dead) handle and its IGMP membership FIRST. When the new capture
        // cannot open, there is none until the next attempt.
        self.open = None;
        let open = open_ptp_capture(&self.hint, &self.gm_allowlist)?;
        let (iface, ip) = (open.device_name.clone(), open.iface_ip);
        info!("Npcap PTP capture re-opened on {} ({}) (rejoin)", iface, ip);
        let changed = self.last_join.0 != iface || self.last_join.1 != ip;
        self.last_join = (iface.clone(), ip);
        self.open = Some(open);
        Ok(crate::traits::RejoinOutcome { iface, ip, changed })
    }
}

// ============================================================================
// #53 — kernel-timestamped NTP transport (Windows)
// ============================================================================
// The Windows NTP client's offset scattered by tens of milliseconds even
// while this same box's PTP servo (above) reported locked, because
// `NtpClient::measure_once()` (src/ntp.rs) took t1/t4 as userspace
// `SystemTime::now()` calls around a blocking socket — exactly the
// scheduling-jitter problem `NpcapPtpNetwork` above was built to avoid for
// PTP. `PcapNtpTransport` gives NTP the same treatment: a SEPARATE Npcap
// capture (own device lookup, own HostHighPrec timestamps, own BPF filter —
// unicast NTP traffic to one server, nothing to do with the PTP multicast
// group) sees both our own outgoing request leaving the NIC (t1) and the
// server's reply arriving (t4), sidestepping userspace scheduling delay on
// both ends. t2/t3 come from the reply packet's own fields.
//
// This glue is intentionally thin: everything it depends on (packet
// build/parse, the offset/RTT formula, frame parsing) lives in the
// zero-I/O, fully-unit-tested `ntp_packet` module. This file only opens the
// capture, sends the request, and correlates captured packets by direction.
const NTP_PORT: u16 = 123;

/// Upper bound on one pcap-based NTP round trip. NTP checks run every
/// 10-30s in production (`controller.rs`'s adaptive interval) — this bounds
/// the cost of a single check so a lost request or reply can never hang the
/// sync loop (dantesync#53's "bound the cost" requirement).
const NTP_PCAP_TIMEOUT: Duration = Duration::from_millis(500);

/// Kernel-timestamped NTP transport for Windows: captures t1 (our own
/// request leaving the NIC) and t4 (the server's reply arriving) via Npcap
/// `HostHighPrec` timestamps instead of userspace `SystemTime::now()`.
pub struct PcapNtpTransport {
    capture: Capture<Active>,
    socket: UdpSocket,
    server_ip: Ipv4Addr,
    local_ip: Ipv4Addr,
}

impl PcapNtpTransport {
    /// `server_ip` is the already-resolved NTP server address (the BPF
    /// filter needs a concrete IP, not a hostname). The capture device is
    /// selected by which interface can actually REACH `server_ip`
    /// (`find_device_for_ntp_server`, dantesync#53 continuation) — NOT by
    /// inheriting the PTP capture interface, which fails outright on a
    /// dual-homed host where PTP and NTP live on different subnets.
    pub fn new(server_ip: Ipv4Addr, dscp: &crate::dscp::DscpConfig) -> Result<Self> {
        let device = find_device_for_ntp_server(server_ip)?;
        let local_ip = device_ipv4(&device)?;

        let filter = format!("udp and host {} and port {}", server_ip, NTP_PORT);
        let capture = open_hiprec_capture(&device, &filter)?;

        // A plain socket only to SEND the request -- Npcap sees the packet
        // leave the NIC and gives us the real t1, so this socket's own
        // send() timing is irrelevant (that userspace timing is exactly the
        // defect this transport exists to route around).
        let socket = UdpSocket::bind((local_ip, 0))?;
        socket.connect((server_ip, NTP_PORT))?;

        // dantesync#52: mark the NTP client request egress with DSCP. On Windows
        // the OS filters a socket-set IP_TOS (needs a QoS policy at provisioning),
        // so `crate::dscp::apply` is a logged no-op here — wired for completeness
        // and to surface the QoS guidance in the log. See `crate::dscp`.
        crate::dscp::apply(&socket, dscp, "ntp-client request (pcap)");

        info!(
            "[NTP][Npcap] kernel-timestamped NTP transport ready: {} ({}) -> {}",
            device.name, local_ip, server_ip
        );

        Ok(Self {
            capture,
            socket,
            server_ip,
            local_ip,
        })
    }

    /// One kernel-timestamped NTP round trip: send a request, capture our
    /// own outgoing packet (t1) and the server's reply (t4), parse t2/t3
    /// from the reply payload, and compute offset/RTT.
    ///
    /// Adversarial-review fix (#53 continuation): before sending, DRAIN any
    /// packets already queued in the capture (a leftover reply from a prior
    /// `measure_once` call, or the userspace rsntp fallback's own request/
    /// reply exchange to the same server:123, which the open BPF filter also
    /// matches). And a captured reply is only accepted as t4 when its echoed
    /// Origin Timestamp actually matches THIS request's transmit timestamp
    /// (`reply_origin_matches_request`) -- without both of these, a stale or
    /// foreign reply could be silently paired with the current request,
    /// producing a self-consistent but wrong measurement (the exact fat-tail
    /// bug this fixes: healthy median, occasional multi-ms-wrong sample).
    pub fn measure_once(&mut self) -> Result<crate::ntp::RawSample> {
        use crate::ntp_packet::{
            build_client_request, compute_offset_rtt_us, parse_reply, parse_udp_frame,
            reply_origin_matches_request, systemtime_to_unix_micros,
        };

        // Drain anything already queued before this request exists -- a
        // stale reply/request sitting in the capture buffer must never be
        // considered for THIS round trip's t1/t4.
        const MAX_DRAIN_PACKETS: u32 = 64;
        for _ in 0..MAX_DRAIN_PACKETS {
            match self.capture.next_packet() {
                Ok(_) => {
                    debug!("[NTP][Npcap] drained a stale queued packet before sending");
                }
                Err(pcap::Error::TimeoutExpired) => break,
                Err(_) => break, // any other capture error: nothing more to drain
            }
        }

        let request_transmit_ts_us = systemtime_to_unix_micros(SystemTime::now());
        let request = build_client_request(request_transmit_ts_us);
        self.socket.send(&request)?;

        let mut t1_us: Option<i64> = None;
        let mut t4_reply: Option<(i64, crate::ntp_packet::ParsedReply)> = None;
        let deadline = std::time::Instant::now() + NTP_PCAP_TIMEOUT;

        while t4_reply.is_none() && std::time::Instant::now() < deadline {
            match self.capture.next_packet() {
                Ok(packet) => {
                    let Some((src_ip, _dst_port, payload)) = parse_udp_frame(packet.data) else {
                        continue;
                    };
                    let ts = pcap_ts_to_systemtime(
                        packet.header.ts.tv_sec as i64,
                        packet.header.ts.tv_usec as i64,
                    );
                    let ts_us = systemtime_to_unix_micros(ts);

                    if src_ip == self.local_ip && t1_us.is_none() {
                        t1_us = Some(ts_us);
                        debug!("[NTP][Npcap] t1 (our request) captured at {}us", ts_us);
                    } else if src_ip == self.server_ip {
                        match parse_reply(payload) {
                            Ok(reply) => {
                                if reply_origin_matches_request(
                                    reply.origin_ts_us,
                                    request_transmit_ts_us,
                                ) {
                                    t4_reply = Some((ts_us, reply));
                                    debug!(
                                        "[NTP][Npcap] t4 (server reply) captured at {}us",
                                        ts_us
                                    );
                                } else {
                                    debug!(
                                        "[NTP][Npcap] ignoring reply with mismatched origin \
                                         timestamp {}us (our request: {}us) -- stale or foreign \
                                         reply, not paired",
                                        reply.origin_ts_us, request_transmit_ts_us
                                    );
                                }
                            }
                            Err(e) => {
                                debug!(
                                    "[NTP][Npcap] ignoring unparseable server-port packet: {}",
                                    e
                                )
                            }
                        }
                    }
                }
                Err(pcap::Error::TimeoutExpired) => continue,
                Err(e) => return Err(e.into()),
            }
        }

        let t1_us = t1_us.ok_or_else(|| {
            anyhow!(
                "NTP/Npcap: never observed our own outgoing request within {:?}",
                NTP_PCAP_TIMEOUT
            )
        })?;
        let (t4_us, reply) = t4_reply
            .ok_or_else(|| anyhow!("NTP/Npcap: no server reply within {:?}", NTP_PCAP_TIMEOUT))?;

        let (offset_us, rtt_us) =
            compute_offset_rtt_us(t1_us, reply.receive_ts_us, reply.transmit_ts_us, t4_us);

        Ok(crate::ntp::RawSample {
            offset_us,
            rtt_us: rtt_us.max(0) as u64,
        })
    }
}

/// Get list of available Npcap devices.
///
/// Adversarial-review fix (#53 continuation): this used to call
/// `Device::list()` directly, bypassing `list_devices_guarded()` -- making
/// that function's own doc comment ("the SINGLE choke point ... so neither
/// path can bypass the guard") false. Zero live callers today, so there was
/// no real #58 crash exposure yet, but a future caller (e.g. a `--list-devices`
/// CLI flag) would have hit the exact unguarded delay-load crash #58 fixed
/// for every other pcap:: entry point. Routed through the guard instead of
/// just correcting the comment.
pub fn list_npcap_devices() -> Result<Vec<String>> {
    let devices = list_devices_guarded()?;
    Ok(devices
        .iter()
        .map(|d| format!("{}: {:?}", d.name, d.desc))
        .collect())
}

#[cfg(test)]
mod tests;
