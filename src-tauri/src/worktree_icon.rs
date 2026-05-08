//! Worktree-tab icon assignment (Issue #45).
//!
//! Each [`WorktreeTab`] carries a small unsigned integer `icon_id` (1..=[`WORKTREE_ICON_COUNT`]) that selects one of the bundled tree-icon PNGs in
//! `src/assets/tree-icons/`. The frontend resolves the integer to an asset URL — the backend's only job is to **pick** the icon when a tab is first
//! created and persist the choice on the record.
//!
//! ## Algorithm
//!
//! [`pick_least_used_icon`] is a pure helper: it counts how many of the existing tabs already use each icon id 1..=N and returns the *first*
//! (lowest-numbered) icon at the *minimum* count. This deterministic tiebreak means:
//!
//! * The first 16 distinct worktree tabs in a fresh workspace get icons 1, 2, 3, …, 16 in order.
//! * The 17th tab reuses icon 1, the 18th reuses icon 2, and so on — keeping the duplicate distribution as flat as possible.
//! * Closing a tab that owns icon 7 frees icon 7 to be the *next* pick (since it now has the lowest count among ties).
//!
//! Determinism matters: it makes the v6→v7 migration reproducible, lets tests assert on specific icon ids, and keeps the choice stable across
//! cosmetic refactors.
//!
//! [`WorktreeTab`]: crate::types::WorktreeTab

/// Number of tree icons bundled under `src/assets/tree-icons/`. Icon ids are 1-based (`1..=WORKTREE_ICON_COUNT`); `0` is reserved for the serde
/// default that signals "not yet assigned" during the v6→v7 migration.
pub const WORKTREE_ICON_COUNT: u32 = 16;

/// Pick the least-used icon id from `existing_icon_ids`, breaking ties by the **lowest** icon number. Returns a value in `1..=WORKTREE_ICON_COUNT`.
///
/// Icon ids in `existing_icon_ids` outside the valid range are ignored — they don't poison the count for any real icon. Callers should still treat
/// this as a hint that something earlier failed to enforce the range invariant.
#[must_use]
pub fn pick_least_used_icon(existing_icon_ids: &[u32]) -> u32 {
    let mut counts = [0_usize; WORKTREE_ICON_COUNT as usize];
    for &id in existing_icon_ids {
        if (1..=WORKTREE_ICON_COUNT).contains(&id) {
            counts[(id - 1) as usize] += 1;
        }
    }
    // `position` returns the index of the first element matching the predicate, which is the lowest-numbered icon at the minimum count — exactly the
    // tiebreak we want. The slice is non-empty (WORKTREE_ICON_COUNT > 0), so `min` always returns Some.
    let min_count = *counts.iter().min().expect("counts is non-empty");
    let idx = counts.iter().position(|&c| c == min_count).expect("min_count came from counts");
    (idx as u32) + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_first_icon() {
        assert_eq!(pick_least_used_icon(&[]), 1);
    }

    #[test]
    fn single_icon_used_skips_to_next_unused() {
        assert_eq!(pick_least_used_icon(&[1]), 2);
        assert_eq!(pick_least_used_icon(&[1, 2]), 3);
    }

    #[test]
    fn first_n_distinct_picks_walk_1_through_n() {
        let mut chosen: Vec<u32> = Vec::new();
        for _ in 0..WORKTREE_ICON_COUNT {
            chosen.push(pick_least_used_icon(&chosen));
        }
        let expected: Vec<u32> = (1..=WORKTREE_ICON_COUNT).collect();
        assert_eq!(chosen, expected, "first N picks must walk 1..=N when starting from empty");
    }

    #[test]
    fn after_exhausting_set_wraps_back_to_lowest_number() {
        let exhausted: Vec<u32> = (1..=WORKTREE_ICON_COUNT).collect();
        // Every icon used once — lowest-numbered (1) wins the tiebreak for the next pick.
        assert_eq!(pick_least_used_icon(&exhausted), 1);
        // After picking 1, count[0] = 2 and the rest are 1 — next pick is 2.
        let mut after_one_more = exhausted.clone();
        after_one_more.push(1);
        assert_eq!(pick_least_used_icon(&after_one_more), 2);
    }

    #[test]
    fn out_of_range_ids_are_ignored() {
        // Garbage values (0, > WORKTREE_ICON_COUNT) must not contribute to any real icon's count and must not panic on indexing.
        let with_garbage = [0_u32, WORKTREE_ICON_COUNT + 5, u32::MAX];
        assert_eq!(pick_least_used_icon(&with_garbage), 1, "valid icons all have count 0, so icon 1 wins");
    }

    #[test]
    fn picks_lowest_among_ties() {
        // Icon 1 used twice, every other icon used once → min count is 1 with icons 2..=16 tied; lowest-numbered (2) must win the tiebreak.
        let mut existing: Vec<u32> = (1..=WORKTREE_ICON_COUNT).collect();
        existing.push(1);
        assert_eq!(pick_least_used_icon(&existing), 2);
    }

    #[test]
    fn freed_icon_becomes_immediate_next_pick() {
        // Simulate: every icon used once, then icon 7 is "freed" by closing its tab — it now sits at count 0 while everything else is at 1, so it
        // is the next pick. Mirrors the user-visible expectation of "closing a tab releases its icon" (the documented behaviour in the helper).
        let mut existing: Vec<u32> = (1..=WORKTREE_ICON_COUNT).collect();
        existing.retain(|&id| id != 7);
        assert_eq!(pick_least_used_icon(&existing), 7);
    }
}
