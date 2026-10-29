//! Owned world and cache reservations for the window scene.
use super::*;
use torse_types::{EvictionRank, Fault, FaultCode, MemoryClass, MemoryLease, MemoryPool};

pub(super) struct Memory {
    pub pool: MemoryPool,
    pub world: MemoryLease,
    pub meshes: BTreeMap<SectionPos, MemoryLease>,
    pub center: SectionPos,
    _control: MemoryLease,
}

pub(super) fn mesh_bytes(mesh: &MeshData) -> usize {
    size_of::<MeshData>() + mesh.allocated_bytes() + 8192
}
pub(super) fn mesh_work_bytes(side: u32) -> u64 {
    // Six quads per cell, including both tint channels and shading. Growable
    // vectors can retain almost twice their payload while reallocating.
    (side as u64)
        .pow(3)
        .saturating_mul(6 * 268 * 3)
        .saturating_add(32768 + 16384)
}
impl Scene {
    pub(super) fn resize_visual_control(&mut self) -> Result<(), Fault> {
        if let Some(memory) = &mut self.memory {
            let wanted = ((self.radius * 2 + 1).pow(2) * self.layers) as u64;
            memory._control.resize(
                wanted
                    .saturating_mul(512)
                    .saturating_add(self.state_materials.len() as u64 * 4096)
                    .saturating_add(super::surfaces::CONTROL_BYTES),
            )?;
        }
        Ok(())
    }
    pub fn startup_world_reservation(pool: &MemoryPool, radius: i64) -> Result<MemoryLease, Fault> {
        if !(0..=16).contains(&radius) {
            return Err(Fault::invalid_data("scene radius must be 0..=16"));
        }
        let sections = (radius as u64 * 2 + 1).pow(2) * 4;
        pool.reserve(
            MemoryClass::World,
            sections * (16u64.pow(3) * 8 + 16384) + 16384,
            EvictionRank::Pinned,
        )
    }
    pub fn attach_memory_prepared(
        &mut self,
        pool: MemoryPool,
        world: MemoryLease,
    ) -> Result<(), Fault> {
        self.attach_memory_reserved(pool, Some(world))
    }
    pub fn reserve_frame_memory(&mut self, bytes: u64) -> Result<Option<MemoryLease>, Fault> {
        self.reserve_memory(MemoryClass::Codec, bytes, EvictionRank::Pinned)
    }
    #[cfg(test)]
    pub fn attach_memory(&mut self, pool: MemoryPool) -> Result<(), Fault> {
        self.attach_memory_reserved(pool, None)
    }
    fn attach_memory_reserved(
        &mut self,
        pool: MemoryPool,
        held: Option<MemoryLease>,
    ) -> Result<(), Fault> {
        if self.memory.is_some() {
            return Err(Fault::invalid_data("scene memory already attached"));
        }
        let bytes = self.world_memory_bytes();
        let mut world = match held {
            Some(lease) => lease,
            None => pool.reserve(MemoryClass::World, bytes, EvictionRank::Pinned)?,
        };
        if world.bytes() < bytes {
            return Err(Fault::invalid_data(
                "scene world reservation underestimated",
            ));
        }
        world.resize(bytes)?;
        let wanted = if self.is_network() {
            4096
        } else {
            ((self.radius * 2 + 1).pow(2) * self.layers) as u64
        };
        let control = pool.reserve(
            MemoryClass::Resource,
            wanted
                .saturating_mul(512)
                .saturating_add(self.state_materials.len() as u64 * 4096)
                .saturating_add(super::surfaces::CONTROL_BYTES),
            EvictionRank::Pinned,
        )?;
        let mut meshes = BTreeMap::new();
        for (pos, (_, mesh)) in &self.meshes {
            meshes.insert(
                *pos,
                pool.reserve(
                    MemoryClass::Mesh,
                    mesh_bytes(mesh) as u64,
                    EvictionRank::Mesh {
                        distance_sq: u64::MAX,
                    },
                )?,
            );
        }
        let background = self
            .background
            .as_ref()
            .map(|background| background.replacement_with_memory(pool.clone()))
            .transpose()?;
        self.palette.attach_memory(pool.clone())?;
        if background.is_some() {
            self.background = background;
        }
        self.memory = Some(Memory {
            pool,
            world,
            meshes,
            center: SectionPos::ZERO,
            _control: control,
        });
        Ok(())
    }
    /// Preflight replica expansion before cloning any world or component payload.
    pub fn from_snapshot_charged(
        snapshot: &ReplicaSnapshot,
        pool: MemoryPool,
    ) -> Result<Self, Fault> {
        let section = (snapshot.side as u64)
            .checked_pow(3)
            .and_then(|n| n.checked_mul(8))
            .and_then(|n| n.checked_add(16384))
            .ok_or_else(|| Fault::resource_limit("replica world reservation overflow"))?;
        let mut bytes = section
            .saturating_mul(snapshot.sections.len() as u64)
            .saturating_add(16384);
        for entity in &snapshot.entities {
            bytes = bytes.saturating_add(16384);
            for fields in entity.components.values() {
                bytes = bytes.saturating_add(torse_world::component_memory_bytes(fields));
            }
        }
        let world = pool.reserve(MemoryClass::World, bytes, EvictionRank::Pinned)?;
        let mut scene = Self::from_snapshot(snapshot).map_err(Fault::invalid_data)?;
        scene.attach_memory_reserved(pool, Some(world))?;
        Ok(scene)
    }
    pub(super) fn world_memory_bytes(&self) -> u64 {
        self.authority
            .accounted_bytes()
            .saturating_add(self.authority.store().len() as u64 * 4096)
    }
    pub(super) fn memory_pool(&self) -> Option<&MemoryPool> {
        self.memory.as_ref().map(|memory| &memory.pool)
    }
    /// Drop owned cache payloads before their leases. Committed world data is never selected.
    pub(super) fn evict_one_mesh(&mut self, center: SectionPos) -> bool {
        let Some(pos) = self.memory.as_ref().and_then(|memory| {
            memory.meshes.keys().copied().max_by_key(|pos| {
                let distance = |a: i64, b: i64| a.abs_diff(b).saturating_pow(2);
                distance(pos.x, center.x)
                    .saturating_add(distance(pos.y, center.y))
                    .saturating_add(distance(pos.z, center.z))
            })
        }) else {
            return false;
        };
        self.meshes.remove(&pos);
        self.memory
            .as_mut()
            .expect("cache account")
            .meshes
            .remove(&pos);
        true
    }
    pub(super) fn reserve_memory(
        &mut self,
        class: MemoryClass,
        bytes: u64,
        rank: EvictionRank,
    ) -> Result<Option<MemoryLease>, Fault> {
        let Some(pool) = self.memory_pool().cloned() else {
            return Ok(None);
        };
        loop {
            match pool.reserve(class, bytes, rank) {
                Ok(lease) => return Ok(Some(lease)),
                Err(error)
                    if error.code == FaultCode::ResourceLimit
                        && self
                            .evict_one_mesh(self.memory.as_ref().expect("memory pool").center) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
    }
    pub(super) fn reconcile_world_memory(
        &mut self,
        mut growth: Option<MemoryLease>,
    ) -> Result<(), Fault> {
        let bytes = self.world_memory_bytes();
        if let Some(memory) = &mut self.memory {
            if let Some(growth) = &mut growth {
                if bytes > memory.world.bytes().saturating_add(growth.bytes()) {
                    return Err(Fault::invalid_data(
                        "scene growth reservation underestimated",
                    ));
                }
                memory.world.absorb(growth, bytes)?;
            } else {
                if bytes > memory.world.bytes() {
                    return Err(Fault::invalid_data(
                        "scene changed without a world reservation",
                    ));
                }
                memory.world.resize(bytes)?;
            }
        }
        Ok(())
    }
    pub(super) fn forget_mesh(&mut self, pos: &SectionPos) {
        self.meshes.remove(pos);
        if let Some(memory) = &mut self.memory {
            memory.meshes.remove(pos);
        }
    }
    pub(super) fn install_mesh(
        &mut self,
        pos: SectionPos,
        stamp: MeshStamp,
        mesh: MeshData,
        lease: Option<MemoryLease>,
    ) {
        self.forget_mesh(&pos);
        self.meshes.insert(pos, (stamp, mesh));
        if let (Some(memory), Some(lease)) = (&mut self.memory, lease) {
            memory.meshes.insert(pos, lease);
        }
    }
    pub fn try_apply_changes(&mut self, changes: &ReplicaChanges) -> Result<bool, String> {
        let section = (self.side as u64)
            .pow(3)
            .saturating_mul(8)
            .saturating_add(16384);
        let mut bytes = section.saturating_mul(changes.sections.len() as u64);
        if !changes.entities.is_empty() || !changes.removed_entities.is_empty() {
            bytes = bytes.saturating_add(self.authority.entities().accounted_bytes());
        }
        for entity in &changes.entities {
            bytes = bytes.saturating_add(16384);
            for fields in entity.components.values() {
                bytes = bytes.saturating_add(torse_world::component_memory_bytes(fields));
            }
        }
        let growth = match self.reserve_memory(MemoryClass::World, bytes, EvictionRank::Pinned) {
            Ok(lease) => lease,
            Err(error) if error.code == FaultCode::ResourceLimit => return Ok(false),
            Err(error) => return Err(error.to_string()),
        };
        self.apply_changes_unbudgeted(changes)?;
        self.reconcile_world_memory(growth)
            .map_err(|error| error.to_string())?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stream_over_thousands_of_centers_keeps_world_reservations_bounded() {
        let pool = MemoryPool::new(torse_types::MemoryBudget::new(128 * 1024 * 1024));
        let mut scene = Scene::new(0).unwrap();
        scene.attach_memory(pool.clone()).unwrap();
        scene.enable_items(None).unwrap();
        let owner = scene.item_owner().unwrap();
        scene
            .apply_item_call(torse_items::edit_call(
                owner,
                true,
                2,
                BlockPos::new(8, 14, 8),
                0,
            ))
            .unwrap();
        scene.stream_around(SectionPos::new(1, 0, 0), 1).unwrap();
        scene.stream_around(SectionPos::ZERO, 1).unwrap();
        let before = pool.usage().current[MemoryClass::World.index()];
        for x in 1..=3072 {
            scene.stream_around(SectionPos::new(x, 0, 0), 1).unwrap();
            let used = pool.usage().current[MemoryClass::World.index()];
            assert!(
                used < before + 8 * 1024 * 1024,
                "world reservation kept growing at {x}: {used}"
            );
        }
        scene.stream_around(SectionPos::ZERO, 1).unwrap();
        assert!(scene.authority.store().revision(SectionPos::ZERO).is_some());
        drop(scene);
        assert_eq!(pool.usage().total, 0);
    }
    #[test]
    fn foreground_occupancy_denial_precedes_work_and_preserves_the_world() {
        let pool = MemoryPool::new(
            MemoryBudget::new(128 * 1024 * 1024).with_class_limit(MemoryClass::Mesh, 0),
        );
        let mut scene = Scene::new(0).unwrap();
        scene.attach_memory(pool.clone()).unwrap();
        let revision = scene.authority.store().revision(SectionPos::ZERO);
        assert!(scene.refresh_meshes().is_empty());
        assert_eq!(scene.authority.store().revision(SectionPos::ZERO), revision);
        assert_eq!(pool.usage().peak[MemoryClass::Executor.index()], 0);
        assert_eq!(pool.usage().peak[MemoryClass::Mesh.index()], 0);
        drop(scene);
        assert_eq!(pool.usage().total, 0);
    }
    use torse_types::MemoryBudget;
    #[test]
    fn pressure_evicts_meshes_keeps_world_and_releases_scene_and_palette() {
        let pool = MemoryPool::new(MemoryBudget::new(128 * 1024 * 1024));
        let mut scene = Scene::new(1).unwrap();
        scene.attach_memory(pool.clone()).unwrap();
        let palette = scene.palette.clone();
        assert_eq!(scene.refresh_meshes().len(), 9);
        let (vertices, _, faces) = scene.geometry();
        assert_eq!(scene.geometry_counts(), (vertices.len(), faces));
        drop(vertices);
        let sections = scene.sections.clone();
        let world = pool.usage().current[MemoryClass::World.index()];
        let before = pool.usage().total;
        let held = scene
            .reserve_frame_memory(128 * 1024 * 1024 - before + 1)
            .unwrap();
        assert!(scene.mesh_count() < 9);
        assert_eq!(scene.sections, sections);
        assert_eq!(pool.usage().current[MemoryClass::World.index()], world);
        drop(held);
        assert!(!scene.refresh_meshes().is_empty());
        drop(scene);
        assert_eq!(pool.usage().current[MemoryClass::World.index()], 0);
        assert!(pool.usage().total > 0);
        drop(palette);
        assert_eq!(pool.usage().total, 0);
    }
    #[test]
    fn bootstrap_rejects_before_world_allocation() {
        let pool = MemoryPool::new(MemoryBudget::new(1024));
        assert!(Scene::startup_world_reservation(&pool, 16).is_err());
        assert_eq!(pool.usage().total, 0);
    }
    #[test]
    fn denied_replica_growth_keeps_the_world_unchanged_and_can_retry() {
        let pool = MemoryPool::new(
            MemoryBudget::new(128 * 1024 * 1024).with_class_limit(MemoryClass::World, 1024 * 1024),
        );
        let mut scene = Scene::new(0).unwrap();
        scene.attach_memory(pool.clone()).unwrap();
        let held = pool
            .reserve(
                MemoryClass::World,
                1024 * 1024 - pool.usage().current[MemoryClass::World.index()],
                EvictionRank::Pinned,
            )
            .unwrap();
        let pos = SectionPos::new(1, 0, 0);
        let changes = ReplicaChanges {
            states: Vec::new(),
            sections: vec![SnapshotSection {
                pos,
                cells: SnapshotCells::Uniform(BlockStateId::EMPTY),
            }],
            removed_sections: Vec::new(),
            entities: Vec::new(),
            removed_entities: Vec::new(),
        };
        assert!(!scene.try_apply_changes(&changes).unwrap());
        assert!(scene.authority.store().section(pos).is_none());
        drop(held);
        assert!(scene.try_apply_changes(&changes).unwrap());
        assert!(scene.authority.store().section(pos).is_some());
        assert_eq!(
            pool.usage().current[MemoryClass::World.index()],
            scene.world_memory_bytes()
        );
        drop(scene);
        assert_eq!(pool.usage().total, 0);
    }
}
