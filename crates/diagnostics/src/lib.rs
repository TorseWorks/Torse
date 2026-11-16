//! Opt-in, bounded wall-time samples for one serial execution stream.
//!
//! Nested spans share a recorder and charge their exclusive time to their own
//! stage. Spans must be properly nested on one serial stream; future parallel
//! workers need separate recorders. No measurement participates in authority decisions.

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum Stage {
    Planning,
    Preparation,
    Encoding,
    Publication,
    Projection,
    Flush,
    JournalAppend,
    JournalSync,
}

impl Stage {
    pub const ALL: [Self; 8] = [
        Self::Planning,
        Self::Preparation,
        Self::Encoding,
        Self::Publication,
        Self::Projection,
        Self::Flush,
        Self::JournalAppend,
        Self::JournalSync,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Planning => "planning",
            Self::Preparation => "preparation",
            Self::Encoding => "encoding",
            Self::Publication => "publication",
            Self::Projection => "projection",
            Self::Flush => "flush",
            Self::JournalAppend => "journal_append",
            Self::JournalSync => "journal_sync",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StageTotal {
    pub calls: u64,
    pub total_ns: u128,
    pub max_ns: u64,
    /// Encoded envelope bytes when this stage reports an output size.
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Sample {
    pub stage: Stage,
    pub elapsed_ns: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug)]
pub struct Profile {
    pub totals: [StageTotal; Stage::ALL.len()],
    /// Completion order; nested children precede their parent.
    pub samples: Vec<Sample>,
    /// Totals still include samples beyond the fixed retention limit.
    pub discarded_samples: u64,
}

struct State {
    profile: Profile,
    capacity: usize,
    accounted_ns: u128,
    active: usize,
}

/// Disabled by default: no clock reads, locks, allocations, or retained samples.
#[derive(Clone, Default)]
pub struct SerialProfiler(Option<Arc<Mutex<State>>>);

impl SerialProfiler {
    /// Reserve the complete sample capacity before enabling measurement.
    pub fn new(max_samples: usize) -> Self {
        Self(Some(Arc::new(Mutex::new(State {
            profile: Profile {
                totals: [StageTotal::default(); Stage::ALL.len()],
                samples: Vec::with_capacity(max_samples),
                discarded_samples: 0,
            },
            capacity: max_samples,
            accounted_ns: 0,
            active: 0,
        }))))
    }

    pub fn span(&self, stage: Stage) -> Span {
        let timing = self.0.as_ref().map(|state| {
            let mut data = state.lock().expect("serial profiler lock");
            data.active += 1;
            let before = data.accounted_ns;
            drop(data);
            (Arc::clone(state), before, Instant::now())
        });
        Span {
            stage,
            timing,
            bytes: 0,
        }
    }

    /// Snapshot only between spans, outside the measured execution path.
    pub fn profile(&self) -> Profile {
        let Some(state) = &self.0 else {
            return Profile {
                totals: [StageTotal::default(); Stage::ALL.len()],
                samples: Vec::new(),
                discarded_samples: 0,
            };
        };
        let data = state.lock().expect("serial profiler lock");
        assert_eq!(data.active, 0, "profile requires a completed serial span");
        data.profile.clone()
    }

    pub fn clear(&self) {
        if let Some(state) = &self.0 {
            let mut data = state.lock().expect("serial profiler lock");
            assert_eq!(data.active, 0, "clear requires completed serial spans");
            data.profile.totals = [StageTotal::default(); Stage::ALL.len()];
            data.profile.samples.clear();
            data.profile.discarded_samples = 0;
            data.accounted_ns = 0;
        }
    }

    pub fn totals(&self) -> [StageTotal; Stage::ALL.len()] {
        self.0
            .as_ref()
            .map(|state| state.lock().expect("serial profiler lock").profile.totals)
            .unwrap_or_default()
    }
}

#[must_use = "keep the span alive until the measured operation finishes"]
pub struct Span {
    stage: Stage,
    timing: Option<(Arc<Mutex<State>>, u128, Instant)>,
    bytes: u64,
}

impl Span {
    pub fn set_bytes(&mut self, bytes: u64) {
        self.bytes = bytes;
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        if let Some((state, before, start)) = &self.timing {
            let elapsed = start.elapsed().as_nanos();
            let mut data = state.lock().expect("serial profiler lock");
            let children = data.accounted_ns.saturating_sub(*before);
            let exclusive = elapsed.saturating_sub(children).min(u128::from(u64::MAX)) as u64;
            data.accounted_ns += u128::from(exclusive);
            data.active -= 1;
            let total = &mut data.profile.totals[self.stage as usize];
            total.calls += 1;
            total.total_ns += u128::from(exclusive);
            total.max_ns = total.max_ns.max(exclusive);
            total.bytes += self.bytes;
            if data.profile.samples.len() < data.capacity {
                data.profile.samples.push(Sample {
                    stage: self.stage,
                    elapsed_ns: exclusive,
                    bytes: self.bytes,
                });
            } else {
                data.profile.discarded_samples += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_time_is_charged_once_and_retention_is_bounded() {
        let profiler = SerialProfiler::new(1);
        let elapsed = Instant::now();
        {
            let _parent = profiler.span(Stage::Preparation);
            let mut child = profiler.span(Stage::Planning);
            child.set_bytes(23);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let profile = profiler.profile();
        assert_eq!(profile.samples.len(), 1);
        assert_eq!(profile.samples[0].stage, Stage::Planning);
        assert_eq!(profile.discarded_samples, 1);
        assert_eq!(profile.totals[Stage::Preparation as usize].calls, 1);
        assert_eq!(profile.totals[Stage::Planning as usize].calls, 1);
        assert_eq!(profile.totals[Stage::Planning as usize].bytes, 23);
        assert!(
            profile.totals.iter().map(|t| t.total_ns).sum::<u128>() <= elapsed.elapsed().as_nanos()
        );
        profiler.clear();
        assert!(profiler.profile().samples.is_empty());
        assert_eq!(
            profiler.profile().totals,
            [StageTotal::default(); Stage::ALL.len()]
        );
    }

    #[test]
    fn disabled_profiler_and_early_errors_are_observational() {
        fn operation(profiler: &SerialProfiler) -> Result<(), ()> {
            let _span = profiler.span(Stage::Encoding);
            Err(())
        }
        let disabled = SerialProfiler::default();
        assert!(operation(&disabled).is_err());
        assert!(disabled.profile().samples.is_empty());
        let enabled = SerialProfiler::new(4);
        assert!(operation(&enabled).is_err());
        assert_eq!(enabled.profile().totals[Stage::Encoding as usize].calls, 1);
    }
}
