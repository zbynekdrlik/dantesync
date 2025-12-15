use anyhow::Result;
use log::{info, warn, error, debug};
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};
use std::process::Command;
use crate::clock::SystemClock;
use crate::traits::{NtpSource, PtpNetwork};
use crate::ptp::{PtpV1Header, PtpV1Control, PtpV1FollowUpBody};
use crate::servo::PiServo;
#[cfg(unix)]
use crate::rtc;

// Constants
const MIN_DELTA_NS: i64 = 1_000_000;       // 1ms
const MAX_DELTA_NS: i64 = 2_000_000_000;   // 2s
const MAX_PHASE_OFFSET_FOR_STEP_NS: i64 = 1_000_000; // 1ms
const RTC_UPDATE_INTERVAL: Duration = Duration::from_secs(600); // 10 minutes

pub struct PtpController<C, N, S> 
where 
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource
{
    clock: C,
    network: N,
    ntp: S,
    servo: PiServo,
    
    // State
    pending_syncs: HashMap<u16, PendingSync>,
    prev_t1_ns: i64,
    prev_t2_ns: i64,
    
    // Metrics
    last_phase_offset_ns: i64,
    last_adj_ppm: f64,
    
    // Epoch Alignment
    initial_epoch_offset_ns: i64, // t2 - t1 at first lock
    epoch_aligned: bool,
    
    // RTC
    last_rtc_update: Instant,

    valid_count: usize,
    clock_settled: bool,
    settling_threshold: usize,
}

struct PendingSync {
    rx_time_sys: SystemTime,
    source_uuid: [u8; 6],
}

impl<C, N, S> PtpController<C, N, S>
where 
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource
{
    pub fn new(clock: C, network: N, ntp: S) -> Self {
        PtpController {
            clock,
            network,
            ntp,
            servo: PiServo::new(0.8, 0.2),
            pending_syncs: HashMap::new(),
            prev_t1_ns: 0,
            prev_t2_ns: 0,
            last_phase_offset_ns: 0,
            last_adj_ppm: 0.0,
            initial_epoch_offset_ns: 0,
            epoch_aligned: false,
            last_rtc_update: Instant::now(), 
            valid_count: 0,
            clock_settled: false,
            settling_threshold: 1, 
        }
    }

    pub fn run_ntp_sync(&mut self, skip: bool) {
        if skip { return; }
        
        match self.ntp.get_offset() {
            Ok((offset, sign)) => {
                let sign_str = if sign > 0 { "+" } else { "-" };
                info!("NTP Offset: {}{:?}", sign_str, offset);
                
                if offset.as_millis() > 50 {
                    info!("Stepping clock (NTP)...");
                    if let Err(e) = self.clock.step_clock(offset, sign) {
                        error!("Failed to step clock: {}", e);
                    } else {
                        info!("Clock stepped successfully.");
                    }
                } else {
                    info!("Clock offset small, skipping step.");
                }
            }
            Err(e) => {
                warn!("NTP Sync failed: {}", e);
            }
        }
    }

    pub fn log_status(&self) {
        if !self.clock_settled {
            info!("[Status] Settling... ({}/{}) Waiting for valid PTP pairs...", self.valid_count, self.settling_threshold);
        } else {
            let phase_offset_us = self.last_phase_offset_ns as f64 / 1_000.0;
            let action_str = if self.last_adj_ppm.abs() < 0.01 {
                format!("Locked (Stable)")
            } else if self.last_adj_ppm > 0.0 {
                format!("Speeding up ({:+.3} ppm)", self.last_adj_ppm)
            } else {
                format!("Slowing down ({:+.3} ppm)", self.last_adj_ppm)
            };
            
            let factor = 1.0 + (self.last_adj_ppm / 1_000_000.0);

            info!("[Status] {} | Phase Offset: {:.3} µs | Factor: {:.9}", 
                action_str, phase_offset_us, factor);
        }
    }

    fn update_rtc(&mut self) {
        if self.last_rtc_update.elapsed() > RTC_UPDATE_INTERVAL {
            self.perform_rtc_update();
            self.last_rtc_update = Instant::now();
        }
    }
    
    fn perform_rtc_update(&self) {
        #[cfg(unix)]
        {
            info!("Updating RTC hardware clock (via ioctl)...");
            if let Err(e) = rtc::update_rtc(SystemTime::now()) {
                warn!("Failed to update RTC: {}", e);
            } else {
                info!("RTC updated successfully.");
            }
        }
        #[cfg(not(unix))]
        {
            // Windows fallback
        }
    }

    pub fn process_loop_iteration(&mut self) -> Result<()> {
        let (buf, size, t2) = match self.network.recv_packet()? {
            Some(res) => res,
            None => return Ok(()),
        };
        
        if size < PtpV1Header::SIZE {
            return Ok(());
        }

        let header = match PtpV1Header::parse(&buf[..size]) {
            Ok(h) => h,
            Err(_) => return Ok(()),
        };

        match header.message_type {
            PtpV1Control::Sync => {
                self.pending_syncs.insert(header.sequence_id, PendingSync {
                    rx_time_sys: t2,
                    source_uuid: header.source_uuid,
                });
            }
            PtpV1Control::FollowUp => {
                if let Ok(body) = PtpV1FollowUpBody::parse(&buf[PtpV1Header::SIZE..size]) {
                    if let Some(sync_info) = self.pending_syncs.remove(&body.associated_sequence_id) {
                        if sync_info.source_uuid == header.source_uuid {
                            self.handle_sync_pair(body.precise_origin_timestamp.to_nanos(), sync_info.rx_time_sys);
                        }
                    }
                }
            }
            _ => {}
        }
        
        if self.pending_syncs.len() > 100 {
             let now_sys = SystemTime::now();
             self.pending_syncs.retain(|_, v| now_sys.duration_since(v.rx_time_sys).unwrap_or(Duration::ZERO) < Duration::from_secs(5));
        }
        
        Ok(())
    }

    fn handle_sync_pair(&mut self, t1_ns: i64, t2_sys: SystemTime) {
         let t2_ns = t2_sys.duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as i64;
        
        let mut phase_offset_ns = (t2_ns % 1_000_000_000) - (t1_ns % 1_000_000_000);
        if phase_offset_ns > 500_000_000 { phase_offset_ns -= 1_000_000_000; }
        else if phase_offset_ns < -500_000_000 { phase_offset_ns += 1_000_000_000; }

        self.last_phase_offset_ns = phase_offset_ns;

        if self.prev_t1_ns > 0 && self.prev_t2_ns > 0 {
            let delta_master = t1_ns - self.prev_t1_ns;
            let delta_slave = t2_ns - self.prev_t2_ns;
            
            if delta_master < MIN_DELTA_NS || delta_master > MAX_DELTA_NS ||
               delta_slave < MIN_DELTA_NS || delta_slave > MAX_DELTA_NS {
                warn!("Delta out of range. Skipping.");
                self.prev_t1_ns = t1_ns;
                self.prev_t2_ns = t2_ns;
                return;
            }
        }

        self.valid_count += 1;
        if self.valid_count >= self.settling_threshold {
            if !self.clock_settled {
                self.clock_settled = true;
                self.initial_epoch_offset_ns = t2_ns - t1_ns;
                self.epoch_aligned = true;

                if phase_offset_ns.abs() > MAX_PHASE_OFFSET_FOR_STEP_NS {
                    info!("Initial Phase Offset {}ms is large. Stepping clock to align phase...", phase_offset_ns / 1_000_000);
                    let step_duration = Duration::from_nanos(phase_offset_ns.abs() as u64);
                    let sign = if phase_offset_ns > 0 { -1 } else { 1 };
                    if let Err(e) = self.clock.step_clock(step_duration, sign) {
                        error!("Failed to step clock for phase alignment: {}", e);
                    } else {
                        info!("Phase step complete.");
                        self.reset_filter();
                        self.servo.reset();
                        return;
                    }
                }
                
                info!("Sync established. Updating RTC...");
                self.update_rtc_now();
            }

            let adj_ppm = self.servo.sample(phase_offset_ns);
            self.last_adj_ppm = adj_ppm;
            
            let factor = 1.0 + (adj_ppm / 1_000_000.0);
            
            if let Err(e) = self.clock.adjust_frequency(factor) {
                warn!("Clock adjustment failed: {}", e);
            }
            
            self.update_rtc();
        }
        
        self.prev_t1_ns = t1_ns;
        self.prev_t2_ns = t2_ns;
    }
    
    fn update_rtc_now(&mut self) {
        self.perform_rtc_update();
        self.last_rtc_update = Instant::now(); 
    }
    
    fn reset_filter(&mut self) {
        self.valid_count = 0;
        self.clock_settled = false;
        self.prev_t1_ns = 0;
        self.prev_t2_ns = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::MockSystemClock;
    use crate::traits::{MockNtpSource, MockPtpNetwork};
    use mockall::predicate::*;
    use mockall::Sequence;

    #[test]
    fn test_ntp_sync_trigger() {
        let _ = env_logger::builder().is_test(true).try_init();
        let mut mock_clock = MockSystemClock::new();
        let mut mock_net = MockPtpNetwork::new();
        let mut mock_ntp = MockNtpSource::new();

        mock_ntp.expect_get_offset()
            .times(1)
            .returning(|| Ok((Duration::from_millis(100), 1)));

        mock_clock.expect_step_clock()
            .with(eq(Duration::from_millis(100)), eq(1))
            .times(1)
            .returning(|_, _| Ok(()));

        let mut controller = PtpController::new(mock_clock, mock_net, mock_ntp);
        controller.run_ntp_sync(false);
    }

    #[test]
    fn test_process_sync_followup_flow_with_servo() {
        let _ = env_logger::builder().is_test(true).try_init();
        let mut mock_clock = MockSystemClock::new();
        let mut mock_net = MockPtpNetwork::new();
        let mock_ntp = MockNtpSource::new();

        let mut sync_pkt = vec![0u8; 36];
        sync_pkt[0] = 0x10;
        sync_pkt[30] = 0x00; sync_pkt[31] = 0x01; 
        sync_pkt[32] = 0; 
        sync_pkt[22] = 0xAA; 

        let mut fu_pkt = vec![0u8; 36 + 16];
        fu_pkt[0] = 0x10;
        fu_pkt[30] = 0x00; fu_pkt[31] = 0x02;
        fu_pkt[32] = 2; 
        fu_pkt[22] = 0xAA; 
        
        fu_pkt[36+6] = 0x00; fu_pkt[36+7] = 0x01; 
        fu_pkt[36+11] = 10; 
        fu_pkt[36+15] = 0; 

        let t2 = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        
        let mut seq = Sequence::new();
        
        mock_net.expect_recv_packet()
            .times(1)
            .in_sequence(&mut seq)
            .returning(move || Ok(Some((sync_pkt.clone(), 36, t2))));
            
        mock_net.expect_recv_packet()
            .times(1)
            .in_sequence(&mut seq)
            .returning(move || Ok(Some((fu_pkt.clone(), 52, t2))));

        // Adjusted expectation: times(1) because settling_threshold=1
        mock_clock.expect_adjust_frequency().times(1).returning(|_| Ok(()));

        let mut controller = PtpController::new(mock_clock, mock_net, mock_ntp);
        
        assert!(controller.process_loop_iteration().is_ok());
        assert!(controller.process_loop_iteration().is_ok());
        
        assert_eq!(controller.valid_count, 1);
    }
}
