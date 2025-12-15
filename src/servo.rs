use log::debug;

pub struct PiServo {
    kp: f64,
    ki: f64,
    integral: f64,
    max_integral: f64,
}

impl PiServo {
    pub fn new(kp: f64, ki: f64) -> Self {
        PiServo {
            kp,
            ki,
            integral: 0.0,
            // 200 PPM is a safe upper bound for standard crystal drift.
            // Allowing more just invites instability (windup).
            max_integral: 200.0, 
        }
    }

    pub fn reset(&mut self) {
        self.integral = 0.0;
    }

    /// Calculate frequency adjustment (in PPM) to correct the phase offset (in nanoseconds).
    /// `offset_ns`: Local - Master (positive if Local is ahead)
    pub fn sample(&mut self, offset_ns: i64) -> f64 {
        // We want to drive offset_ns to 0.
        // If offset_ns > 0 (ahead), we need to slow down (negative adj).
        // If offset_ns < 0 (behind), we need to speed up (positive adj).
        
        let error = -offset_ns as f64; 

        // Update Integral
        self.integral += error * self.ki;
        
        // Clamp integral
        if self.integral > self.max_integral { self.integral = self.max_integral; }
        if self.integral < -self.max_integral { self.integral = -self.max_integral; }

        // Proportional
        let proportional = error * self.kp;

        let adjustment_ppm = proportional + self.integral;
        
        debug!("Servo: Err={}ns, P={:.3}, I={:.3}, Adj={:.3}ppm", offset_ns, proportional, self.integral, adjustment_ppm);
        
        adjustment_ppm
    }
}