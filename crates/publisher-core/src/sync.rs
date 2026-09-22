use crate::state::{PublisherState, SourcePhoto};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncAction {
    Add(SourcePhoto),
    Update(SourcePhoto),
    Remove { relative_path: String },
}

pub fn plan_sync(previous: &PublisherState, current: &[SourcePhoto]) -> Vec<SyncAction> {
    let current_map: BTreeMap<_, _> = current
        .iter()
        .map(|p| (p.relative_path.clone(), p))
        .collect();
    let mut plan = Vec::new();

    for (path, photo) in &current_map {
        match previous.photos.get(path) {
            None => plan.push(SyncAction::Add((*photo).clone())),
            Some(old) if old.sha256 != photo.sha256 => {
                plan.push(SyncAction::Update((*photo).clone()))
            }
            _ => {}
        }
    }

    for path in previous.photos.keys() {
        if !current_map.contains_key(path) {
            plan.push(SyncAction::Remove {
                relative_path: path.clone(),
            });
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    fn make_photo(path: &str, sha256: &str) -> SourcePhoto {
        SourcePhoto {
            relative_path: path.into(),
            bytes: 100,
            sha256: sha256.into(),
        }
    }

    #[test]
    fn empty_to_empty_produces_no_actions() {
        let prev = PublisherState::default();
        let actions = plan_sync(&prev, &[]);
        assert!(actions.is_empty());
    }

    #[test]
    fn empty_to_photos_produces_adds() {
        let prev = PublisherState::default();
        let current = [
            make_photo("a.jpg", "hash1"),
            make_photo("b.jpg", "hash2"),
            make_photo("c.jpg", "hash3"),
        ];
        let actions = plan_sync(&prev, &current);
        assert_eq!(actions.len(), 3);
        assert!(matches!(actions[0], SyncAction::Add(_)));
        assert!(matches!(actions[1], SyncAction::Add(_)));
        assert!(matches!(actions[2], SyncAction::Add(_)));
    }

    #[test]
    fn photos_to_empty_produces_removes() {
        let mut prev = PublisherState::default();
        prev.photos
            .insert("a.jpg".into(), make_photo("a.jpg", "hash1"));
        prev.photos
            .insert("b.jpg".into(), make_photo("b.jpg", "hash2"));
        prev.photos
            .insert("c.jpg".into(), make_photo("c.jpg", "hash3"));

        let actions = plan_sync(&prev, &[]);
        assert_eq!(actions.len(), 3);
        assert!(actions
            .iter()
            .all(|a| matches!(a, SyncAction::Remove { .. })));
    }

    #[test]
    fn unchanged_photos_produce_no_actions() {
        let mut prev = PublisherState::default();
        prev.photos
            .insert("a.jpg".into(), make_photo("a.jpg", "hash1"));
        let current = [make_photo("a.jpg", "hash1")];

        let actions = plan_sync(&prev, &current);
        assert!(actions.is_empty());
    }

    #[test]
    fn changed_hash_produces_update() {
        let mut prev = PublisherState::default();
        prev.photos
            .insert("a.jpg".into(), make_photo("a.jpg", "hash1"));
        let current = [make_photo("a.jpg", "hash2")]; // changed hash

        let actions = plan_sync(&prev, &current);
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], SyncAction::Update(_)));
    }

    #[test]
    fn mixed_add_update_remove() {
        let mut prev = PublisherState::default();
        prev.photos
            .insert("removed.jpg".into(), make_photo("removed.jpg", "hash1"));
        prev.photos
            .insert("updated.jpg".into(), make_photo("updated.jpg", "hash2"));
        prev.photos
            .insert("kept.jpg".into(), make_photo("kept.jpg", "hash3"));

        let current = [
            make_photo("added.jpg", "hash4"),
            make_photo("updated.jpg", "hash5"), // changed hash
            make_photo("kept.jpg", "hash3"),    // same hash
        ];

        let actions = plan_sync(&prev, &current);
        assert_eq!(actions.len(), 3);

        // Output order of BTreeMap keys: "added.jpg", "updated.jpg" from current loop,
        // then "removed.jpg" from prev loop
        assert!(matches!(&actions[0], SyncAction::Add(ref p) if p.relative_path == "added.jpg"));
        assert!(
            matches!(&actions[1], SyncAction::Update(ref p) if p.relative_path == "updated.jpg")
        );
        assert!(
            matches!(&actions[2], SyncAction::Remove { ref relative_path } if relative_path == "removed.jpg")
        );
    }

    #[test]
    fn sync_plan_is_deterministic() {
        let mut prev = PublisherState::default();
        prev.photos
            .insert("a.jpg".into(), make_photo("a.jpg", "h1"));

        let current = [make_photo("a.jpg", "h2"), make_photo("b.jpg", "h3")];

        let actions1 = plan_sync(&prev, &current);
        let actions2 = plan_sync(&prev, &current);
        assert_eq!(actions1, actions2);
    }

    #[test]
    fn renamed_file_produces_remove_and_add() {
        let mut prev = PublisherState::default();
        prev.photos
            .insert("old.jpg".into(), make_photo("old.jpg", "hash"));

        let current = [make_photo("new.jpg", "hash")];

        let actions = plan_sync(&prev, &current);
        assert_eq!(actions.len(), 2);
        assert!(matches!(&actions[0], SyncAction::Add(ref p) if p.relative_path == "new.jpg"));
        assert!(
            matches!(&actions[1], SyncAction::Remove { ref relative_path } if relative_path == "old.jpg")
        );
    }
}
