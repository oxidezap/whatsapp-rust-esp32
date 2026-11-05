//! Bounded metadata for already validated, existing NVS session slots.

use std::cell::RefCell;
use std::collections::VecDeque;

#[derive(Default)]
pub(super) struct SessionSlots(RefCell<VecDeque<(String, Vec<u8>)>>);

impl SessionSlots {
    pub(super) fn matches(&self, name: &str, key: &[u8]) -> Option<bool> {
        self.0
            .borrow()
            .iter()
            .find(|(stored, _)| stored == name)
            .map(|(_, stored_key)| stored_key == key)
    }

    pub(super) fn remember(&self, name: &str, key: &[u8]) {
        let mut slots = self.0.borrow_mut();
        if let Some(index) = slots.iter().position(|(stored, _)| stored == name) {
            slots.remove(index);
        }
        if slots.len() == 8 {
            slots.pop_front();
        }
        slots.push_back((name.to_owned(), key.to_vec()));
    }

    pub(super) fn clear(&self) {
        self.0.borrow_mut().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::SessionSlots;

    #[test]
    fn a_colliding_name_never_validates_a_different_logical_key() {
        let slots = SessionSlots::default();
        assert_eq!(slots.matches("collision", b"first"), None);
        slots.remember("collision", b"first");
        assert_eq!(slots.matches("collision", b"first"), Some(true));
        assert_eq!(slots.matches("collision", b"second"), Some(false));
    }

    #[test]
    fn eviction_and_invalidation_require_flash_validation_again() {
        let slots = SessionSlots::default();
        for index in 0..8 {
            slots.remember(&index.to_string(), &[index]);
        }
        slots.remember("0", &[0]);
        slots.remember("8", &[8]);
        assert_eq!(slots.matches("0", &[0]), Some(true));
        assert_eq!(slots.matches("1", &[1]), None);
        assert_eq!(slots.0.borrow().len(), 8);
        slots.clear();
        assert_eq!(slots.matches("0", &[0]), None);
        slots.remember("0", b"replacement after erase");
        assert_eq!(slots.matches("0", &[0]), Some(false));
        assert_eq!(slots.matches("0", b"replacement after erase"), Some(true));
    }
}
