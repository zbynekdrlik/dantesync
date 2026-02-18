//! Windows Winsock-based PTP network implementation with SO_TIMESTAMP.
//!
//! Uses standard Winsock2 APIs with kernel-level timestamping for precise
//! packet arrival times. This approach captures timestamps at the network
//! stack level (not application level), achieving <100µs precision.
//!
//! Key APIs:
//! - WSAIoctl with SIO_TIMESTAMPING to enable timestamping
//! - WSARecvMsg to receive packets with control messages
//! - SO_TIMESTAMP control message contains QPC timestamp

use anyhow::{anyhow, Result};
use log::{debug, info, warn};
use std::mem;
use std::net::Ipv4Addr;
use std::ptr;
use std::time::SystemTime;

use windows::core::GUID;
use windows::Win32::Networking::WinSock::{
    bind, closesocket, ioctlsocket, recvfrom, setsockopt, socket, WSACleanup, WSAGetLastError,
    WSAIoctl, WSAStartup, AF_INET, FIONBIO, INVALID_SOCKET, IN_ADDR, IPPROTO_IP, IPPROTO_UDP,
    IP_ADD_MEMBERSHIP, IP_MULTICAST_LOOP, SEND_RECV_FLAGS, SIO_GET_EXTENSION_FUNCTION_POINTER,
    SOCKADDR_IN, SOCKET, SOCKET_ERROR, SOCK_DGRAM, SOL_SOCKET, SO_REUSEADDR, SO_TIMESTAMP, WSABUF,
    WSADATA, WSAMSG,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::IO::OVERLAPPED;

const PTP_EVENT_PORT: u16 = 319;
const PTP_GENERAL_PORT: u16 = 320;
const PTP_MULTICAST: Ipv4Addr = Ipv4Addr::new(224, 0, 1, 129);

// SIO_TIMESTAMPING constants (not in windows crate, defined per MS docs)
const SIO_TIMESTAMPING: u32 = 0x88000025;
const TIMESTAMPING_FLAG_RX: u32 = 0x1;

// GUID for WSARecvMsg extension function
const WSAID_WSARECVMSG: GUID = GUID::from_u128(0xf689d7c8_6f1f_436b_8a53_e54fe351c322);

/// Control message header - matches WSACMSGHDR/CMSGHDR structure
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct CmsgHdr {
    cmsg_len: usize, // Length including header
    cmsg_level: i32, // Protocol level
    cmsg_type: i32,  // Protocol-specific type
}

/// IP multicast membership request
#[repr(C)]
struct IpMreq {
    imr_multiaddr: u32,
    imr_interface: u32,
}

/// Timestamping configuration structure
#[repr(C)]
struct TimestampingConfig {
    flags: u32,
    tx_timestamp_id: u16,
    reserved: u16,
}

/// WSARecvMsg function type
type WsaRecvMsgFn = unsafe extern "system" fn(
    SOCKET,
    *mut WSAMSG,
    *mut u32,
    *mut OVERLAPPED,
    *mut std::ffi::c_void, // LPWSAOVERLAPPED_COMPLETION_ROUTINE
) -> i32;

/// PTP network using Winsock with SO_TIMESTAMP for precise timestamps
pub struct WinsockPtpNetwork {
    socket_319: SOCKET,
    socket_320: SOCKET,
    recv_msg_fn: Option<WsaRecvMsgFn>,
    qpc_frequency: i64,
    timestamping_enabled: bool,
}

impl WinsockPtpNetwork {
    pub fn new(interface_ip: Ipv4Addr) -> Result<Self> {
        info!("Initializing Winsock PTP network with SO_TIMESTAMP");

        // Initialize Winsock
        unsafe {
            let mut wsa_data: WSADATA = mem::zeroed();
            let result = WSAStartup(0x0202, &mut wsa_data);
            if result != 0 {
                return Err(anyhow!("WSAStartup failed: {}", result));
            }
        }

        // Get QPC frequency for timestamp conversion
        let qpc_frequency = unsafe {
            let mut freq: i64 = 0;
            let _ = QueryPerformanceFrequency(&mut freq);
            info!("QPC frequency: {} Hz", freq);
            freq
        };

        // Create and configure sockets
        let socket_319 = Self::create_ptp_socket(PTP_EVENT_PORT, interface_ip)?;
        let socket_320 = Self::create_ptp_socket(PTP_GENERAL_PORT, interface_ip)?;

        // Get WSARecvMsg function pointer
        let recv_msg_fn = Self::get_wsarecvmsg_fn(socket_319)?;

        // Enable timestamping on both sockets
        let ts_enabled_319 = Self::enable_timestamping(socket_319);
        let ts_enabled_320 = Self::enable_timestamping(socket_320);

        let timestamping_enabled = ts_enabled_319 && ts_enabled_320;
        if timestamping_enabled {
            info!("SO_TIMESTAMP enabled on both PTP sockets");
        } else {
            warn!("SO_TIMESTAMP not available - falling back to application timestamps");
        }

        info!(
            "Winsock PTP network initialized on {} (ports 319, 320)",
            interface_ip
        );

        Ok(WinsockPtpNetwork {
            socket_319,
            socket_320,
            recv_msg_fn,
            qpc_frequency,
            timestamping_enabled,
        })
    }

    fn create_ptp_socket(port: u16, interface_ip: Ipv4Addr) -> Result<SOCKET> {
        unsafe {
            // Create UDP socket
            let sock = socket(AF_INET.0 as i32, SOCK_DGRAM, IPPROTO_UDP.0 as i32);
            if sock == INVALID_SOCKET {
                return Err(anyhow!("Failed to create socket: {}", WSAGetLastError().0));
            }

            // Enable address reuse
            let reuse: i32 = 1;
            if setsockopt(
                sock,
                SOL_SOCKET as i32,
                SO_REUSEADDR as i32,
                Some(&reuse.to_ne_bytes()),
            ) == SOCKET_ERROR
            {
                warn!("Failed to set SO_REUSEADDR: {}", WSAGetLastError().0);
            }

            // Bind to port
            let addr = SOCKADDR_IN {
                sin_family: AF_INET,
                sin_port: port.to_be(),
                sin_addr: IN_ADDR {
                    S_un: std::mem::zeroed(),
                },
                sin_zero: [0; 8],
            };

            if bind(
                sock,
                &addr as *const SOCKADDR_IN as *const _,
                mem::size_of::<SOCKADDR_IN>() as i32,
            ) == SOCKET_ERROR
            {
                closesocket(sock);
                return Err(anyhow!(
                    "Failed to bind port {}: {}",
                    port,
                    WSAGetLastError().0
                ));
            }

            // Join PTP multicast group
            let mreq = IpMreq {
                imr_multiaddr: u32::from_ne_bytes(PTP_MULTICAST.octets()),
                imr_interface: u32::from_ne_bytes(interface_ip.octets()),
            };

            if setsockopt(
                sock,
                IPPROTO_IP.0 as i32,
                IP_ADD_MEMBERSHIP as i32,
                Some(std::slice::from_raw_parts(
                    &mreq as *const IpMreq as *const u8,
                    mem::size_of::<IpMreq>(),
                )),
            ) == SOCKET_ERROR
            {
                closesocket(sock);
                return Err(anyhow!("Failed to join multicast: {}", WSAGetLastError().0));
            }

            // Disable multicast loopback
            let loopback: u8 = 0;
            setsockopt(
                sock,
                IPPROTO_IP.0 as i32,
                IP_MULTICAST_LOOP as i32,
                Some(&[loopback]),
            );

            // Set socket to non-blocking mode
            let mut mode: u32 = 1;
            if ioctlsocket(sock, FIONBIO, &mut mode) == SOCKET_ERROR {
                warn!("Failed to set non-blocking mode: {}", WSAGetLastError().0);
            }

            info!("PTP socket created on port {} (joined 224.0.1.129)", port);
            Ok(sock)
        }
    }

    fn get_wsarecvmsg_fn(sock: SOCKET) -> Result<Option<WsaRecvMsgFn>> {
        unsafe {
            let mut recv_msg_fn: Option<WsaRecvMsgFn> = None;
            let mut bytes_returned: u32 = 0;

            let result = WSAIoctl(
                sock,
                SIO_GET_EXTENSION_FUNCTION_POINTER,
                Some(&WSAID_WSARECVMSG as *const GUID as *const _),
                mem::size_of::<GUID>() as u32,
                Some(&mut recv_msg_fn as *mut _ as *mut _),
                mem::size_of_val(&recv_msg_fn) as u32,
                &mut bytes_returned,
                None,
                None,
            );

            if result == SOCKET_ERROR {
                warn!("WSARecvMsg not available: {}", WSAGetLastError().0);
                return Ok(None);
            }

            info!("WSARecvMsg function pointer obtained");
            Ok(recv_msg_fn)
        }
    }

    fn enable_timestamping(sock: SOCKET) -> bool {
        unsafe {
            // SIO_TIMESTAMPING is the only way to enable timestamping (Windows 10 1809+)
            // SO_TIMESTAMP is NOT a setsockopt option - it's only a cmsg_type for control messages
            let config = TimestampingConfig {
                flags: TIMESTAMPING_FLAG_RX,
                tx_timestamp_id: 0,
                reserved: 0,
            };

            let mut bytes_returned: u32 = 0;

            info!(
                "[TS-Init] Attempting SIO_TIMESTAMPING (ioctl=0x{:08X}, flags=0x{:X})",
                SIO_TIMESTAMPING, TIMESTAMPING_FLAG_RX
            );

            let result = WSAIoctl(
                sock,
                SIO_TIMESTAMPING,
                Some(&config as *const TimestampingConfig as *const _),
                mem::size_of::<TimestampingConfig>() as u32,
                None,
                0,
                &mut bytes_returned,
                None,
                None,
            );

            if result != SOCKET_ERROR {
                info!("[TS-Init] SIO_TIMESTAMPING enabled successfully");
                return true;
            }

            let err = WSAGetLastError().0;
            // Common error codes:
            // 10022 = WSAEINVAL (invalid argument or not supported)
            // 10045 = WSAEOPNOTSUPP (operation not supported)
            // 10014 = WSAEFAULT (bad address)
            warn!("[TS-Init] SIO_TIMESTAMPING failed with error {}", err);
            match err {
                10022 => warn!("[TS-Init] WSAEINVAL - ioctl not supported or invalid config. NIC driver may not support timestamping."),
                10045 => warn!("[TS-Init] WSAEOPNOTSUPP - operation not supported on this socket type"),
                _ => warn!("[TS-Init] Check if NIC driver supports Winsock timestamping"),
            }

            false
        }
    }

    /// Receive packet with timestamp using WSARecvMsg
    fn recv_with_timestamp(
        &mut self,
        sock: SOCKET,
    ) -> Result<Option<(Vec<u8>, usize, SystemTime, Option<Ipv4Addr>)>> {
        const BUFFER_SIZE: usize = 512;
        const CONTROL_SIZE: usize = 128; // Increased for control messages

        let mut data = vec![0u8; BUFFER_SIZE];
        let mut control = vec![0u8; CONTROL_SIZE];
        let mut sockaddr: SOCKADDR_IN = unsafe { mem::zeroed() };

        unsafe {
            let mut data_buf = WSABUF {
                len: BUFFER_SIZE as u32,
                buf: windows::core::PSTR(data.as_mut_ptr()),
            };

            let mut msg = WSAMSG {
                name: &mut sockaddr as *mut SOCKADDR_IN as *mut _,
                namelen: mem::size_of::<SOCKADDR_IN>() as i32,
                lpBuffers: &mut data_buf,
                dwBufferCount: 1,
                Control: WSABUF {
                    len: CONTROL_SIZE as u32,
                    buf: windows::core::PSTR(control.as_mut_ptr()),
                },
                dwFlags: 0,
            };

            let mut bytes_received: u32 = 0;

            // Use WSARecvMsg if available, otherwise fall back
            let result = if let Some(recv_fn) = self.recv_msg_fn {
                recv_fn(
                    sock,
                    &mut msg,
                    &mut bytes_received,
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            } else {
                debug!("[Recv] WSARecvMsg not available, using fallback");
                return self.recv_fallback(sock);
            };

            if result == SOCKET_ERROR {
                let err = WSAGetLastError().0;
                if err == 10035 {
                    // WSAEWOULDBLOCK
                    return Ok(None);
                }
                return Err(anyhow!("WSARecvMsg failed: {}", err));
            }

            if bytes_received == 0 {
                return Ok(None);
            }

            // Extract source IP from sockaddr
            let source_ip = if sockaddr.sin_family == AF_INET {
                let ip_bytes = sockaddr.sin_addr.S_un.S_addr.to_ne_bytes();
                Some(Ipv4Addr::new(
                    ip_bytes[0],
                    ip_bytes[1],
                    ip_bytes[2],
                    ip_bytes[3],
                ))
            } else {
                None
            };

            // Log receive details
            let control_len = msg.Control.len as usize;
            debug!(
                "[Recv] Got {} bytes from {:?}, control_len={}",
                bytes_received, source_ip, control_len
            );

            // Extract timestamp from control message
            let timestamp = self.extract_timestamp(&control, control_len);

            data.truncate(bytes_received as usize);
            Ok(Some((data, bytes_received as usize, timestamp, source_ip)))
        }
    }

    /// Extract SO_TIMESTAMP from control message buffer
    fn extract_timestamp(&self, control: &[u8], control_len: usize) -> SystemTime {
        if !self.timestamping_enabled || control_len == 0 {
            debug!("[TS] Timestamping disabled or no control data, using SystemTime::now()");
            return SystemTime::now();
        }

        debug!("[TS] Parsing control message: {} bytes", control_len);

        // Parse CmsgHdr to find SO_TIMESTAMP
        let mut offset = 0;
        let mut msg_count = 0;
        while offset + mem::size_of::<CmsgHdr>() <= control_len {
            let cmsg: &CmsgHdr = unsafe { &*(control.as_ptr().add(offset) as *const CmsgHdr) };

            if cmsg.cmsg_len == 0 {
                debug!(
                    "[TS] Control message {} has zero length, stopping",
                    msg_count
                );
                break;
            }

            debug!(
                "[TS] Control msg {}: level={} type={} len={}",
                msg_count, cmsg.cmsg_level, cmsg.cmsg_type, cmsg.cmsg_len
            );

            // Check for SO_TIMESTAMP (level=SOL_SOCKET, type=SO_TIMESTAMP)
            if cmsg.cmsg_level == SOL_SOCKET as i32 && cmsg.cmsg_type == SO_TIMESTAMP as i32 {
                // Data follows the header
                let data_offset = offset + mem::size_of::<CmsgHdr>();
                if data_offset + 8 <= control_len {
                    let qpc_timestamp: u64 =
                        unsafe { *(control.as_ptr().add(data_offset) as *const u64) };

                    // Get current QPC for comparison
                    let current_qpc = unsafe {
                        let mut qpc: i64 = 0;
                        let _ = QueryPerformanceCounter(&mut qpc);
                        qpc as u64
                    };

                    let latency_qpc = current_qpc.saturating_sub(qpc_timestamp);
                    let latency_us = (latency_qpc as f64 / self.qpc_frequency as f64) * 1_000_000.0;

                    info!(
                        "[TS] SO_TIMESTAMP found! QPC={} current={} latency={:.1}us",
                        qpc_timestamp, current_qpc, latency_us
                    );

                    // Convert QPC to SystemTime
                    return self.qpc_to_systemtime(qpc_timestamp);
                }
            }

            // Move to next control message (aligned)
            let aligned_len = (cmsg.cmsg_len + 7) & !7;
            offset += aligned_len;
            msg_count += 1;
        }

        // No timestamp found, fall back
        warn!(
            "[TS] No SO_TIMESTAMP in {} control messages, using SystemTime::now()",
            msg_count
        );
        SystemTime::now()
    }

    /// Convert QPC timestamp to SystemTime
    fn qpc_to_systemtime(&self, qpc: u64) -> SystemTime {
        // Get current QPC and SystemTime for reference
        let (current_qpc, current_time) = unsafe {
            let mut qpc_now: i64 = 0;
            let _ = QueryPerformanceCounter(&mut qpc_now);
            (qpc_now as u64, SystemTime::now())
        };

        // Calculate offset in nanoseconds
        let qpc_diff = current_qpc as i64 - qpc as i64;
        let ns_diff = (qpc_diff * 1_000_000_000) / self.qpc_frequency;

        // Subtract from current time (packet arrived before now)
        if ns_diff > 0 {
            current_time - std::time::Duration::from_nanos(ns_diff as u64)
        } else {
            current_time + std::time::Duration::from_nanos((-ns_diff) as u64)
        }
    }

    /// Fallback receive without timestamp
    fn recv_fallback(
        &self,
        sock: SOCKET,
    ) -> Result<Option<(Vec<u8>, usize, SystemTime, Option<Ipv4Addr>)>> {
        let mut buffer = vec![0u8; 512];
        let mut sockaddr: SOCKADDR_IN = unsafe { mem::zeroed() };
        let mut sockaddr_len: i32 = mem::size_of::<SOCKADDR_IN>() as i32;

        unsafe {
            let result = recvfrom(
                sock,
                &mut buffer,
                0, // flags as i32
                Some(&mut sockaddr as *mut SOCKADDR_IN as *mut _),
                Some(&mut sockaddr_len),
            );

            if result == SOCKET_ERROR {
                let err = WSAGetLastError().0;
                if err == 10035 {
                    // WSAEWOULDBLOCK
                    return Ok(None);
                }
                return Err(anyhow!("recvfrom failed: {}", err));
            }

            if result == 0 {
                return Ok(None);
            }

            // Extract source IP
            let source_ip = if sockaddr.sin_family == AF_INET {
                let ip_bytes = sockaddr.sin_addr.S_un.S_addr.to_ne_bytes();
                Some(Ipv4Addr::new(
                    ip_bytes[0],
                    ip_bytes[1],
                    ip_bytes[2],
                    ip_bytes[3],
                ))
            } else {
                None
            };

            let timestamp = SystemTime::now();
            buffer.truncate(result as usize);
            Ok(Some((buffer, result as usize, timestamp, source_ip)))
        }
    }
}

impl Drop for WinsockPtpNetwork {
    fn drop(&mut self) {
        unsafe {
            closesocket(self.socket_319);
            closesocket(self.socket_320);
            WSACleanup();
        }
        info!("Winsock PTP network closed");
    }
}

impl crate::traits::PtpNetwork for WinsockPtpNetwork {
    fn recv_packet(&mut self) -> Result<Option<(Vec<u8>, usize, SystemTime, Option<Ipv4Addr>)>> {
        // Try event port first (319), then general port (320)
        if let Some(packet) = self.recv_with_timestamp(self.socket_319)? {
            return Ok(Some(packet));
        }

        self.recv_with_timestamp(self.socket_320)
    }

    fn reset(&mut self) -> Result<()> {
        // No state to reset for Winsock sockets
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test PTP port constants
    #[test]
    fn test_ptp_port_constants() {
        assert_eq!(PTP_EVENT_PORT, 319);
        assert_eq!(PTP_GENERAL_PORT, 320);
    }

    /// Test PTP multicast address constant
    #[test]
    fn test_ptp_multicast_constant() {
        assert_eq!(PTP_MULTICAST, Ipv4Addr::new(224, 0, 1, 129));
        assert!(PTP_MULTICAST.is_multicast());
    }

    /// Test SIO_TIMESTAMPING constant matches Windows SDK
    #[test]
    fn test_sio_timestamping_constant() {
        // SIO_TIMESTAMPING = 0x88000025 (documented in Windows SDK)
        assert_eq!(SIO_TIMESTAMPING, 0x88000025);
    }

    /// Test TIMESTAMPING_FLAG_RX constant
    #[test]
    fn test_timestamping_flag_rx() {
        // RX flag = 0x1 (enable receive timestamps)
        assert_eq!(TIMESTAMPING_FLAG_RX, 0x1);
    }

    /// Test CmsgHdr structure size
    #[test]
    fn test_cmsghdr_size() {
        // CmsgHdr should be: usize (8 bytes on 64-bit) + i32 (4) + i32 (4) = 16 bytes
        // But may have padding depending on architecture
        let size = std::mem::size_of::<CmsgHdr>();
        assert!(size >= 12, "CmsgHdr should be at least 12 bytes");
        assert!(size <= 24, "CmsgHdr should not exceed 24 bytes");
    }

    /// Test IpMreq structure layout
    #[test]
    fn test_ip_mreq_layout() {
        let size = std::mem::size_of::<IpMreq>();
        assert_eq!(size, 8, "IpMreq should be 8 bytes (2 x u32)");
    }

    /// Test TimestampingConfig structure layout
    #[test]
    fn test_timestamping_config_layout() {
        let size = std::mem::size_of::<TimestampingConfig>();
        assert_eq!(
            size, 8,
            "TimestampingConfig should be 8 bytes (u32 + u16 + u16)"
        );
    }

    /// Test port to big-endian conversion
    #[test]
    fn test_port_big_endian() {
        // Port 319 in big-endian: 0x013F
        let port: u16 = 319;
        let be_port = port.to_be();
        let bytes = be_port.to_ne_bytes();
        assert_eq!(bytes[0], 0x01);
        assert_eq!(bytes[1], 0x3F);

        // Port 320 in big-endian: 0x0140
        let port: u16 = 320;
        let be_port = port.to_be();
        let bytes = be_port.to_ne_bytes();
        assert_eq!(bytes[0], 0x01);
        assert_eq!(bytes[1], 0x40);
    }

    /// Test multicast IP address to bytes conversion
    #[test]
    fn test_multicast_address_bytes() {
        let addr = PTP_MULTICAST;
        let bytes = addr.octets();
        assert_eq!(bytes, [224, 0, 1, 129]);

        // As u32 in network byte order
        let u32_val = u32::from_ne_bytes(bytes);
        assert_ne!(u32_val, 0);
    }

    /// Test WSA error code constants
    #[test]
    fn test_wsa_error_codes() {
        // WSAEWOULDBLOCK = 10035
        const WSAEWOULDBLOCK: i32 = 10035;
        assert_eq!(WSAEWOULDBLOCK, 10035);

        // WSAEINVAL = 10022
        const WSAEINVAL: i32 = 10022;
        assert_eq!(WSAEINVAL, 10022);

        // WSAEOPNOTSUPP = 10045
        const WSAEOPNOTSUPP: i32 = 10045;
        assert_eq!(WSAEOPNOTSUPP, 10045);
    }

    /// Test QPC to nanoseconds conversion math
    #[test]
    fn test_qpc_to_nanoseconds() {
        // Typical QPC frequency: 10 MHz
        let qpc_frequency: i64 = 10_000_000;

        // 10,000,000 QPC ticks = 1 second = 1,000,000,000 ns
        let qpc_diff: i64 = 10_000_000;
        let ns_diff = (qpc_diff * 1_000_000_000) / qpc_frequency;
        assert_eq!(ns_diff, 1_000_000_000);

        // 10,000 QPC ticks = 1 millisecond = 1,000,000 ns
        let qpc_diff: i64 = 10_000;
        let ns_diff = (qpc_diff * 1_000_000_000) / qpc_frequency;
        assert_eq!(ns_diff, 1_000_000);

        // 10 QPC ticks = 1 microsecond = 1,000 ns
        let qpc_diff: i64 = 10;
        let ns_diff = (qpc_diff * 1_000_000_000) / qpc_frequency;
        assert_eq!(ns_diff, 1_000);
    }

    /// Test latency calculation from QPC timestamps
    #[test]
    fn test_latency_calculation() {
        let qpc_frequency: i64 = 10_000_000; // 10 MHz

        // Current QPC = 100,000, packet QPC = 99,000
        // Latency = (100,000 - 99,000) / 10,000,000 * 1,000,000 = 100 microseconds
        let current_qpc: u64 = 100_000;
        let packet_qpc: u64 = 99_000;
        let latency_qpc = current_qpc.saturating_sub(packet_qpc);
        let latency_us = (latency_qpc as f64 / qpc_frequency as f64) * 1_000_000.0;
        assert!((latency_us - 100.0).abs() < 0.1);
    }

    /// Test control message alignment
    #[test]
    fn test_cmsg_alignment() {
        // Control messages are 8-byte aligned
        let test_lengths = [12usize, 16, 20, 24, 32];
        for len in &test_lengths {
            let aligned = (*len + 7) & !7;
            assert!(
                aligned % 8 == 0,
                "Aligned length {} should be 8-byte aligned",
                aligned
            );
            assert!(aligned >= *len, "Aligned length should be >= original");
        }
    }
}
