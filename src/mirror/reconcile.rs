//! The three-way decision per key, straight from the client-algorithm table
//! in `docs/sync-protocol.md`. Pure: digests in, an `Action` out. Everything
//! that touches the network or the store lives in `sync.rs`, so this table
//! can be tested row by row without either.
//!
//! `L` = what the local bytes are, `B` = the last version agreed with the
//! hub (base ledger), `R` = what the hub has now. `None` = absent.
//!
//! | L vs B | R vs B      | do                                              |
//! |--------|-------------|-------------------------------------------------|
//! | =      | =           | nothing                                         |
//! | ≠      | =           | push (`If-Match: B`; `If-None-Match: *` when B absent); L absent → delete `If-Match: B` |
//! | =      | ≠           | pull (R absent → local copy to trash)           |
//! | ≠      | ≠, L = R    | adopt: base := L                                |
//! | ≠      | ≠, L ≠ R    | conflict: keep L, upload as conflict, pull R    |
//! | absent | ≠           | pull (edit beats delete)                        |
//! | ≠      | absent      | push as new                                     |

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Nothing,
    /// `PUT` the local bytes. `if_match` is B; None means the key must be
    /// free (`If-None-Match: *`).
    Push {
        if_match: Option<String>,
    },
    /// `DELETE` with `If-Match: B`.
    DeleteRemote {
        if_match: String,
    },
    /// Fetch R into place; base := R.
    Pull {
        sha256: String,
    },
    /// The hub deleted it and we have no local change: local copy to trash.
    TrashLocal,
    /// Both sides ended up with the same bytes: base := L, no traffic.
    Adopt,
    /// Both changed, differently: upload L as a conflict, then pull R.
    Conflict {
        remote: String,
    },
}

pub fn decide(local: Option<&str>, base: Option<&str>, remote: Option<&str>) -> Action {
    let local_changed = local != base;
    let remote_changed = remote != base;
    match (local_changed, remote_changed) {
        (false, false) => Action::Nothing,
        (true, false) => match local {
            Some(_) => Action::Push {
                if_match: base.map(str::to_string),
            },
            None => match base {
                Some(b) => Action::DeleteRemote {
                    if_match: b.to_string(),
                },
                // L absent, B absent, R = B absent: nothing exists anywhere.
                None => Action::Nothing,
            },
        },
        (false, true) => match remote {
            Some(r) => Action::Pull {
                sha256: r.to_string(),
            },
            None => match local {
                Some(_) => Action::TrashLocal,
                None => Action::Nothing,
            },
        },
        (true, true) => {
            if local == remote {
                Action::Adopt
            } else {
                match (local, remote) {
                    // Hub changed it, we deleted it: edit beats delete → pull.
                    (None, Some(r)) => Action::Pull {
                        sha256: r.to_string(),
                    },
                    // Hub deleted it, we changed it: edit beats delete → push
                    // as new (the base is gone, so no precondition to carry).
                    (Some(_), None) => Action::Push { if_match: None },
                    (Some(_), Some(r)) => Action::Conflict {
                        remote: r.to_string(),
                    },
                    // (None, None) is `local == remote` above.
                    (None, None) => Action::Adopt,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Option<&str> = Some("a");
    const B: Option<&str> = Some("b");
    const C: Option<&str> = Some("c");
    const X: Option<&str> = None;

    #[test]
    fn row_nothing_when_all_agree() {
        assert_eq!(decide(A, A, A), Action::Nothing);
        assert_eq!(decide(X, X, X), Action::Nothing);
    }

    #[test]
    fn row_push_when_only_local_changed() {
        assert_eq!(
            decide(B, A, A),
            Action::Push {
                if_match: Some("a".into())
            }
        );
        // New local file, never on the hub: create-only.
        assert_eq!(decide(A, X, X), Action::Push { if_match: None });
        // Local delete, hub untouched: delete with the base as precondition.
        assert_eq!(
            decide(X, A, A),
            Action::DeleteRemote {
                if_match: "a".into()
            }
        );
    }

    #[test]
    fn row_pull_when_only_remote_changed() {
        assert_eq!(decide(A, A, B), Action::Pull { sha256: "b".into() });
        // Brand new on the hub.
        assert_eq!(decide(X, X, A), Action::Pull { sha256: "a".into() });
        // Hub deleted, we had not touched it: trash the local copy.
        assert_eq!(decide(A, A, X), Action::TrashLocal);
    }

    #[test]
    fn row_adopt_when_both_reached_the_same_bytes() {
        assert_eq!(decide(B, A, B), Action::Adopt);
        // Both deleted.
        assert_eq!(decide(X, A, X), Action::Adopt);
        // Same new file on both sides (e.g. restored from the same source).
        assert_eq!(decide(A, X, A), Action::Adopt);
    }

    #[test]
    fn row_conflict_when_both_changed_differently() {
        assert_eq!(decide(B, A, C), Action::Conflict { remote: "c".into() });
        // No base at all (two devices created the same key independently).
        assert_eq!(decide(A, X, B), Action::Conflict { remote: "b".into() });
    }

    #[test]
    fn row_edit_beats_delete_both_ways() {
        // We deleted, hub edited: pull the hub's edit back.
        assert_eq!(decide(X, A, B), Action::Pull { sha256: "b".into() });
        // Hub deleted, we edited: push as new.
        assert_eq!(decide(B, A, X), Action::Push { if_match: None });
    }
}
