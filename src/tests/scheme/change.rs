use std::sync::Arc;

use rand::{RngExt, SeedableRng, rngs::StdRng};

use crate::scheme::change::{AddressChange, ChangeSet, FlowUnit, Revision, Splice};

impl FlowUnit for u8 {
    fn extent(&self) -> usize {
        1
    }
}

#[test]
fn sparse_transaction_validates_base_and_final_coordinates() {
    let changes = ChangeSet {
        revision: Revision { base: 4, target: 5 },
        changes: vec![AddressChange {
            address: "file:///test.json",
            old_extent: 3,
            new_extent: 4,
            splices: vec![Splice {
                old_range: 1..2,
                new_range: 1..3,
                removed: Arc::from([()]),
                inserted: Arc::from([(), ()]),
            }],
        }],
    };

    assert!(changes.validate().is_ok());
}

#[test]
fn empty_transaction_advances_revision() {
    let changes = ChangeSet::<(), ()>::empty(Revision { base: 8, target: 9 });
    assert!(changes.changes.is_empty());
    assert!(changes.validate().is_ok());
}

#[test]
fn transaction_rejects_overlap_and_mismatched_payload_extent() {
    let overlap = ChangeSet {
        revision: Revision { base: 1, target: 2 },
        changes: vec![AddressChange {
            address: (),
            old_extent: 3,
            new_extent: 3,
            splices: vec![
                Splice {
                    old_range: 0..1,
                    new_range: 0..1,
                    removed: Arc::from([()]),
                    inserted: Arc::from([()]),
                },
                Splice {
                    old_range: 0..1,
                    new_range: 0..1,
                    removed: Arc::from([()]),
                    inserted: Arc::from([()]),
                },
            ],
        }],
    };
    assert!(overlap.validate().is_err());

    let extent = ChangeSet {
        revision: Revision { base: 2, target: 3 },
        changes: vec![AddressChange {
            address: (),
            old_extent: 2,
            new_extent: 1,
            splices: vec![Splice {
                old_range: 0..2,
                new_range: 0..1,
                removed: Arc::from([()]),
                inserted: Arc::from([()]),
            }],
        }],
    };
    assert!(extent.validate().is_err());
}

#[test]
fn sparse_splices_reconstruct_random_vectors() {
    let mut rng = StdRng::seed_from_u64(0x51_1ce);
    for revision in 1..=1_000 {
        let old = (0..rng.random_range(0..80))
            .map(|_| rng.random())
            .collect::<Vec<u8>>();
        let mut expected = Vec::new();
        let mut splices = Vec::new();
        let mut old_cursor = 0;
        let mut new_cursor = 0;
        for _ in 0..32 {
            if old_cursor == old.len() || !rng.random_bool(0.7) {
                break;
            }
            let gap = rng.random_range(0..=old.len() - old_cursor);
            expected.extend_from_slice(&old[old_cursor..old_cursor + gap]);
            old_cursor += gap;
            new_cursor += gap;
            if old_cursor == old.len() && !rng.random_bool(0.5) {
                break;
            }
            let removed_len = rng.random_range(0..=usize::min(5, old.len() - old_cursor));
            let inserted = (0..rng.random_range(0..=5))
                .map(|_| rng.random())
                .collect::<Vec<u8>>();
            if removed_len == 0 && inserted.is_empty() {
                continue;
            }
            let old_end = old_cursor + removed_len;
            let new_end = new_cursor + inserted.len();
            splices.push(Splice {
                old_range: old_cursor..old_end,
                new_range: new_cursor..new_end,
                removed: Arc::from(&old[old_cursor..old_end]),
                inserted: Arc::from(inserted.as_slice()),
            });
            expected.extend_from_slice(&inserted);
            old_cursor = old_end;
            new_cursor = new_end;
        }
        expected.extend_from_slice(&old[old_cursor..]);
        if splices.is_empty() {
            continue;
        }
        let change = ChangeSet {
            revision: Revision {
                base: revision,
                target: revision + 1,
            },
            changes: vec![AddressChange {
                address: (),
                old_extent: old.len(),
                new_extent: expected.len(),
                splices,
            }],
        };
        change.validate().unwrap();

        let mut actual = old.clone();
        for splice in change.changes[0].splices.iter().rev() {
            actual.splice(splice.old_range.clone(), splice.inserted.iter().copied());
        }
        assert_eq!(actual, expected);
    }
}

#[test]
fn transaction_rejects_duplicate_addresses_and_non_monotonic_revisions() {
    let change = AddressChange {
        address: 7,
        old_extent: 1,
        new_extent: 1,
        splices: vec![Splice {
            old_range: 0..1,
            new_range: 0..1,
            removed: Arc::from([()]),
            inserted: Arc::from([()]),
        }],
    };
    assert!(
        ChangeSet {
            revision: Revision { base: 2, target: 3 },
            changes: vec![change.clone(), change],
        }
        .validate()
        .is_err()
    );
    assert!(
        ChangeSet::<(), ()>::empty(Revision { base: 3, target: 2 })
            .validate()
            .is_err()
    );

    for range in [2..1, 1..1] {
        assert!(
            ChangeSet {
                revision: Revision { base: 3, target: 4 },
                changes: vec![AddressChange {
                    address: (),
                    old_extent: 2,
                    new_extent: 2,
                    splices: vec![Splice::<()> {
                        old_range: range.clone(),
                        new_range: range,
                        removed: Arc::from([]),
                        inserted: Arc::from([]),
                    }],
                }],
            }
            .validate()
            .is_err()
        );
    }
}
