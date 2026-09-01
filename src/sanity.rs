//! Reading smoother: maintains a "current time" estimate from the last N
//! accepted readings, so a single late or slightly-off read doesn't wobble
//! the reported time.

use std::collections::VecDeque;

pub struct Smoother {
    window: usize,
    /// (wall_ms, timer_ms) pairs for accepted readings.
    buf: VecDeque<(i64, i64)>,
}

impl Smoother {
    pub fn new(window: usize) -> Self {
        Self {
            window: window.max(1),
            buf: VecDeque::new(),
        }
    }

    pub fn push(&mut self, wall_ms: i64, timer_ms: i64) {
        self.buf.push_back((wall_ms, timer_ms));
        while self.buf.len() > self.window {
            self.buf.pop_front();
        }
    }

    /// Median of each stored reading projected forward to `now_ms` (a running
    /// timer advances 1:1 with wall time, so projection is just addition).
    pub fn current(&self, now_ms: i64) -> Option<i64> {
        if self.buf.is_empty() {
            return None;
        }
        let mut proj: Vec<i64> = self.buf.iter().map(|&(t, v)| v + (now_ms - t)).collect();
        proj.sort_unstable();
        Some(proj[proj.len() / 2])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_gives_none() {
        assert_eq!(Smoother::new(5).current(1000), None);
    }

    #[test]
    fn projects_readings_forward() {
        let mut s = Smoother::new(5);
        s.push(0, 1000);
        s.push(1000, 2000);
        s.push(2000, 3000);
        // All three project to 4000 at t=3000.
        assert_eq!(s.current(3000), Some(4000));
    }

    #[test]
    fn median_absorbs_one_late_read() {
        let mut s = Smoother::new(5);
        s.push(0, 1000);
        s.push(1000, 2000);
        s.push(2000, 2600); // read arrived late: projects 400ms low
        assert_eq!(s.current(3000), Some(4000));
    }

    #[test]
    fn window_evicts_oldest() {
        let mut s = Smoother::new(2);
        s.push(0, 100_000); // stale, should be evicted
        s.push(1000, 1000);
        s.push(2000, 2000);
        assert_eq!(s.current(2000), Some(2000));
    }
}
