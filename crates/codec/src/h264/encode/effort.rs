//! Wall-clock feedback controlling compression effort, never frame count.
use std::collections::VecDeque;

pub(super) struct Governor {
    pub level: u8,
    budget_ms: f64,
    samples: VecDeque<f64>,
    overloaded: u8,
    headroom: u8,
    age: u8,
    pub p95: f64,
}
impl Governor {
    pub fn new(fps: u32, percent: u8) -> Self {
        Self {
            level: 0,
            budget_ms: 10. * percent as f64 / fps as f64,
            samples: VecDeque::with_capacity(32),
            overloaded: 0,
            headroom: 0,
            age: 0,
            p95: 0.,
        }
    }
    pub fn observe(&mut self, ms: f64) {
        if self.budget_ms == 0. {
            return;
        }
        self.samples.push_back(ms);
        if self.samples.len() > 32 {
            self.samples.pop_front();
        }
        let mut sorted: Vec<_> = self.samples.iter().copied().collect();
        sorted.sort_unstable_by(f64::total_cmp);
        self.p95 = sorted[(sorted.len() * 95).div_ceil(100) - 1];
        self.age = self.age.saturating_add(1);
        self.overloaded = if ms > self.budget_ms * 1.5 {
            self.overloaded + 1
        } else {
            0
        };
        self.headroom = if self.p95 < self.budget_ms * 0.60 {
            self.headroom.saturating_add(1)
        } else {
            0
        };
        let old = self.level;
        if self.overloaded >= 2 {
            self.level = (self.level + 2).min(4);
        } else if self.age >= 8
            && self.p95 > self.budget_ms
            && self.samples.iter().filter(|&&v| v > self.budget_ms).count() >= 2
        {
            self.level = (self.level + 1).min(4);
        } else if self.headroom >= 64 {
            self.level = self.level.saturating_sub(1);
        }
        self.overloaded = self.overloaded.min(2);
        if old != self.level {
            // Old-level costs must not trigger another demotion or prevent recovery.
            self.samples.clear();
            self.age = 0;
            self.overloaded = 0;
            self.headroom = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overload_recovery_and_disabled_are_deterministic() {
        let mut g = Governor::new(60, 65);
        g.observe(100.); // one scheduling outlier cannot demote immediately
        assert_eq!(g.level, 0);
        g.observe(20.);
        assert_eq!(g.level, 2);
        for _ in 0..8 {
            g.observe(12.);
        }
        assert_eq!(g.level, 3);
        for _ in 0..8 {
            g.observe(12.);
        }
        assert_eq!(g.level, 4);
        for _ in 0..96 {
            g.observe(2.);
        }
        assert_eq!(g.level, 3);
        for _ in 0..256 {
            g.observe(2.);
        }
        assert_eq!(g.level, 0);
        let mut spike = Governor::new(60, 65);
        spike.observe(100.);
        for _ in 0..40 {
            spike.observe(2.);
        }
        assert_eq!(spike.level, 0);
        let mut off = Governor::new(60, 0);
        for _ in 0..100 {
            off.observe(1000.);
        }
        assert_eq!(off.level, 0);
    }
}
