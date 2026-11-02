//! The pure half of the publisher: given the last published view and the
//! current one, which changes describe the difference?

use std::collections::HashMap;

use super::model::{SessionEntry, StatusChange};

/// Changes that turn `old` into `new`, in the order a client must apply them:
/// removals first, then each surviving or new session in `new` order.
///
/// `last_activity_ms` alone never produces a change. It rides along on the
/// next `StatusChanged`, and is fresh in every snapshot. Without that rule
/// every hook event would be a delta and the feed would be a firehose.
pub(crate) fn diff(old: &[SessionEntry], new: &[SessionEntry]) -> Vec<StatusChange> {
    let new_ids: HashMap<&str, ()> = new.iter().map(|e| (e.id.as_str(), ())).collect();
    let old_by_id: HashMap<&str, &SessionEntry> = old.iter().map(|e| (e.id.as_str(), e)).collect();

    let mut changes: Vec<StatusChange> = old
        .iter()
        .filter(|e| !new_ids.contains_key(e.id.as_str()))
        .map(|e| StatusChange::Removed { id: e.id.clone() })
        .collect();

    for entry in new {
        let Some(prev) = old_by_id.get(entry.id.as_str()) else {
            changes.push(StatusChange::Added {
                session: entry.clone(),
            });
            continue;
        };
        if prev.project_id != entry.project_id
            || prev.project_name != entry.project_name
            || prev.group != entry.group
        {
            // Structural move: replace the whole entry.
            changes.push(StatusChange::Added {
                session: entry.clone(),
            });
            continue;
        }
        if prev.label != entry.label {
            changes.push(StatusChange::Renamed {
                id: entry.id.clone(),
                label: entry.label.clone(),
            });
        }
        if prev.status != entry.status || prev.attention != entry.attention {
            changes.push(StatusChange::StatusChanged {
                id: entry.id.clone(),
                status: entry.status,
                attention: entry.attention,
                last_activity_ms: entry.last_activity_ms,
            });
        }
    }
    changes
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::status_feed::model::{FeedAttention, FeedStatus};

    pub(crate) fn entry(id: &str, status: FeedStatus) -> SessionEntry {
        SessionEntry {
            id: id.into(),
            label: format!("label {id}"),
            project_id: "p1".into(),
            project_name: "proj".into(),
            group: None,
            status,
            attention: None,
            last_activity_ms: 1,
        }
    }

    #[test]
    fn identical_views_produce_nothing() {
        let v = vec![
            entry("a", FeedStatus::Idle),
            entry("b", FeedStatus::Running),
        ];
        assert!(diff(&v, &v).is_empty());
    }

    #[test]
    fn last_activity_alone_is_not_a_change() {
        let old = vec![entry("a", FeedStatus::Running)];
        let mut new = old.clone();
        new[0].last_activity_ms = 999;
        assert!(diff(&old, &new).is_empty());
    }

    #[test]
    fn added_removed_renamed_and_status_are_each_reported() {
        let old = vec![
            entry("gone", FeedStatus::Idle),
            entry("kept", FeedStatus::Running),
        ];
        let mut kept = entry("kept", FeedStatus::AwaitingInput);
        kept.label = "new name".into();
        kept.attention = Some(FeedAttention::Question);
        kept.last_activity_ms = 5;
        let new = vec![kept, entry("fresh", FeedStatus::Idle)];

        assert_eq!(
            diff(&old, &new),
            vec![
                StatusChange::Removed { id: "gone".into() },
                StatusChange::Renamed {
                    id: "kept".into(),
                    label: "new name".into()
                },
                StatusChange::StatusChanged {
                    id: "kept".into(),
                    status: FeedStatus::AwaitingInput,
                    attention: Some(FeedAttention::Question),
                    last_activity_ms: 5,
                },
                StatusChange::Added {
                    session: entry("fresh", FeedStatus::Idle)
                },
            ]
        );
    }

    #[test]
    fn attention_kind_change_alone_is_a_status_change() {
        let mut old = entry("a", FeedStatus::AwaitingInput);
        old.attention = Some(FeedAttention::Permission);
        let mut new = old.clone();
        new.attention = Some(FeedAttention::Question);
        assert!(matches!(
            diff(&[old], &[new]).as_slice(),
            [StatusChange::StatusChanged {
                attention: Some(FeedAttention::Question),
                ..
            }]
        ));
    }

    #[test]
    fn a_project_rename_replaces_the_entry() {
        let old = entry("a", FeedStatus::Idle);
        let mut new = old.clone();
        new.project_name = "renamed".into();
        assert_eq!(
            diff(&[old], std::slice::from_ref(&new)),
            vec![StatusChange::Added { session: new }]
        );
    }
}
