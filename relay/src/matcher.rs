use std::collections::{HashMap, HashSet};

use serde_derive::Deserialize;

use crate::coords::{angle_diff_deg, quat_xyzw_to_yaw_deg, ue_dist_sq};
use crate::ipc::RmpActorPose;

/// Foundation furnishing slot summary returned by `GET /estates/{land_ident}/bridge/snapshot`.
#[allow(dead_code)]
#[derive(Clone, Debug, Deserialize)]
pub struct FoundationSlotInfo {
    pub flat_slot: u16,
    pub item_id: u32,
    pub catalog_id: u16,
    pub name: String,
    #[serde(default)]
    pub ue_loc: [f32; 3],
    #[serde(default)]
    pub ue_yaw_deg: f32,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawTransform {
    location: [f32; 3],
    rotation: [f32; 4],
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawFurnitureItem {
    name: String,
    item_id: u32,
    transform: RawTransform,
    attachments: Vec<RawFurnitureItem>,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawLayoutFile {
    interior_scale: Option<f32>,
    exterior_scale: Option<f32>,
    interior_furniture: Vec<RawFurnitureItem>,
    exterior_furniture: Vec<RawFurnitureItem>,
}

#[derive(Clone, Debug)]
struct FlattenedSnapshotItem {
    item_id: u32,
    name: String,
    ue_loc: [f32; 3],
    ue_yaw_deg: f32,
}

fn flatten_snapshot_items(
    items: &[RawFurnitureItem],
    scale: f32,
    out: &mut Vec<FlattenedSnapshotItem>,
) {
    let s = if scale.is_finite() && scale.abs() > 1e-4 {
        scale
    } else {
        1.0
    };
    let factor = 100.0 / s;
    for item in items {
        out.push(FlattenedSnapshotItem {
            item_id: item.item_id,
            name: item.name.clone(),
            ue_loc: [
                item.transform.location[0] * factor,
                item.transform.location[1] * factor,
                item.transform.location[2] * factor,
            ],
            ue_yaw_deg: quat_xyzw_to_yaw_deg(item.transform.rotation),
        });
        if !item.attachments.is_empty() {
            flatten_snapshot_items(&item.attachments, scale, out);
        }
    }
}

/// Parses a ReMakePlace JSON snapshot and enriches each live `AFurniture` actor with `item_id`
/// using geometric proximity & name affinity (works even across differing game/RMP locales).
pub fn enrich_actors_from_snapshot_json(
    snapshot_json: &str,
    is_outside: bool,
    actors: &[RmpActorPose],
) -> Vec<Option<u32>> {
    let mut out = vec![None; actors.len()];
    let Ok(parsed) = serde_json::from_str::<RawLayoutFile>(snapshot_json) else {
        return out;
    };

    let (raw_items, scale) = if is_outside {
        (
            &parsed.exterior_furniture,
            parsed.exterior_scale.unwrap_or(100.0),
        )
    } else {
        (
            &parsed.interior_furniture,
            parsed.interior_scale.unwrap_or(100.0),
        )
    };

    let mut snap_items = Vec::new();
    flatten_snapshot_items(raw_items, scale, &mut snap_items);
    if snap_items.is_empty() || actors.is_empty() {
        return out;
    }

    let mut pairs: Vec<(f32, f32, usize, usize)> =
        Vec::with_capacity(actors.len() * snap_items.len());
    for (ai, actor) in actors.iter().enumerate() {
        let actor_name = actor.item_name.trim().to_ascii_lowercase();
        for (si, snap) in snap_items.iter().enumerate() {
            let d2 = ue_dist_sq(actor.loc, snap.ue_loc);
            let name_mismatch = if !actor_name.is_empty() && !snap.name.is_empty() {
                if actor_name == snap.name.trim().to_ascii_lowercase() {
                    0.0
                } else {
                    1.0
                }
            } else {
                0.5
            };
            // Distance dominates; name/yaw tie-break duplicate stacked or nearby items.
            let yaw_pen = angle_diff_deg(actor.rot[1], snap.ue_yaw_deg) * 0.01;
            let cost = d2 + name_mismatch * 50.0 + yaw_pen;
            pairs.push((cost, d2, ai, si));
        }
    }
    pairs.sort_by(|a, b| a.0.total_cmp(&b.0));

    let mut used_actor = vec![false; actors.len()];
    let mut used_snap = vec![false; snap_items.len()];
    for (_cost, d2, ai, si) in pairs {
        if used_actor[ai] || used_snap[si] {
            continue;
        }
        // Reject wildly unrelated matches (> 100cm apart when array lengths differ).
        if d2 > 100.0 * 100.0 {
            continue;
        }
        if snap_items[si].item_id > 0 {
            out[ai] = Some(snap_items[si].item_id);
        }
        used_actor[ai] = true;
        used_snap[si] = true;
    }

    out
}

/// Maintains sticky bidirectional mappings between UE5 `AFurniture` `actor_key`s and Foundation `(indoors, flat_slot)`s.
#[derive(Default, Debug)]
pub struct ActorSlotBinder {
    key_to_slot: HashMap<String, (bool, u16)>,
    slot_to_key: HashMap<(bool, u16), String>,
    key_to_item_id: HashMap<String, u32>,
    slot_to_item_id: HashMap<(bool, u16), u32>,
}

impl ActorSlotBinder {
    pub fn len(&self) -> usize {
        self.key_to_slot.len()
    }

    pub fn is_empty(&self) -> bool {
        self.key_to_slot.is_empty()
    }

    pub fn resolve_slot(&self, actor_key: &str) -> Option<(bool, u16, Option<u32>)> {
        let &(indoors, flat_slot) = self.key_to_slot.get(actor_key)?;
        let item_id = self
            .key_to_item_id
            .get(actor_key)
            .copied()
            .or_else(|| self.slot_to_item_id.get(&(indoors, flat_slot)).copied());
        Some((indoors, flat_slot, item_id))
    }

    pub fn resolve_actor_key(&self, indoors: bool, flat_slot: u16) -> Option<&str> {
        self.slot_to_key
            .get(&(indoors, flat_slot))
            .map(String::as_str)
    }

    pub fn clear_scope(&mut self, indoors: bool) {
        let keys_to_remove: Vec<String> = self
            .key_to_slot
            .iter()
            .filter(|&(_, (ind, _))| *ind == indoors)
            .map(|(k, _)| k.clone())
            .collect();
        for k in keys_to_remove {
            self.key_to_slot.remove(&k);
            self.key_to_item_id.remove(&k);
        }
        self.slot_to_key.retain(|(ind, _), _| *ind != indoors);
        self.slot_to_item_id.retain(|(ind, _), _| *ind != indoors);
    }

    /// Rebuilds or updates bindings for `indoors` scope.
    /// When `preserve_existing` is `true`, still-valid existing links for active actors with matching
    /// `item_id` are kept pinned so dragging duplicate furnishings never jumps slots mid-drag.
    pub fn rebind(
        &mut self,
        indoors: bool,
        actors: &[RmpActorPose],
        enriched_item_ids: &[Option<u32>],
        foundation_slots: &[FoundationSlotInfo],
        preserve_existing: bool,
    ) -> usize {
        if !preserve_existing {
            self.clear_scope(indoors);
        }

        let slot_by_id: HashMap<u16, &FoundationSlotInfo> =
            foundation_slots.iter().map(|s| (s.flat_slot, s)).collect();
        let actor_index_by_key: HashMap<&str, usize> = actors
            .iter()
            .enumerate()
            .map(|(i, a)| (a.actor_key.as_str(), i))
            .collect();

        let mut bound_actors = HashSet::new();
        let mut bound_slots = HashSet::new();

        // Keep or prune prior bindings for this scope.
        let prev_pairs: Vec<(String, u16)> = self
            .key_to_slot
            .iter()
            .filter(|&(_, &(ind, _))| ind == indoors)
            .map(|(k, &(_, slot))| (k.clone(), slot))
            .collect();

        for (key, slot) in prev_pairs {
            let keep = if preserve_existing
                && let Some(&ai) = actor_index_by_key.get(key.as_str())
                && let Some(fslot) = slot_by_id.get(&slot)
            {
                match enriched_item_ids.get(ai).copied().flatten() {
                    Some(id) if id > 0 => id == fslot.item_id,
                    _ => match self.key_to_item_id.get(&key).copied() {
                        Some(prev_id) if prev_id > 0 => prev_id == fslot.item_id,
                        _ => true,
                    },
                }
            } else {
                false
            };

            if keep && !bound_actors.contains(&key) && !bound_slots.contains(&slot) {
                let fslot = slot_by_id[&slot];
                bound_actors.insert(key.clone());
                bound_slots.insert(slot);
                self.key_to_item_id.insert(key, fslot.item_id);
                self.slot_to_item_id.insert((indoors, slot), fslot.item_id);
            } else {
                self.key_to_slot.remove(&key);
                self.key_to_item_id.remove(&key);
                self.slot_to_key.remove(&(indoors, slot));
                self.slot_to_item_id.remove(&(indoors, slot));
            }
        }

        // Greedy minimum-cost matching for remaining unbound actors & Foundation slots.
        let mut candidates: Vec<(u8, f32, usize, u16)> = Vec::new();
        for (ai, actor) in actors.iter().enumerate() {
            if bound_actors.contains(&actor.actor_key) {
                continue;
            }
            let rmp_item_id = enriched_item_ids
                .get(ai)
                .copied()
                .flatten()
                .filter(|&id| id > 0);
            let rmp_name = actor.item_name.trim().to_ascii_lowercase();

            for fslot in foundation_slots {
                if bound_slots.contains(&fslot.flat_slot) {
                    continue;
                }
                let d2 = ue_dist_sq(actor.loc, fslot.ue_loc);
                let yaw_pen = angle_diff_deg(actor.rot[1], fslot.ue_yaw_deg) * 0.05;
                let fname = fslot.name.trim().to_ascii_lowercase();

                let tier = if rmp_item_id == Some(fslot.item_id) {
                    0u8
                } else if rmp_item_id.is_none() && !rmp_name.is_empty() && rmp_name == fname {
                    1u8
                } else if rmp_item_id.is_none() && d2 <= 25.0 * 25.0 {
                    // Geometric fallback (essential when RMP displays English names and Foundation uses Korean/Japanese catalog names).
                    2u8
                } else {
                    continue;
                };

                candidates.push((tier, d2 + yaw_pen, ai, fslot.flat_slot));
            }
        }

        candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.total_cmp(&b.1)));

        for (_tier, _score, ai, flat_slot) in candidates {
            let key = &actors[ai].actor_key;
            if bound_actors.contains(key) || bound_slots.contains(&flat_slot) {
                continue;
            }
            let fslot = slot_by_id[&flat_slot];
            bound_actors.insert(key.clone());
            bound_slots.insert(flat_slot);
            self.key_to_slot.insert(key.clone(), (indoors, flat_slot));
            self.slot_to_key.insert((indoors, flat_slot), key.clone());
            self.key_to_item_id.insert(key.clone(), fslot.item_id);
            self.slot_to_item_id
                .insert((indoors, flat_slot), fslot.item_id);
        }

        bound_slots.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_actor(key: &str, name: &str, loc: [f32; 3], yaw: f32) -> RmpActorPose {
        RmpActorPose {
            actor_key: key.to_string(),
            item_name: name.to_string(),
            loc,
            rot: [0.0, yaw, 0.0],
        }
    }

    fn make_slot(
        flat_slot: u16,
        item_id: u32,
        name: &str,
        ue_loc: [f32; 3],
        ue_yaw_deg: f32,
    ) -> FoundationSlotInfo {
        FoundationSlotInfo {
            flat_slot,
            item_id,
            catalog_id: 1,
            name: name.to_string(),
            ue_loc,
            ue_yaw_deg,
        }
    }

    #[test]
    fn rebind_keeps_duplicate_items_sticky_when_dragged_past_sibling() {
        let mut binder = ActorSlotBinder::default();
        let initial_actors = vec![
            make_actor(
                "Partition_A",
                "White Rectangular Partition",
                [100.0, 0.0, 0.0],
                0.0,
            ),
            make_actor(
                "Partition_B",
                "White Rectangular Partition",
                [300.0, 0.0, 0.0],
                90.0,
            ),
        ];
        let slots = vec![
            make_slot(0, 28901, "하얀 직사각형 칸막이", [100.0, 0.0, 0.0], 0.0),
            make_slot(5, 28901, "하얀 직사각형 칸막이", [300.0, 0.0, 0.0], 90.0),
        ];

        // Even with Korean vs English names, geometric proximity fallback binds initially.
        let count = binder.rebind(true, &initial_actors, &[None, None], &slots, false);
        assert_eq!(count, 2);
        assert_eq!(
            binder.resolve_slot("Partition_A"),
            Some((true, 0, Some(28901)))
        );
        assert_eq!(
            binder.resolve_slot("Partition_B"),
            Some((true, 5, Some(28901)))
        );

        // Now Partition_A is dragged all the way next to Partition_B, while Foundation still reports old snapshot.
        let moved_actors = vec![
            make_actor(
                "Partition_A",
                "White Rectangular Partition",
                [295.0, 0.0, 0.0],
                0.0,
            ),
            make_actor(
                "Partition_B",
                "White Rectangular Partition",
                [300.0, 0.0, 0.0],
                90.0,
            ),
        ];
        binder.rebind(
            true,
            &moved_actors,
            &[Some(28901), Some(28901)],
            &slots,
            true,
        );
        assert_eq!(binder.resolve_slot("Partition_A").unwrap().1, 0);
        assert_eq!(binder.resolve_slot("Partition_B").unwrap().1, 5);
    }

    #[test]
    fn snapshot_enrichment_handles_attachments_and_scale_factor() {
        let snap = r#"{
            "houseSize": "Small",
            "interiorScale": 1,
            "interiorFurniture": [
                {
                    "name": "Table",
                    "itemId": 1001,
                    "transform": { "location": [1.5, 2.0, 0.0], "rotation": [0, 0, 0, 1], "scale": [1,1,1] },
                    "attachments": [
                        {
                            "name": "Lamp",
                            "itemId": 1002,
                            "transform": { "location": [1.5, 2.0, 1.0], "rotation": [0, 0, 0, 1], "scale": [1,1,1] },
                            "attachments": []
                        }
                    ]
                }
            ]
        }"#;

        let actors = vec![
            make_actor("Actor_Lamp", "Lamp", [150.0, 200.0, 100.0], 0.0),
            make_actor("Actor_Table", "Table", [150.0, 200.0, 0.0], 0.0),
        ];
        let enriched = enrich_actors_from_snapshot_json(snap, false, &actors);
        assert_eq!(enriched, vec![Some(1002), Some(1001)]);
    }
}
