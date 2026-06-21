use crate::scheme::change::{LayerChange, ReplacementBatch, ReplacementChange};

#[test]
fn replacement_batch_reports_unchanged_when_ranges_are_empty() {
    let batch = ReplacementBatch {
        old_units: vec![1, 2, 3],
        new_units: vec![1, 2, 3],
        prefix_len: 3,
        suffix_len: 0,
        old_changed_range: 3..3,
        new_changed_range: 3..3,
    };

    assert!(!batch.is_changed());
}

#[test]
fn replacement_batch_reports_changed_for_insert_delete_and_replace() {
    let insert = ReplacementBatch {
        old_units: vec![1, 3],
        new_units: vec![1, 2, 3],
        prefix_len: 1,
        suffix_len: 1,
        old_changed_range: 1..1,
        new_changed_range: 1..2,
    };
    let delete = ReplacementBatch {
        old_units: vec![1, 2, 3],
        new_units: vec![1, 3],
        prefix_len: 1,
        suffix_len: 1,
        old_changed_range: 1..2,
        new_changed_range: 1..1,
    };
    let replace = ReplacementBatch {
        old_units: vec![1, 2, 3],
        new_units: vec![1, 4, 3],
        prefix_len: 1,
        suffix_len: 1,
        old_changed_range: 1..2,
        new_changed_range: 1..2,
    };

    assert!(insert.is_changed());
    assert!(delete.is_changed());
    assert!(replace.is_changed());
}

#[test]
fn replacement_change_uses_address_and_batch_change_state() {
    let change = ReplacementChange {
        address: "file:///test.json",
        batch: ReplacementBatch {
            old_units: vec!['a'],
            new_units: vec!['b'],
            prefix_len: 0,
            suffix_len: 0,
            old_changed_range: 0..1,
            new_changed_range: 0..1,
        },
    };

    assert_eq!(change.address(), &"file:///test.json");
    assert_eq!(change.batch().new_units, vec!['b']);
    assert!(change.is_changed());
}
