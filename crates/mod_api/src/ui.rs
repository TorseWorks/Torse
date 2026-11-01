//! Minimal declarative machine presentation over an authorized replica.
//!
//! Rebuild after each complete mirror update. Missing fields mean unavailable,
//! never zero. Drop the view on disconnect/reset and rebuild from the new seed.
//! Action bindings describe intent only; the server checks every target again.

use std::collections::BTreeMap;
use torse_types::{ContentKey, EntityId};
use torse_world::{ComponentFields, ComponentValue};

fn key(text: &str) -> ContentKey {
    ContentKey::parse(text).expect("static UI key")
}

/// Fixed, bounded component tree understood by the first machine renderer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MachineDescription {
    pub version: u32,
    pub title: ContentKey,
    pub schema: ContentKey,
    pub progress: ContentKey,
    pub input: ContentKey,
    pub output: ContentKey,
    pub steps: u32,
    pub nodes: [Node; 4],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Node {
    Progress,
    InputSummary,
    OutputSummary,
    TransferOne,
}

/// Native example description; labels resolve through client resources.
pub fn machine() -> MachineDescription {
    MachineDescription {
        version: 1,
        title: key("machine:text/title"),
        schema: key("machine:schema/state"),
        progress: key("machine:field/progress"),
        input: key("machine:field/input"),
        output: key("machine:field/output"),
        steps: 4,
        nodes: [
            Node::Progress,
            Node::InputSummary,
            Node::OutputSummary,
            Node::TransferOne,
        ],
    }
}

/// Renderer input, derived solely from fields present in the authorized mirror.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MachineView {
    pub entity: EntityId,
    pub progress: u32,
    pub steps: u32,
    pub input: Option<InventorySummary>,
    pub output: Option<InventorySummary>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InventorySummary {
    pub entity: EntityId,
    pub items: BTreeMap<ContentKey, u32>,
}

/// A versioned action binding. Attach the current session's RequestSeq when sent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionIntent {
    pub action: ContentKey,
    pub version: u32,
    pub target: EntityId,
    pub parameters: ComponentFields,
}

impl MachineView {
    /// `component` must read the current authorized projection, never a store
    /// snapshot. Returning None clears the entire machine view.
    pub fn rebuild<'a>(
        description: &MachineDescription,
        entity: EntityId,
        component: impl Fn(EntityId, &ContentKey) -> Option<&'a ComponentFields>,
    ) -> Option<Self> {
        if description.version != 1 || description.steps == 0 {
            return None;
        }
        let fields = component(entity, &description.schema)?;
        let ComponentValue::U32(progress) = fields.get(&description.progress)? else {
            return None;
        };
        if *progress >= description.steps {
            return None;
        }
        let summary = |field| {
            let ComponentValue::Entity(id) = fields.get(field)? else {
                return None;
            };
            inventory_summary(*id, component(*id, &key("inventory:schema/container"))?)
        };
        Some(Self {
            entity,
            progress: *progress,
            steps: description.steps,
            input: summary(&description.input),
            output: summary(&description.output),
        })
    }

    /// Move one item between the example's first slots. Visibility does not
    /// imply transfer permission; server-side ActionService decides the result.
    pub fn transfer_one(&self) -> Option<ActionIntent> {
        let input = self.input.as_ref()?;
        let output = self.output.as_ref()?;
        Some(ActionIntent {
            action: key("inventory:action/transfer"),
            version: 2,
            target: input.entity,
            parameters: BTreeMap::from([
                (key("inventory:field/source_slot"), ComponentValue::U32(0)),
                (
                    key("inventory:field/destination"),
                    ComponentValue::Entity(output.entity),
                ),
                (
                    key("inventory:field/destination_slot"),
                    ComponentValue::U32(0),
                ),
                (key("inventory:field/count"), ComponentValue::U32(1)),
            ]),
        })
    }
}

fn inventory_summary(entity: EntityId, fields: &ComponentFields) -> Option<InventorySummary> {
    torse_world::component_size(fields).ok()?;
    let ComponentValue::List(slots) = fields.get(&key("inventory:field/slots"))? else {
        return None;
    };
    if fields.len() != 1 || slots.is_empty() || slots.len() > 64 {
        return None;
    }
    let mut items = BTreeMap::<ContentKey, u32>::new();
    for slot in slots {
        let ComponentValue::Record(fields) = slot else {
            return None;
        };
        if fields.is_empty() {
            continue;
        }
        let ComponentValue::Content(item) = fields.get(&key("inventory:field/item"))? else {
            return None;
        };
        let ComponentValue::U32(count) = fields.get(&key("inventory:field/count"))? else {
            return None;
        };
        let valid_envelope = match (
            fields.get(&key("inventory:field/data_format")),
            fields.get(&key("inventory:field/data")),
        ) {
            (None, None) => fields.len() == 2,
            (Some(ComponentValue::U32(1)), Some(ComponentValue::Record(_))) => fields.len() == 4,
            _ => false,
        };
        if !valid_envelope || *count == 0 {
            return None;
        }
        // This display total does not decide slot compatibility. Transfer intents
        // retain source slots, and authority checks the complete instance data.
        let total = items.entry(item.clone()).or_default();
        *total = total.checked_add(*count)?;
    }
    Some(InventorySummary { entity, items })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_accept_versioned_instances_without_changing_payloads() {
        let mut stack: ComponentFields = [
            (
                key("inventory:field/item"),
                ComponentValue::Content(key("base:stone")),
            ),
            (key("inventory:field/count"), ComponentValue::U32(2)),
            (key("inventory:field/data_format"), ComponentValue::U32(1)),
            (
                key("inventory:field/data"),
                ComponentValue::Record(
                    [(
                        key("extension:unknown"),
                        ComponentValue::Text("keep exactly".into()),
                    )]
                    .into(),
                ),
            ),
        ]
        .into();
        let fields = |stack| {
            ComponentFields::from([(
                key("inventory:field/slots"),
                ComponentValue::List(vec![ComponentValue::Record(stack)]),
            )])
        };
        let input = fields(stack.clone());
        let original = input.clone();
        assert_eq!(
            inventory_summary(EntityId(1), &input).unwrap().items[&key("base:stone")],
            2
        );
        assert_eq!(input, original);
        stack.insert(key("inventory:field/data_format"), ComponentValue::U32(2));
        assert!(inventory_summary(EntityId(1), &fields(stack.clone())).is_none());
        stack.remove(&key("inventory:field/data_format"));
        assert!(inventory_summary(EntityId(1), &fields(stack)).is_none());
    }

    #[test]
    fn summaries_reject_overflow_invalid_slots_and_excess_output() {
        let occupied = |count| {
            ComponentValue::Record(BTreeMap::from([
                (
                    key("inventory:field/item"),
                    ComponentValue::Content(key("base:stone")),
                ),
                (key("inventory:field/count"), ComponentValue::U32(count)),
            ]))
        };
        let fields = |slots| {
            ComponentFields::from([(key("inventory:field/slots"), ComponentValue::List(slots))])
        };
        for slots in [
            vec![],
            vec![occupied(0)],
            vec![ComponentValue::U32(1)],
            vec![occupied(u32::MAX), occupied(1)],
            vec![occupied(1); 65],
        ] {
            assert!(inventory_summary(EntityId(1), &fields(slots)).is_none());
        }
        let summary =
            inventory_summary(EntityId(1), &fields(vec![occupied(2), occupied(3)])).unwrap();
        assert_eq!(summary.items[&key("base:stone")], 5);
    }

    #[test]
    fn missing_or_invalid_progress_clears_view_instead_of_retaining_old_fields() {
        let mut description = machine();
        let mut fields =
            ComponentFields::from([(description.progress.clone(), ComponentValue::U32(1))]);
        assert_eq!(
            MachineView::rebuild(&description, EntityId(1), |_, _| Some(&fields))
                .unwrap()
                .progress,
            1
        );
        assert!(MachineView::rebuild(&description, EntityId(1), |_, _| None).is_none());
        fields.insert(description.progress.clone(), ComponentValue::U32(4));
        assert!(MachineView::rebuild(&description, EntityId(1), |_, _| Some(&fields)).is_none());
        fields.insert(description.progress.clone(), ComponentValue::U32(1));
        description.version = 2;
        assert!(MachineView::rebuild(&description, EntityId(1), |_, _| Some(&fields)).is_none());
    }
}
