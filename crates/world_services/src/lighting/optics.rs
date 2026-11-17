use sha2::{Digest as _, Sha256};
use torse_types::{Digest, Fault};
use torse_world::{BlockStateId, BlockStateRegistry};

/// Monochromatic optical coefficients, in solver levels (0..=15).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OpticalState {
    pub absorption: u8,
    pub emission: u8,
}

/// Lookup bound to an immutable content identity for the life of an observation.
pub trait LightOptics {
    fn identity(&self) -> Digest;
    fn state(&self, id: BlockStateId) -> Option<OpticalState>;
}

/// Validate at the solver boundary as hosts can implement LightOptics directly.
pub(super) fn checked_state(
    optics: &(impl LightOptics + ?Sized),
    id: BlockStateId,
) -> Result<OpticalState, Fault> {
    let state = optics
        .state(id)
        .ok_or_else(|| Fault::invalid_data("unregistered lighting state"))?;
    if state.absorption > super::MAX_LIGHT || state.emission > super::MAX_LIGHT {
        return Err(Fault::invalid_data("invalid lighting coefficients"));
    }
    Ok(state)
}

impl LightOptics for BlockStateRegistry {
    fn identity(&self) -> Digest {
        self.authority_digest()
    }

    fn state(&self, id: BlockStateId) -> Option<OpticalState> {
        if id.is_empty() {
            return Some(OpticalState::default());
        }
        self.light_transport(id).map(|light| OpticalState {
            absorption: (light.absorption * 15.0).ceil() as u8,
            emission: (light.emission * 15.0).round() as u8,
        })
    }
}

/// Immutable host-supplied coefficients indexed by the bound registry's state IDs.
pub struct LightOpticsTable {
    registry: Digest,
    identity: Digest,
    states: Box<[OpticalState]>,
}

impl LightOpticsTable {
    /// Include a nonzero host version in the identity along with the coefficients
    /// and registry identity. Empty space must remain transparent and nonemissive.
    pub fn new(
        registry: &BlockStateRegistry,
        version: u64,
        states: Vec<OpticalState>,
    ) -> Result<Self, Fault> {
        if version == 0
            || states.len() != registry.state_count() as usize
            || states.first() != Some(&OpticalState::default())
            || states
                .iter()
                .any(|s| s.absorption > super::MAX_LIGHT || s.emission > super::MAX_LIGHT)
        {
            return Err(Fault::invalid_data("invalid light optics table"));
        }
        let registry = registry.authority_digest();
        let mut hash = Sha256::new();
        hash.update(b"torse-light-optics-v1");
        hash.update(registry.0);
        hash.update(version.to_le_bytes());
        for s in &states {
            hash.update([s.absorption, s.emission]);
        }
        Ok(Self {
            registry,
            identity: Digest(hash.finalize().into()),
            states: states.into_boxed_slice(),
        })
    }

    pub const fn registry_identity(&self) -> Digest {
        self.registry
    }

    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

impl LightOptics for LightOpticsTable {
    fn identity(&self) -> Digest {
        self.identity
    }

    fn state(&self, id: BlockStateId) -> Option<OpticalState> {
        self.states.get(usize::from(id.0)).copied()
    }
}
