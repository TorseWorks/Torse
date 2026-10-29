//! Conservative clip-space bounds and logical resident geometry admission.
use torse_types::SectionPos;

#[derive(Clone, Copy, Debug)]
pub(super) struct Bounds {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

impl Bounds {
    pub fn vertices(bytes: &[u8], stride: usize) -> Option<Self> {
        if bytes.is_empty() || !bytes.len().is_multiple_of(stride) {
            return None;
        }
        let mut bounds = Self {
            min: [f32::INFINITY; 3],
            max: [f32::NEG_INFINITY; 3],
        };
        for vertex in bytes.chunks_exact(stride) {
            for axis in 0..3 {
                let value = f32::from_le_bytes(vertex[axis * 4..axis * 4 + 4].try_into().ok()?);
                if !value.is_finite() {
                    return None;
                }
                bounds.min[axis] = bounds.min[axis].min(value);
                bounds.max[axis] = bounds.max[axis].max(value);
            }
        }
        Some(bounds)
    }

    pub fn section(pos: SectionPos, side: u32) -> Self {
        let min = [pos.x, pos.y, pos.z].map(|p| (p as f64 * f64::from(side)) as f32 - 1.0);
        let max = [pos.x, pos.y, pos.z].map(|p| ((p as f64 + 1.0) * f64::from(side)) as f32 + 1.0);
        Self { min, max }
    }

    pub fn visible(self, matrix: &[f32; 16], eye: [f32; 3]) -> bool {
        if (0..3).all(|a| eye[a] >= self.min[a] && eye[a] <= self.max[a]) {
            return true;
        }
        let mut outside = [true; 6];
        for mask in 0..8 {
            let p: [f32; 3] = std::array::from_fn(|a| {
                if mask & (1 << a) == 0 {
                    self.min[a]
                } else {
                    self.max[a]
                }
            });
            let clip: [f64; 4] = std::array::from_fn(|a| {
                f64::from(matrix[a]) * f64::from(p[0])
                    + f64::from(matrix[4 + a]) * f64::from(p[1])
                    + f64::from(matrix[8 + a]) * f64::from(p[2])
                    + f64::from(matrix[12 + a])
            });
            if clip.iter().any(|v| !v.is_finite()) {
                return true;
            }
            let [x, y, z, w] = clip;
            let epsilon = 1e-5 * (x.abs() + y.abs() + z.abs() + w.abs() + 1.0);
            for (plane, value) in outside
                .iter_mut()
                .zip([x + w, w - x, y + w, w - y, z, w - z])
            {
                *plane &= value < -epsilon;
            }
        }
        !outside.into_iter().any(|all| all)
    }
}

pub(super) fn priority(pos: SectionPos, focus: SectionPos) -> (u128, SectionPos) {
    (
        u128::from(pos.x.abs_diff(focus.x))
            + u128::from(pos.y.abs_diff(focus.y))
            + u128::from(pos.z.abs_diff(focus.z)),
        pos,
    )
}

pub(super) fn admission(
    limit: u64,
    used: u64,
    old: u64,
    incoming: u64,
    need_entry: bool,
    candidates: impl Iterator<Item = (SectionPos, u64, (u128, SectionPos))>,
) -> Option<Vec<SectionPos>> {
    if incoming > limit {
        return None;
    }
    let mut projected = used.checked_sub(old)?.checked_add(incoming)?;
    if projected <= limit && !need_entry {
        return Some(Vec::new());
    }
    let mut candidates: Vec<_> = candidates.collect();
    candidates.sort_unstable_by(|a, b| b.2.cmp(&a.2));
    let mut victims = Vec::new();
    for (pos, bytes, _) in candidates {
        projected = projected.checked_sub(bytes)?;
        victims.push(pos);
        if projected <= limit {
            return Some(victims);
        }
    }
    None
}

pub(super) fn can_admit(
    limit: u64,
    used: u64,
    old: u64,
    incoming: u64,
    need_entry: bool,
    candidates: impl Iterator<Item = u64>,
) -> bool {
    if incoming > limit {
        return false;
    }
    let Some(mut projected) = used
        .checked_sub(old)
        .and_then(|used| used.checked_add(incoming))
    else {
        return false;
    };
    if projected <= limit && !need_entry {
        return true;
    }
    for bytes in candidates {
        let Some(next) = projected.checked_sub(bytes) else {
            return false;
        };
        projected = next;
        if projected <= limit {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    const IDENTITY: [f32; 16] = [
        1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1.,
    ];
    #[test]
    fn allocation_free_probe_matches_the_atomic_victim_plan() {
        for length in 0..=3 {
            let residents = &[
                (SectionPos::ZERO, 1, (3, SectionPos::ZERO)),
                (SectionPos::new(1, 0, 0), 3, (2, SectionPos::ZERO)),
                (SectionPos::new(2, 0, 0), 5, (1, SectionPos::ZERO)),
            ][..length];
            for limit in 0..18 {
                for old in [0, 4] {
                    for incoming in 0..18 {
                        for need in [false, true] {
                            let used = residents.iter().map(|r| r.1).sum::<u64>() + old;
                            assert_eq!(
                                can_admit(
                                    limit,
                                    used,
                                    old,
                                    incoming,
                                    need,
                                    residents.iter().map(|r| r.1)
                                ),
                                admission(
                                    limit,
                                    used,
                                    old,
                                    incoming,
                                    need,
                                    residents.iter().copied()
                                )
                                .is_some()
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn clip_bounds_keep_crossings_and_reject_all_six_outside_planes() {
        let eye = [10.; 3];
        let inside = Bounds {
            min: [-0.5, -0.5, 0.2],
            max: [0.5, 0.5, 0.8],
        };
        assert!(inside.visible(&IDENTITY, eye));
        for (axis, min, max) in [
            (0, -3., -2.),
            (0, 2., 3.),
            (1, -3., -2.),
            (1, 2., 3.),
            (2, -2., -1.),
            (2, 2., 3.),
        ] {
            let mut bounds = inside;
            bounds.min[axis] = min;
            bounds.max[axis] = max;
            assert!(!bounds.visible(&IDENTITY, eye));
        }
        assert!(
            Bounds {
                min: [-2.; 3],
                max: [2.; 3]
            }
            .visible(&IDENTITY, eye)
        );
        assert!(
            Bounds {
                min: [-0.1, -0.1, -0.2],
                max: [0.1, 0.1, 0.2]
            }
            .visible(&IDENTITY, eye)
        );
        assert!(inside.visible(&[f32::NAN; 16], eye));
        assert!(inside.visible(&IDENTITY, [0., 0., 0.5]));
    }

    #[test]
    fn admission_is_atomic_bounded_and_evicts_farthest_first() {
        let a = SectionPos::new(1, 0, 0);
        let b = SectionPos::new(8, 0, 0);
        let entries = || {
            [
                (a, 20, priority(a, SectionPos::ZERO)),
                (b, 30, priority(b, SectionPos::ZERO)),
            ]
            .into_iter()
        };
        assert_eq!(admission(60, 50, 0, 40, false, entries()), Some(vec![b]));
        assert!(admission(60, 50, 0, 61, false, entries()).is_none());
        assert!(admission(60, 50, 0, 40, false, std::iter::empty()).is_none());
        assert_eq!(admission(60, 50, 20, 25, false, entries()), Some(vec![]));
        assert_eq!(admission(60, 50, 0, 1, true, entries()), Some(vec![b]));
    }
}
