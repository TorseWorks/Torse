use torse_sim::replica::ReplicaScope;
use torse_types::{Fault, SectionPos};

use super::MAX_VISUAL_SECTIONS;

const MAX_EXPLICIT_APPROVAL_SECTIONS: usize = 16_384;

/// Host-approved display coordinates. A rectangular approval takes constant
/// space regardless of the number of covered sections.
#[derive(Clone, Debug)]
pub struct VisualTerrainApproval(Approval);

#[derive(Clone, Debug)]
enum Approval {
    Rectangle(SectionBox),
    Explicit(ReplicaScope),
}

#[derive(Clone, Copy, Debug)]
struct SectionBox {
    min: SectionPos,
    max: SectionPos,
}

impl SectionBox {
    fn new(min: SectionPos, max: SectionPos) -> Result<Self, Fault> {
        if min.x > max.x || min.y > max.y || min.z > max.z {
            return Err(Fault::invalid_data("inverted visual terrain range"));
        }
        Ok(Self { min, max })
    }

    fn contains(self, pos: SectionPos) -> bool {
        (self.min.x..=self.max.x).contains(&pos.x)
            && (self.min.y..=self.max.y).contains(&pos.y)
            && (self.min.z..=self.max.z).contains(&pos.z)
    }

    fn volume(self) -> Option<u128> {
        let x = u128::from(self.min.x.abs_diff(self.max.x)) + 1;
        let y = u128::from(self.min.y.abs_diff(self.max.y)) + 1;
        let z = u128::from(self.min.z.abs_diff(self.max.z)) + 1;
        x.checked_mul(y)?.checked_mul(z)
    }

    fn intersection(self, other: Self) -> Option<Self> {
        Self::new(
            component_min_max(self.min, other.min, i64::max),
            component_min_max(self.max, other.max, i64::min),
        )
        .ok()
    }
}

fn component_min_max(a: SectionPos, b: SectionPos, op: fn(i64, i64) -> i64) -> SectionPos {
    SectionPos::new(op(a.x, b.x), op(a.y, b.y), op(a.z, b.z))
}

impl VisualTerrainApproval {
    /// Create an approval using inclusive section coordinates on all three axes.
    pub fn rectangular(min: SectionPos, max: SectionPos) -> Result<Self, Fault> {
        Ok(Self(Approval::Rectangle(SectionBox::new(min, max)?)))
    }

    /// Test display approval for a coordinate without loading terrain.
    pub fn contains(&self, pos: SectionPos) -> bool {
        match &self.0 {
            Approval::Rectangle(area) => area.contains(pos),
            Approval::Explicit(scope) => scope.sections.contains(&pos),
        }
    }

    /// Select the approved intersection of an inclusive window. Pass the page
    /// budget of the desired window. An oversized result is rejected as a whole.
    pub fn select_window(
        &self,
        min: SectionPos,
        max: SectionPos,
        max_sections: usize,
    ) -> Result<ReplicaScope, Fault> {
        let window = SectionBox::new(min, max)?;
        if max_sections > MAX_VISUAL_SECTIONS {
            return Err(Fault::resource_limit("visual window page limit"));
        }
        let mut scope = ReplicaScope {
            sections: Default::default(),
        };
        match &self.0 {
            Approval::Rectangle(area) => {
                let Some(area) = area.intersection(window) else {
                    return Ok(scope);
                };
                if area.volume().is_none_or(|n| n > max_sections as u128) {
                    return Err(Fault::resource_limit("visual window page budget"));
                }
                for x in area.min.x..=area.max.x {
                    for y in area.min.y..=area.max.y {
                        for z in area.min.z..=area.max.z {
                            scope.sections.insert(SectionPos::new(x, y, z));
                        }
                    }
                }
            }
            Approval::Explicit(approved) => {
                self.reservation_bytes()?;
                for pos in approved
                    .sections
                    .iter()
                    .copied()
                    .filter(|p| window.contains(*p))
                {
                    if scope.sections.len() == max_sections {
                        return Err(Fault::resource_limit("visual window page budget"));
                    }
                    scope.sections.insert(pos);
                }
            }
        }
        Ok(scope)
    }

    pub(super) fn check_desired(
        &self,
        desired: &ReplicaScope,
        max_sections: usize,
    ) -> Result<(), Fault> {
        if desired.sections.len() > max_sections {
            return Err(Fault::resource_limit("visual desired page budget"));
        }
        if desired.sections.iter().any(|&pos| !self.contains(pos)) {
            return Err(Fault::invalid_data("visual scope outside approval"));
        }
        Ok(())
    }

    pub(super) fn reservation_bytes(&self) -> Result<u64, Fault> {
        let explicit_sections = match &self.0 {
            Approval::Rectangle(_) => 0,
            Approval::Explicit(scope) => scope.sections.len(),
        };
        if explicit_sections > MAX_EXPLICIT_APPROVAL_SECTIONS {
            return Err(Fault::resource_limit("explicit visual approval metadata"));
        }
        Ok(size_of::<Self>() as u64 + explicit_sections as u64 * 128)
    }
}

impl From<ReplicaScope> for VisualTerrainApproval {
    /// Compress a complete rectangle exactly. A scope with any holes stays
    /// explicit, preserving the approved coordinates.
    fn from(scope: ReplicaScope) -> Self {
        if let Some(&first) = scope.sections.first() {
            let mut area = SectionBox {
                min: first,
                max: first,
            };
            for &pos in &scope.sections {
                area.min = component_min_max(area.min, pos, i64::min);
                area.max = component_min_max(area.max, pos, i64::max);
            }
            if area.volume() == Some(scope.sections.len() as u128) {
                return Self(Approval::Rectangle(area));
            }
        }
        Self(Approval::Explicit(scope))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use torse_types::FaultCode;

    #[test]
    fn window_queries_intersect_before_counting_and_reject_overflow() {
        let min = SectionPos::new(i64::MIN, i64::MIN, i64::MIN);
        let max = SectionPos::new(i64::MAX, i64::MAX, i64::MAX);
        assert!(VisualTerrainApproval::rectangular(max, min).is_err());
        let approval = VisualTerrainApproval::rectangular(min, max).unwrap();
        let singleton = VisualTerrainApproval::rectangular(min, min).unwrap();
        assert_eq!(
            approval.reservation_bytes().unwrap(),
            singleton.reservation_bytes().unwrap()
        );
        assert_eq!(
            approval.select_window(min, max, 8).unwrap_err().code,
            FaultCode::ResourceLimit
        );
        assert_eq!(
            approval.select_window(max, max, 1).unwrap().sections,
            [max].into()
        );

        let approval = VisualTerrainApproval::rectangular(SectionPos::ZERO, max).unwrap();
        assert_eq!(
            approval
                .select_window(min, SectionPos::ZERO, 1)
                .unwrap()
                .sections,
            [SectionPos::ZERO].into()
        );
        assert!(
            approval
                .select_window(min, min, 0)
                .unwrap()
                .sections
                .is_empty()
        );
    }

    #[test]
    fn sparse_approval_keeps_holes_and_uses_actual_intersection_size() {
        let end = SectionPos::new(2, 0, 0);
        let scope = ReplicaScope {
            sections: [SectionPos::ZERO, end].into(),
        };
        let approval = VisualTerrainApproval::from(scope.clone());
        assert!(!approval.contains(SectionPos::new(1, 0, 0)));
        assert_eq!(
            approval.select_window(SectionPos::ZERO, end, 2).unwrap(),
            scope
        );
        assert_eq!(
            approval
                .select_window(SectionPos::ZERO, end, 1)
                .unwrap_err()
                .code,
            FaultCode::ResourceLimit
        );
    }
}
