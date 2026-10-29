//! Optional stock belongs to the player identity, independently of a walk body.
use super::*;
use torse_inventory::{Inventory, ItemStack};
use torse_types::{EntityId, EvictionRank, MemoryClass, MemoryLease};

pub(super) struct Stock {
    catalog: torse_items::Catalog,
    owner: EntityId,
    _memory: Option<MemoryLease>,
}

impl Scene {
    pub fn enable_items(&mut self, owner: Option<EntityId>) -> Result<(), String> {
        if self.items.is_some() {
            return Ok(());
        }
        let catalog = if self.layered.is_some() {
            torse_items::block_catalog(self.authority.registry())
        } else {
            torse_items::fixture_catalog(self.authority.registry())
        }
        .map_err(|error| format!("item catalog: {error:?}"))?;
        let memory = self
            .reserve_memory(
                MemoryClass::World,
                catalog.accounted_bytes(),
                EvictionRank::Pinned,
            )
            .map_err(|error| error.to_string())?;
        let mut descriptors = block_actions::descriptors();
        descriptors.extend(torse_inventory::descriptors());
        descriptors.extend(torse_items::descriptors());
        let mut handlers = torse_items::block_handlers();
        handlers.extend(torse_inventory::handlers(catalog.stack_rules().clone()));
        handlers.extend(torse_items::handlers(catalog.clone()));
        let service = ActionService::bind(descriptors, handlers, self.authority.schemas())
            .map_err(|error| error.to_string())?;
        let owner = if let Some(owner) = owner {
            // An authorized replica must already contain this player's inventory.
            let handle = self
                .authority
                .entities()
                .handle_of(owner)
                .ok_or("inventory player missing")?;
            let fields = self
                .authority
                .entities()
                .get(handle)
                .and_then(|entity| entity.components.component(&torse_inventory::schema()))
                .ok_or("inventory access unavailable")?;
            Inventory::from_fields(fields, catalog.stack_rules())
                .map_err(|error| format!("inventory: {error:?}"))?;
            owner
        } else {
            if self.is_network() {
                return Err("network inventory requires an assigned player".into());
            }
            if self.authority.seq().0 != 0 {
                return Err("initial stock requires a fresh local world".into());
            }
            let mut components = ComponentSet::default();
            components
                .insert(
                    self.authority.schemas(),
                    &torse_inventory::schema(),
                    catalog
                        .starter(36)
                        .map_err(|error| format!("starter stock: {error:?}"))?
                        .fields(),
                )
                .map_err(|error| error.to_string())?;
            components
                .insert(
                    self.authority.schemas(),
                    &torse_world::collider_schema(),
                    torse_world::EntityCollider::Disabled.fields(),
                )
                .map_err(|error| error.to_string())?;
            let component_bytes: u64 = components
                .ids()
                .map(|schema| {
                    torse_world::component_memory_bytes(
                        components.component(schema).expect("new stock component"),
                    )
                })
                .sum();
            let growth = self
                .reserve_memory(
                    MemoryClass::World,
                    self.authority
                        .entities()
                        .accounted_bytes()
                        .saturating_add(16384)
                        .saturating_add(component_bytes),
                    EvictionRank::Pinned,
                )
                .map_err(|error| error.to_string())?;
            let handle = self
                .authority
                .entities_mut()
                .spawn(Position::new(8.5, 14.0, 8.5))
                .map_err(|error| error.to_string())?;
            *self
                .authority
                .entities_mut()
                .components_mut(handle)
                .expect("new stock") = components;
            let id = self.authority.entities().get(handle).expect("new stock").id;
            self.reconcile_world_memory(growth)
                .map_err(|error| error.to_string())?;
            id
        };
        self.service = service;
        self.items = Some(Stock {
            catalog,
            owner,
            _memory: memory,
        });
        Ok(())
    }
    pub fn item_owner(&self) -> Option<EntityId> {
        self.items.as_ref().map(|stock| stock.owner)
    }
    pub fn inventory_slots(&self) -> Vec<Option<ItemStack>> {
        let Some(stock) = &self.items else {
            return vec![];
        };
        self.authority
            .entities()
            .handle_of(stock.owner)
            .and_then(|handle| self.authority.entities().get(handle))
            .and_then(|entity| entity.components.component(&torse_inventory::schema()))
            .and_then(|fields| Inventory::from_fields(fields, stock.catalog.stack_rules()).ok())
            .map(|inventory| inventory.slots().to_vec())
            .unwrap_or_default()
    }
    pub fn item_blocks(&self) -> Vec<ContentKey> {
        self.inventory_slots()
            .iter()
            .take(9)
            .map(|stack| {
                stack
                    .as_ref()
                    .and_then(|stack| self.items.as_ref()?.catalog.block(&stack.item))
                    .cloned()
                    .unwrap_or_else(|| key("client:empty_slot"))
            })
            .collect()
    }
    pub fn apply_item_call(&mut self, call: torse_sim::durable::Call) -> Result<(), String> {
        if self.is_network() {
            return Err("replicas cannot commit inventory actions".into());
        }
        let owner = self.item_owner().ok_or("inventory is not enabled")?;
        self.edit_counter += 1;
        let counter = self.edit_counter;
        let actor = [
            "world:permission/edit",
            "items:permission/use",
            "inventory:permission/insert",
            "inventory:permission/consume",
            "inventory:permission/swap",
        ]
        .into_iter()
        .fold(
            ActorContext::new(self.principal.clone())
                .target(ActionTarget::Entity(owner))
                .target(call.target),
            |actor, permission| actor.permission(key(permission)),
        );
        let request = ActionRequest {
            action: call.action,
            api_version: call.version,
            origin: ActionOrigin::Ui,
            target: call.target,
            parameters: call.parameters,
            transaction: TransactionId(u128::from(counter)),
            ordering: OrderingKey {
                phase: Phase::Interaction,
                rule_priority: 0,
                stable_task_key: counter,
                local_sequence: 0,
            },
        };
        let preview = self
            .reserve_memory(
                MemoryClass::Executor,
                super::lighting::PREVIEW_BYTES,
                EvictionRank::Pinned,
            )
            .map_err(|error| error.to_string())?;
        let surfaces = match request.target {
            ActionTarget::Block(pos) => self.prepare_surfaces(pos)?,
            _ => Vec::new(),
        };
        let output = self
            .service
            .plan(&self.authority, &actor, Phase::Interaction, &request)
            .map_err(|error| format!("{error:?}"))?;
        let prepared = match output {
            torse_sim::action::ActionOutput::Mutation(tx) => self.authority.prepare(&tx),
            torse_sim::action::ActionOutput::Atomic(group) => self.authority.prepare_group(&group),
            _ => return Err("inventory action must mutate".into()),
        }
        .map_err(|error| format!("{error:?}"))?;
        let growth = self
            .reserve_memory(
                MemoryClass::World,
                self.authority.growth_reservation(&prepared),
                EvictionRank::Pinned,
            )
            .map_err(|error| error.to_string())?;
        self.authority
            .publish(prepared)
            .map_err(|error| format!("{error:?}"))?;
        self.surfaces.record(surfaces);
        if let ActionTarget::Block(pos) = request.target {
            self.preview_lighting(&[pos])?;
        }
        drop(preview);
        self.reconcile_world_memory(growth)
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vertical_glass_places_each_side_and_rejects_wrong_axis_completion() {
        for side in 0..4 {
            let mut scene = Scene::new(0).unwrap();
            scene.enable_items(None).unwrap();
            let owner = scene.item_owner().unwrap();
            let pos = BlockPos::new(8, 14, 8);
            scene
                .apply_item_call(torse_items::edit_call(owner, true, 3, pos, side))
                .unwrap();
            let state = scene.authority.store().query(pos).ready().unwrap();
            assert_eq!(
                scene.authority.registry().state_values(state),
                Some([side as u16].as_slice())
            );
            let before = scene.inventory_slots();
            assert!(
                scene
                    .apply_item_call(torse_items::edit_call(owner, true, 3, pos, side ^ 2))
                    .is_err()
            );
            assert_eq!(scene.inventory_slots(), before);
            scene
                .apply_item_call(torse_items::edit_call(owner, true, 3, pos, side ^ 1))
                .unwrap();
            let state = scene.authority.store().query(pos).ready().unwrap();
            assert_eq!(
                scene.authority.registry().definition(state).unwrap().id,
                key("base:glass")
            );
        }
    }
    #[test]
    fn local_inventory_survives_body_switches_and_stock_edits_are_atomic() {
        let mut scene = Scene::new(0).unwrap();
        let pool = torse_types::MemoryPool::new(torse_types::MemoryBudget::new(128 * 1024 * 1024));
        scene.attach_memory(pool.clone()).unwrap();
        scene.enable_items(None).unwrap();
        let owner = scene.item_owner().unwrap();
        assert_eq!(scene.authority.entities().len(), 1);
        assert!(scene.entity_geometry().0.is_empty());
        let pos = BlockPos::new(8, 14, 8);
        let before = scene.inventory_slots();
        scene
            .apply_item_call(torse_items::edit_call(owner, true, 2, pos, 0))
            .unwrap();
        assert_eq!(
            scene.inventory_slots()[2].as_ref().unwrap().count,
            before[2].as_ref().unwrap().count - 1
        );
        let after = scene.inventory_slots();
        assert!(
            scene
                .apply_item_call(torse_items::edit_call(owner, true, 2, pos, 0))
                .is_err()
        );
        assert_eq!(scene.inventory_slots(), after);
        for _ in 0..4 {
            scene.spawn_walker(Position::new(8.5, 65.0, 8.5)).unwrap();
            scene.stop_walker().unwrap();
            scene.enable_items(None).unwrap();
            assert_eq!(scene.item_owner(), Some(owner));
            assert_eq!(scene.authority.entities().len(), 1);
            assert_eq!(scene.inventory_slots(), after);
        }
        scene
            .apply_item_call(torse_items::edit_call(owner, false, 2, pos, 0))
            .unwrap();
        assert_eq!(scene.inventory_slots(), before);
        drop(scene);
        assert_eq!(pool.usage().total, 0);
    }
}
