use super::*;
impl WindowApp {
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
                self.materials = scene.item_blocks();
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
            Some(owner) => torse_items::edit_call(owner, place, self.material as u32, target, half),
            None => crate::net::edit_call(place, block, target, half),
        }
    }
    pub(super) fn inventory_slot_clicked(&mut self, slot: usize) {
        let Some(owner) = self.scene.as_ref().and_then(Scene::item_owner) else {
            return;
        };
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
}
