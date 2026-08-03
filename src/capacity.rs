use crate::models::Catalog;
use std::collections::HashMap;
use std::sync::Mutex;

pub struct CapacityManager {
    inner: Mutex<CapacityState>,
}

struct CapacityState {
    catalog_version: String,
    remaining: HashMap<String, u32>,
}

impl CapacityManager {
    pub fn new() -> Self {
        CapacityManager {
            inner: Mutex::new(CapacityState {
                catalog_version: String::new(),
                remaining: HashMap::new(),
            }),
        }
    }

    pub fn reset_for_catalog(&self, catalog: &Catalog) {
        let mut state = self.inner.lock().expect("capacity lock poisoned");
        if state.catalog_version == catalog.catalog_version {
            return;
        }
        state.catalog_version = catalog.catalog_version.clone();
        state.remaining.clear();
        for slot in &catalog.home_service.slots {
            state
                .remaining
                .insert(slot.slot_id.clone(), slot.capacity);
        }
    }

    pub fn try_assign(
        &self,
        catalog_version: &str,
        ordered_slot_ids: &[String],
    ) -> Result<String, String> {
        let mut state = self.inner.lock().expect("capacity lock poisoned");
        if state.catalog_version != catalog_version {
            return Err(format!(
                "capacity state belongs to a different catalog version: expected {}, current {}",
                state.catalog_version, catalog_version
            ));
        }

        for slot_id in ordered_slot_ids {
            if let Some(remaining) = state.remaining.get_mut(slot_id) {
                if *remaining > 0 {
                    *remaining -= 1;
                    return Ok(slot_id.clone());
                }
            }
        }

        if ordered_slot_ids.is_empty() {
            Err("home service is eligible but no appointment slots are configured".to_string())
        } else {
            Err(format!(
                "all home-service appointment slots are exhausted: {}",
                ordered_slot_ids.join(", ")
            ))
        }
    }

    #[cfg(test)]
    pub fn remaining(&self, catalog_version: &str, slot_id: &str) -> Option<u32> {
        let state = self.inner.lock().expect("capacity lock poisoned");
        if state.catalog_version != catalog_version {
            return None;
        }
        state.remaining.get(slot_id).copied()
    }
}

impl Default for CapacityManager {
    fn default() -> Self {
        Self::new()
    }
}
