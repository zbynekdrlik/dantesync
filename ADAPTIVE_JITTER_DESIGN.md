# Adaptive Jitter Detection Design Analysis

## Problem Statement

stream.lan (Realtek NIC) has 10.33 µs/s drift stddev vs strih.lan (Marvell NIC) with 0.80 µs/s.
The high jitter causes ACQ↔PROD oscillation and prevents lock.

## Proposed Solution

Detect high-jitter systems automatically and apply heavier EMA smoothing.

## Current System Behavior by Target

| Target | NIC | Drift StdDev | Current Behavior | Expected After |
|--------|-----|--------------|------------------|----------------|
| strih.lan | Marvell 10G | 0.80 µs/s | Stable NANO | NO CHANGE |
| mbc.lan | Realtek USB | ~3-5 µs/s + spikes | LOCK (spikes filtered) | Slight improvement |
| stream.lan | Realtek PCIe | 10.33 µs/s | ACQ↔PROD bounce | Stable LOCK |

## Design Constraints

### 1. MUST NOT cause regression on stable systems (strih.lan)
- α must remain 0.3 when jitter < 2 µs/s
- No additional smoothing when not needed
- Fast acquisition speed must be preserved

### 2. MUST NOT interfere with spike filter
- Jitter detection uses VARIANCE (oscillation)
- Spike filter uses DEVIATION from MEDIAN (outliers)
- These are orthogonal measurements

### 3. MUST NOT slow down real frequency tracking
- A real drift STEP shows high MEAN, not high VARIANCE
- Sustained drift in one direction has low variance
- Only bidirectional oscillation triggers adaptation

### 4. MUST be gradual and reversible
- α changes smoothly, not in steps
- When jitter decreases, α increases back to 0.3
- No hysteresis needed (unlike mode transitions)

## Algorithm Design

```
JITTER_LOW = 2.0 µs/s   // Below: normal α=0.3
JITTER_HIGH = 8.0 µs/s  // Above: smoothed α=0.1
WINDOW_SIZE = 30        // ~30 seconds of samples

For each rate sample:
  1. Add to jitter window (separate from spike filter window)
  2. Calculate stddev of window
  3. Compute adaptive α:
     - jitter ≤ 2.0: α = 0.3 (no change)
     - jitter ≥ 8.0: α = 0.1 (max smoothing)
     - between: linear interpolation
  4. Apply EMA: smoothed = (1-α)*smoothed + α*new_rate
```

## Potential Failure Modes & Mitigations

### FM1: Over-smoothing stable system
- **Risk**: strih.lan gets α < 0.3 unnecessarily
- **Mitigation**: JITTER_LOW = 2.0 is well above strih.lan's 0.8 µs/s
- **Test**: Verify α stays 0.3 when stddev < 1.5 µs/s

### FM2: Under-smoothing noisy system
- **Risk**: stream.lan still oscillates
- **Mitigation**: JITTER_HIGH = 8.0 triggers full smoothing at stream.lan's 10.3 µs/s
- **Test**: Verify α drops to 0.1 when stddev > 10 µs/s

### FM3: Spike filter and jitter detection fighting
- **Risk**: Both reduce responsiveness simultaneously
- **Mitigation**: Spike filter replaces outliers with median (affects data quality)
             Jitter detection adjusts EMA (affects filtering strength)
             They work on different aspects and are complementary
- **Test**: Simulate spikes + high jitter, verify behavior

### FM4: Real frequency step misclassified as jitter
- **Risk**: System drift changes from +10 to +20 ppm, classified as jitter
- **Analysis**: A step causes high MEAN but low VARIANCE if sustained
              Jitter is oscillation: +10, -10, +10, -10 → high variance
              Step is: +10, +11, +12, +13 → low variance
- **Test**: Simulate drift step, verify α stays high

### FM5: Startup jitter triggers over-smoothing
- **Risk**: During warmup, rate varies widely, triggers smoothing
- **Mitigation**: Jitter estimator requires MIN_SAMPLES (15) before activating
              During warmup, α defaults to 0.3
- **Test**: Verify warmup behavior unchanged

### FM6: Performance impact
- **Risk**: StdDev calculation slows down main loop
- **Analysis**: 30 samples, O(n) calculation, ~0.5µs max
              Main loop runs once per second
              Negligible impact
- **No mitigation needed**

## Test Matrix

| Test Case | Input | Expected α | Expected Mode |
|-----------|-------|------------|---------------|
| Stable low jitter | stddev=0.5 | 0.3 | Normal |
| Moderate jitter | stddev=5.0 | 0.2 | Slightly smoothed |
| High jitter | stddev=10.0 | 0.1 | Heavily smoothed |
| Warmup period | <15 samples | 0.3 | Default |
| Jitter decreasing | 10→2 | 0.1→0.3 | Gradual recovery |
| Drift step | +10ppm sudden | 0.3 | Tracks step |
| Spikes + jitter | outliers + oscillation | 0.1-0.15 | Filtered + smoothed |

## Implementation Order

1. Add JitterEstimator struct with comprehensive tests
2. Add adaptive_alpha() function with tests
3. Integrate into controller with logging
4. Run all existing tests (must pass)
5. Run E2E simulation tests
6. Deploy to test targets
7. Verify no regression on strih.lan (NANO mode)
8. Verify improvement on stream.lan (stable LOCK)
9. Verify mbc.lan unchanged (LOCK with spike filtering)

## Success Criteria

- [ ] strih.lan remains in NANO mode with drift < 3 µs/s
- [ ] stream.lan achieves stable LOCK mode (no ACQ↔PROD bounce)
- [ ] mbc.lan behavior unchanged (LOCK with occasional spike rejections)
- [ ] All 71+ unit tests pass
- [ ] All 9 E2E simulation tests pass
- [ ] CI pipeline green
