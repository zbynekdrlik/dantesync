//! camera-box issue 1372 — the controller wiring of the phase lock's grandmaster FREQUENCY-STEP
//! follow: the word follows the step within seconds, `/status` publishes the event (count, size,
//! time), and the fleet `D` and its seq stay where they were. The clock is never stepped (the mock
//! has no `step_clock` expectation, so any step panics).

use super::tests::{phase_lock_config, PL_GM, PL_PTP_NOW_NS};
use super::*;
use crate::clock::MockSystemClock;
use crate::traits::{MockNtpSource, MockPtpNetwork};

#[test]
fn a_grandmaster_frequency_step_is_followed_published_and_never_moves_d_1372() {
    let words = Arc::new(std::sync::Mutex::new(Vec::<f64>::new()));
    let cap = words.clone();
    let mut clock = MockSystemClock::new();
    clock.expect_adjust_frequency().returning(move |factor| {
        cap.lock().expect("cap").push((factor - 1.0) * 1e6);
        Ok(())
    });
    let mut c = PtpController::new(
        clock,
        MockPtpNetwork::new(),
        MockNtpSource::new(),
        Arc::new(RwLock::new(SyncStatus::default())),
        phase_lock_config(),
    );
    // The NTP master (the date-offset authority), locked, the word handed over at 0 ppm.
    c.configure_ntp_server_mode(100_000);
    c.current_gm_uuid = Some(PL_GM);
    c.is_locked = true;
    let d = wall_now_ns() - PL_PTP_NOW_NS;

    // A box whose oscillator the word already holds (0 ppm); after 30 s the grandmaster speeds up
    // by 25 ppm: `e = t2 − t1 − D` then falls at 25 µs/s until the word follows.
    let step_at = 60;
    let mut e_ns = 0.0f64;
    let mut before = None;
    let mut followed_at = None;
    for w in 0..(step_at + 300) {
        c.date_sync.pending_median_ns = Some(d + e_ns.round() as i64);
        c.date_sync.pending_t1_ns = PL_PTP_NOW_NS + w * 500_000_000;
        c.apply_self_tuning_servo(0.0);
        if w == step_at - 1 {
            let st = c.get_status_shared();
            let st = st.read().expect("status");
            assert_eq!(st.freq_steps, 0, "nothing to follow before the step");
            assert_eq!(st.last_freq_step_ppm, None);
            before = Some((st.date_offset_ns, st.date_offset_seq));
        }
        if followed_at.is_none() && c.date_sync.core.freq_steps() == 1 {
            followed_at = Some(w);
        }
        let word = *words
            .lock()
            .expect("cap")
            .last()
            .expect("a word per window");
        let gm_ppm = if w >= step_at { 25.0 } else { 0.0 };
        e_ns += (word - gm_ppm) * 0.5 * 1_000.0;
    }
    let followed_at = followed_at.expect("the step is followed");
    assert!(
        (followed_at - step_at + 1) as f64 * 0.5 <= 30.0,
        "followed {} windows after the step",
        followed_at - step_at + 1
    );
    let last_word = *words.lock().expect("cap").last().expect("a word");
    assert!(
        (last_word - 25.0).abs() < 1.0,
        "the word follows the grandmaster: {last_word}"
    );

    let st = c.get_status_shared();
    let st = st.read().expect("status");
    assert_eq!(st.freq_steps, 1);
    let step = st.last_freq_step_ppm.expect("published");
    assert!((step - 25.0).abs() < 1.0, "the step followed: {step}");
    let ts = st.last_freq_step_ts.expect("published") as i64;
    assert!(
        (ts - wall_now_ns() / 1_000_000_000).abs() <= 5,
        "stamped now: {ts}"
    );
    // D and its seq: exactly as before the step.
    assert_eq!(Some((st.date_offset_ns, st.date_offset_seq)), before);
    assert_eq!(st.date_offset_ns, Some(d));
    assert_eq!(c.date_sync.core.anchor_ns(), Some(d));
    assert!(st.ptp_phase_locked);
}
