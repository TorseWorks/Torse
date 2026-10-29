//! Presentation deadlines, independent of fixed simulation steps.
use std::time::{Duration, Instant};

pub const MIN_FPS: u16 = 30;
pub const MAX_FPS: u16 = 1000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FrameLimit {
    pub enabled: bool,
    pub fps: u16,
}
impl Default for FrameLimit {
    fn default() -> Self {
        Self {
            enabled: false,
            fps: 144,
        }
    }
}
impl FrameLimit {
    pub fn period(self) -> Option<Duration> {
        self.enabled
            .then(|| Duration::from_secs_f64(1.0 / f64::from(self.fps.clamp(MIN_FPS, MAX_FPS))))
    }
}

#[derive(Default)]
pub struct FramePacer {
    next: Option<Instant>,
}
impl FramePacer {
    pub fn reset(&mut self) {
        self.next = None;
    }
    pub fn deadline(&self, now: Instant) -> Instant {
        self.next.unwrap_or(now)
    }
    pub fn ready(&self, now: Instant) -> bool {
        now >= self.deadline(now)
    }
    pub fn started(&mut self, now: Instant, limit: FrameLimit) {
        self.next = limit.period().map(|period| now + period);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deadlines_never_catch_up_missed_frames_or_limit_uncapped_mode() {
        let now = Instant::now();
        let mut pacer = FramePacer::default();
        let limit = FrameLimit {
            enabled: true,
            fps: 100,
        };
        pacer.started(now, limit);
        assert!(!pacer.ready(now + Duration::from_millis(9)));
        assert!(pacer.ready(now + Duration::from_millis(10)));
        let late = now + Duration::from_secs(5);
        pacer.started(late, limit);
        assert_eq!(pacer.deadline(late), late + Duration::from_millis(10));
        pacer.reset();
        assert!(pacer.ready(now));
        pacer.started(now, FrameLimit::default());
        assert!(pacer.ready(now));
    }
}
