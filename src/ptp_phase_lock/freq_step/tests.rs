use super::*;

const DT: f64 = 0.5;

/// `n` points 0.5 s apart on `p = slope·t + f(t)`.
fn ring(n: usize, slope: f64, f: impl Fn(f64) -> f64) -> Vec<(f64, f64)> {
    (0..n)
        .map(|i| {
            let t = i as f64 * DT;
            (t, slope * t + f(t))
        })
        .collect()
}

#[test]
fn a_clean_line_fits_exactly_and_is_linear() {
    let fit = fit_ring(&ring(41, -25.0, |_| 0.0)).expect("enough points");
    assert!((fit.slope_ppm + 25.0).abs() < 1e-9, "{fit:?}");
    assert!(
        fit.linearity_f < 1e-6,
        "a line has nothing to explain: {fit:?}"
    );
    assert_eq!(fit.points, 41);
    assert!((fit.span_s - 20.0).abs() < 1e-12);
    // The residual floor (1 µs) sets the standard error of a noiseless line.
    let floor_sigma = 1.0 / (41.0 * 20.0 * 20.0 / 12.0f64 * (42.0 / 40.0)).sqrt();
    assert!(
        (fit.sigma_ppm - floor_sigma).abs() < 0.2 * floor_sigma,
        "{fit:?} vs {floor_sigma}"
    );
}

#[test]
fn a_fit_needs_enough_points_that_spread_in_time() {
    assert!(fit_ring(&ring(FSTEP_MIN_POINTS - 1, 1.0, |_| 0.0)).is_none());
    assert!(fit_ring(&ring(FSTEP_MIN_POINTS, 1.0, |_| 0.0)).is_some());
    let same_t: Vec<(f64, f64)> = (0..20).map(|i| (3.0, i as f64)).collect();
    assert!(fit_ring(&same_t).is_none());
}

#[test]
fn a_level_shift_reads_as_a_clean_slope_but_fails_the_linearity_test() {
    // A 100 µs median jump in the middle of the ring (a path delay change), on a flat phase.
    let fit = fit_ring(&ring(41, 0.0, |t| if t >= 10.0 { 100.0 } else { 0.0 })).unwrap();
    // What a plain `|s| > 6σ` test would see: 1.5·A / 20 s ≈ 7.5 ppm, many standard errors.
    assert!(fit.slope_ppm > 5.0, "{fit:?}");
    assert!(fit.slope_ppm > FSTEP_SIGMAS * fit.sigma_ppm, "{fit:?}");
    // The linearity test sees the jump …
    assert!(fit.linearity_f > 100.0 * FSTEP_LINEARITY_F_MAX, "{fit:?}");
    // … and with the jump explained nothing of the slope is left.
    assert!(fit.shifted_slope_ppm.abs() < 1e-6, "{fit:?}");
}

#[test]
fn the_shifted_slope_keeps_a_real_slope_and_removes_only_the_shift() {
    // A genuine 25 ppm line: explaining a level shift away changes nothing.
    let fit = fit_ring(&ring(41, 25.0, |_| 0.0)).unwrap();
    assert!((fit.shifted_slope_ppm - 25.0).abs() < 1e-6, "{fit:?}");
    // A 2 ppm line with a 150 µs jump 6 s from the end: the plain slope reads ~9 ppm, the shifted
    // one the true 2 ppm.
    let fit = fit_ring(&ring(41, 2.0, |t| if t >= 14.0 { 150.0 } else { 0.0 })).unwrap();
    assert!(fit.slope_ppm > FSTEP_MIN_PPM, "{fit:?}");
    assert!((fit.shifted_slope_ppm - 2.0).abs() < 1e-6, "{fit:?}");
}

#[test]
fn the_cheap_line_fit_agrees_with_the_full_ring_fit() {
    let pts = ring(41, -7.5, |t| 30.0 * (1.3 * t).sin());
    let line = fit_line(pts.iter()).unwrap();
    let full = fit_ring(&pts).unwrap();
    assert_eq!(line.slope_ppm, full.slope_ppm);
    assert_eq!(line.sigma_ppm, full.sigma_ppm);
    assert_eq!((line.points, line.span_s), (41, 20.0));
    // The split test refuses a line that is not the fit of these points.
    let short = &pts[..30];
    assert_eq!(split_test(short, &line).0, f64::INFINITY);
}

#[test]
fn a_slope_change_inside_the_ring_fails_the_linearity_test() {
    // Flat, then 25 ppm for the last 8 s: the ring still holds the pre-step phase.
    let fit = fit_ring(&ring(41, 0.0, |t| 25.0 * (t - 12.0).max(0.0))).unwrap();
    assert!(fit.slope_ppm > 5.0, "{fit:?}");
    assert!(fit.linearity_f > FSTEP_LINEARITY_F_MAX, "{fit:?}");
}

#[test]
fn a_single_outlier_is_never_a_clean_slope() {
    for at in [1usize, 10, 20, 30, 39] {
        let pts: Vec<(f64, f64)> = ring(41, 0.0, |_| 0.0)
            .into_iter()
            .enumerate()
            .map(|(i, (t, p))| (t, if i == at { p + 500.0 } else { p }))
            .collect();
        let fit = fit_ring(&pts).unwrap();
        assert!(
            fit.slope_ppm.abs() < 3.0_f64.sqrt() * fit.sigma_ppm + 1e-9,
            "an outlier at {at}: {fit:?}"
        );
    }
}

/// Drive the detector on an open-loop plant: `e` integrates `osc + word` (ppm) and the word is
/// whatever `word` returns (the loop's command). Returns every confirmation (window, estimate).
fn drive(
    det: &mut FreqStepDetector,
    windows: usize,
    osc: impl Fn(usize) -> f64,
    word: impl Fn(usize) -> f64,
    integrator: f64,
) -> Vec<(usize, FreqStepEstimate)> {
    let mut e = 0.0;
    let mut out = Vec::new();
    for w in 0..windows {
        if let Some(est) = det.observe(e, DT, integrator) {
            out.push((w, est));
        }
        let u = word(w);
        det.note_word(u);
        e += (osc(w) + u) * DT;
    }
    out
}

#[test]
fn an_unlearned_frequency_is_confirmed_once_the_ring_is_full_and_the_run_holds() {
    let mut det = FreqStepDetector::new();
    // The oscillator runs 25 ppm fast and the loop commands nothing (integrator 0).
    let got = drive(&mut det, 200, |_| 25.0, |_| 0.0, 0.0);
    // Window 40 is the first with a 20 s ring; the run needs FSTEP_CONFIRM windows.
    assert_eq!(got[0].0, 40 + FSTEP_CONFIRM as usize - 1, "{got:?}");
    assert!((got[0].1.error_ppm - 25.0).abs() < 1e-6, "{got:?}");
    assert!(got[0].1.span_s >= FSTEP_WINDOW_S - 1e-9);
    // Nothing more inside the holdoff (the ring refills in 20 s, the holdoff is 30 s).
    let holdoff_windows = (FSTEP_HOLDOFF_S / DT) as usize;
    assert!(
        got.iter()
            .skip(1)
            .all(|(w, _)| *w >= got[0].0 + holdoff_windows),
        "{got:?}"
    );
    // An integrator that already holds the frequency leaves nothing to follow.
    let mut det = FreqStepDetector::new();
    assert!(drive(&mut det, 400, |_| 25.0, |_| 0.0, -25.0).is_empty());
}

#[test]
fn the_loops_own_correction_is_not_a_step() {
    // Re-engaging with a 1 ms error: the proportional term slews 20 ppm, so `e` ramps at 20 ppm
    // while the integrator holds the oscillator exactly. The open-loop phase is flat.
    let mut det = FreqStepDetector::new();
    let got = drive(&mut det, 400, |_| -12.0, |_| 12.0 + 20.0, 12.0);
    assert!(got.is_empty(), "{got:?}");
}

#[test]
fn a_sign_change_restarts_the_confirmation_run() {
    let mut det = FreqStepDetector::new();
    // Fill the ring on a clean +25 ppm line, then flip the integrator between windows so the
    // unlearned error alternates +25 / −25: never FSTEP_CONFIRM of one sign in a row.
    let mut e = 0.0;
    let mut confirmed = false;
    for w in 0..200 {
        let integ = if w >= 40 && w % 2 == 0 { -50.0 } else { 0.0 };
        confirmed |= det.observe(e, DT, integ).is_some();
        det.note_word(0.0);
        e += 25.0 * DT;
    }
    assert!(!confirmed);
}

#[test]
fn a_gap_or_a_reset_restarts_the_ring() {
    let mut det = FreqStepDetector::new();
    drive(&mut det, 30, |_| 0.0, |_| 0.0, 0.0);
    assert_eq!(det.points(), 30);
    // A gap longer than DT_MAX_S: the ring restarts on this window.
    assert!(det.observe(0.0, 6.0, 0.0).is_none());
    assert_eq!(det.points(), 1);
    det.note_word(0.0);
    assert!(det.observe(0.0, f64::NAN, 0.0).is_none());
    assert_eq!(det.points(), 1);
    det.note_word(0.0);
    det.observe(0.0, DT, 0.0);
    assert_eq!(det.points(), 2);
    det.reset();
    assert_eq!(det.points(), 0);
}

#[test]
fn the_holdoff_runs_on_the_windows_dt() {
    let mut det = FreqStepDetector::new();
    let got = drive(&mut det, 60, |_| 25.0, |_| 0.0, 0.0);
    assert_eq!(got.len(), 1);
    assert!((det.holdoff_s() - (FSTEP_HOLDOFF_S - (60 - 1 - got[0].0) as f64 * DT)).abs() < 1e-9);
}
