//! Damped springs for the island morph.

#[derive(Clone, Copy, Debug)]
pub struct Spring {
    pub x: f32,
    pub v: f32,
    pub target: f32,
    stiffness: f32,
    damping: f32,
    /// Absolute distance under which the spring counts as settled.
    eps: f32,
}

impl Spring {
    pub fn new(x: f32, eps: f32) -> Self {
        Self { x, v: 0.0, target: x, stiffness: 1.0, damping: 1.0, eps }
    }

    pub fn snap(&mut self, x: f32) {
        self.x = x;
        self.v = 0.0;
        self.target = x;
    }

    /// `ratio` 1.0 is critically damped, below 1.0 overshoots.
    pub fn retarget(&mut self, target: f32, omega: f32, ratio: f32) {
        self.target = target;
        self.stiffness = omega * omega;
        self.damping = 2.0 * ratio * omega;
    }

    pub fn settled(&self) -> bool {
        (self.x - self.target).abs() < self.eps && self.v.abs() < self.eps * 10.0
    }

    pub fn step(&mut self, dt: f32) {
        let mut left = dt;
        while left > 0.0 {
            let h = left.min(1.0 / 240.0);
            let a = -self.stiffness * (self.x - self.target) - self.damping * self.v;
            self.v += a * h;
            self.x += self.v * h;
            left -= h;
        }
        if self.settled() {
            self.x = self.target;
            self.v = 0.0;
        }
    }
}
