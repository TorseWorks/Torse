use super::*;
impl WindowApp {
    pub(super) fn submit_item_call(&mut self, call: torse_sim::durable::Call) {
        if let Some(net) = self.net.as_mut() {
            match net.request(call) {
                Ok(_) => {
                    self.requests_sent += 1;
                    self.last_result.clear();
                }
                Err(error) => self.last_result = format!("item request failed: {error}"),
            }
        } else if let Some(scene) = self.scene.as_mut() {
            match scene.apply_item_call(call) {
                Ok(()) => {
                    self.actions_ok += 1;
                    self.last_result.clear();
                }
                Err(error) => {
                    self.actions_rejected += 1;
                    self.last_result = error;
                }
            }
        }
    }
    pub(super) fn nearby_pickup(&self) -> Option<torse_inventory::ItemStack> {
        let scene = self.scene.as_ref()?;
        if scene.supply_mode() == torse_items::supply::SupplyMode::Infinite {
            return None;
        }
        let eye = self.camera.as_ref()?.eye;
        scene
            .nearest_drop(Position::new(eye[0] as f64, eye[1] as f64, eye[2] as f64))
            .map(|(_, stack)| stack)
    }
    pub(super) fn pickup_item(&mut self, manual: bool) {
        let Some(camera) = &self.camera else {
            return;
        };
        let eye = Position::new(
            camera.eye[0] as f64,
            camera.eye[1] as f64,
            camera.eye[2] as f64,
        );
        let Some(scene) = &self.scene else {
            return;
        };
        if scene.supply_mode() == torse_items::supply::SupplyMode::Infinite {
            return;
        }
        let Some(owner) = scene.item_owner() else {
            return;
        };
        let Some((drop, _)) = scene.nearest_drop(eye) else {
            if manual {
                self.last_result = "no reachable item".into();
            }
            return;
        };
        let mut call = torse_items::pickup_call(owner, drop);
        if self.net.is_none() {
            call = torse_items::with_position(call, eye);
        }
        self.submit_item_call(call);
    }
    pub(super) fn throw_item(&mut self) {
        let Some(camera) = &self.camera else {
            return;
        };
        let Some(owner) = self.scene.as_ref().and_then(Scene::item_owner) else {
            return;
        };
        let eye = Position::new(
            camera.eye[0] as f64,
            camera.eye[1] as f64,
            camera.eye[2] as f64,
        );
        let mut call = torse_items::throw_call(owner, self.material as u32);
        if let Some(scene) = &self.scene
            && scene.supply_mode() == torse_items::supply::SupplyMode::Infinite
        {
            let Some(item) = self
                .materials
                .get(self.material)
                .and_then(|block| scene.item_for_block(block))
            else {
                return;
            };
            call = torse_items::supply::with_item(call, item);
        }
        if self.net.is_none() {
            call = torse_items::with_position(call, eye);
        }
        self.submit_item_call(call);
    }
    pub(super) fn displayed_inventory(&self) -> Vec<Option<torse_inventory::ItemStack>> {
        self.scene.as_ref().map_or_else(Vec::new, |scene| {
            if scene.supply_mode() == torse_items::supply::SupplyMode::Infinite {
                scene
                    .item_catalog_page(self.ui.catalog_page)
                    .into_iter()
                    .map(Some)
                    .collect()
            } else {
                scene.inventory_slots()
            }
        })
    }
    pub(super) fn displayed_bar(&self) -> Vec<Option<torse_inventory::ItemStack>> {
        self.scene.as_ref().map_or_else(Vec::new, |scene| {
            if scene.supply_mode() == torse_items::supply::SupplyMode::Infinite {
                self.ui
                    .settings
                    .supply_slots
                    .iter()
                    .take(9)
                    .map(|id| {
                        ContentKey::parse(id)
                            .ok()
                            .and_then(|id| scene.catalog_stack(&id))
                    })
                    .collect()
            } else {
                scene.inventory_slots().into_iter().take(9).collect()
            }
        })
    }
    pub(super) fn sync_inventory(&mut self) {
        if let Some(scene) = self.scene.as_mut() {
            if scene.item_owner().is_none()
                && scene.is_network()
                && let Some(owner) = self.net.as_ref().and_then(|net| net.player_entity())
                && scene
                    .authority()
                    .entities()
                    .handle_of(owner)
                    .and_then(|handle| scene.authority().entities().get(handle))
                    .is_some_and(|entity| {
                        entity
                            .components
                            .component(&torse_inventory::schema())
                            .is_some()
                    })
                && let Err(error) = scene.enable_items(Some(owner))
            {
                self.last_result = error;
            }
            if scene.item_owner().is_some() {
                self.ui.settings.infinite_supply =
                    scene.supply_mode() == torse_items::supply::SupplyMode::Infinite;
                if self.ui.settings.infinite_supply {
                    let catalog = scene.item_catalog_page(0);
                    if self.ui.settings.supply_slots.is_empty() {
                        self.ui.settings.supply_slots = catalog
                            .iter()
                            .take(9)
                            .map(|entry| entry.item.to_string())
                            .collect();
                    }
                    self.materials = self
                        .ui
                        .settings
                        .supply_slots
                        .iter()
                        .take(9)
                        .map(|id| {
                            ContentKey::parse(id)
                                .ok()
                                .and_then(|id| scene.item_block(&id))
                                .unwrap_or_else(|| torse_items::key("client:empty_slot"))
                        })
                        .collect();
                } else {
                    self.materials = scene.item_blocks();
                }
                self.materials.resize_with(9, || {
                    ContentKey::parse("client:empty_slot").expect("slot key")
                });
                self.material = self.material.min(8);
            }
        }
    }
    pub(super) fn edit_call(
        &self,
        place: bool,
        block: &ContentKey,
        target: torse_types::BlockPos,
        half: u32,
    ) -> torse_sim::durable::Call {
        match self.scene.as_ref().and_then(Scene::item_owner) {
            Some(owner) => {
                let call = torse_items::edit_call(owner, place, self.material as u32, target, half);
                self.scene
                    .as_ref()
                    .filter(|scene| {
                        scene.supply_mode() == torse_items::supply::SupplyMode::Infinite
                    })
                    .and_then(|scene| scene.item_for_block(block))
                    .map_or_else(
                        || call.clone(),
                        |item| torse_items::supply::with_item(call.clone(), item),
                    )
            }
            None => crate::net::edit_call(place, block, target, half),
        }
    }
    pub(super) fn inventory_slot_clicked(&mut self, slot: usize) {
        let Some(owner) = self.scene.as_ref().and_then(Scene::item_owner) else {
            return;
        };
        if self
            .scene
            .as_ref()
            .is_some_and(|scene| scene.supply_mode() == torse_items::supply::SupplyMode::Infinite)
        {
            if let Some(Some(stack)) = self.displayed_inventory().get(slot) {
                self.ui.settings.supply_slots.resize(9, String::new());
                self.ui.settings.supply_slots[self.material.min(8)] = stack.item.to_string();
                self.sync_inventory();
                self.save_preferences();
            }
            return;
        }
        if slot >= 36 {
            return;
        }
        let Some(source) = self.ui.inventory_source.take() else {
            self.ui.inventory_source = Some(slot);
            return;
        };
        if source == slot {
            return;
        }
        let call = torse_items::swap_call(owner, source as u32, slot as u32);
        if let Some(net) = self.net.as_mut() {
            match net.request(call) {
                Ok(seq) => {
                    self.requests_sent += 1;
                    self.last_result = format!("seq {} sent", seq.0);
                }
                Err(error) => self.last_result = format!("inventory send failed: {error}"),
            }
        } else if let Some(scene) = self.scene.as_mut() {
            match scene.apply_item_call(call) {
                Ok(()) => {
                    self.actions_ok += 1;
                    self.last_result.clear();
                }
                Err(error) => {
                    self.actions_rejected += 1;
                    self.last_result = error;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn two_slot_clicks_swap_authority_and_update_the_actual_hotbar() {
        let mut app = super::super::tests::app();
        let mut scene = Scene::new(0).unwrap();
        scene.enable_items(None).unwrap();
        let before = scene.inventory_slots();
        app.scene = Some(scene);
        app.sync_inventory();
        app.inventory_slot_clicked(2);
        assert_eq!(app.scene.as_ref().unwrap().inventory_slots(), before);
        app.inventory_slot_clicked(8);
        let after = app.scene.as_ref().unwrap().inventory_slots();
        assert!(after[2].is_none());
        assert_eq!(after[8], before[2]);
        app.sync_inventory();
        assert_eq!(app.materials.len(), 9);
        assert_eq!(app.materials[8].to_string(), "base:stone");
        app.inventory_slot_clicked(8);
        app.set_ui_mode(crate::ui::Mode::Game);
        assert_eq!(app.ui.inventory_source, None);
        assert_eq!(app.scene.as_ref().unwrap().inventory_slots(), after);
    }
    #[test]
    fn supply_catalog_selection_does_not_swap_or_consume_finite_slots() {
        let mut app = super::super::tests::app();
        let mut scene = Scene::new(0).unwrap();
        scene.enable_items(None).unwrap();
        let owner = scene.item_owner().unwrap();
        let before = scene.inventory_slots();
        scene
            .apply_item_call(torse_items::supply::call(
                owner,
                torse_items::supply::SupplyMode::Infinite,
            ))
            .unwrap();
        app.scene = Some(scene);
        app.sync_inventory();
        app.material = 7;
        app.inventory_slot_clicked(0);
        assert_eq!(app.materials[7], torse_items::key("base:glass_slab"));
        assert_eq!(app.scene.as_ref().unwrap().inventory_slots(), before);
        assert_eq!(
            app.displayed_bar()[7].as_ref().unwrap().item,
            torse_items::key("base:glass_slab")
        );
        app.scene
            .as_mut()
            .unwrap()
            .apply_item_call(torse_items::supply::call(
                owner,
                torse_items::supply::SupplyMode::Finite,
            ))
            .unwrap();
        app.sync_inventory();
        assert_eq!(app.scene.as_ref().unwrap().inventory_slots(), before);
        assert_eq!(app.materials[2], torse_items::key("base:stone"));
    }
}
