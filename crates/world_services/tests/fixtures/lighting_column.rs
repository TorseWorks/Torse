use super::lighting_api::{LightInputVersion, LightLimits, LightOptics, LightSource, OpticalState};
use torse_types::{Binding, BlockPos, Compatibility, Digest, SectionPos, WorldEpoch, WorldId};
use torse_world::{BlockStateId, TerrainQuery, WorldBounds};

pub struct ColumnSource {
    pub height: i64,
    pub pattern: u8,
}
impl LightSource for ColumnSource {
    fn side(&self) -> u32 {
        16
    }
    fn bounds(&self) -> WorldBounds {
        WorldBounds::new(0, self.height).unwrap()
    }
    fn version(&self, pos: SectionPos) -> LightInputVersion {
        if self.pattern == 2 && pos.x < 0 && pos.z < 0 {
            LightInputVersion::Unknown
        } else {
            LightInputVersion::Content(Digest([self.pattern + 1; 32]))
        }
    }
    fn query(&self, pos: BlockPos) -> TerrainQuery {
        if pos.y < 0 || pos.y >= self.height {
            return TerrainQuery::OutOfBounds;
        }
        if self.pattern == 2 && pos.x < 0 && pos.z < 0 {
            return TerrainQuery::NotLoaded;
        }
        let id = if self.pattern == 0 {
            0
        } else if pos.y == self.height / 2 && (pos.x + pos.z).rem_euclid(7) != 0 {
            1
        } else if pos.x.rem_euclid(11) == 0
            && pos.z.rem_euclid(13) == 0
            && pos.y.rem_euclid(17) == 0
        {
            2
        } else if pos.y < self.height / 3 && pos.x.rem_euclid(5) == 0 {
            3
        } else {
            0
        };
        TerrainQuery::Ready(BlockStateId(id))
    }
}
pub struct Optics;
impl LightOptics for Optics {
    fn identity(&self) -> Digest {
        Digest([7; 32])
    }
    fn state(&self, id: BlockStateId) -> Option<OpticalState> {
        match id.0 {
            0 => Some(OpticalState {
                absorption: 0,
                emission: 0,
            }),
            1 => Some(OpticalState {
                absorption: 15,
                emission: 0,
            }),
            2 => Some(OpticalState {
                absorption: 1,
                emission: 12,
            }),
            3 => Some(OpticalState {
                absorption: 2,
                emission: 0,
            }),
            _ => None,
        }
    }
}
pub fn binding() -> Binding {
    Binding {
        world: WorldId(1),
        epoch: WorldEpoch(1),
        compatibility: Compatibility {
            authority_schema: 1,
            rules_digest: Digest([2; 32]),
            content_digest: Digest([3; 32]),
        },
    }
}
pub fn limits() -> LightLimits {
    LightLimits {
        cells: 1_000_000,
        dependencies: 512,
        frontier: 1_000_000,
        bytes: 64 * 1024 * 1024,
    }
}
