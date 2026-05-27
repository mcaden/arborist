//! Per-session trust store: remembers "yes, this tool+args fingerprint was confirmed" for the
//! lifetime of an Arborist session (or until TTL). Used to grant a `prompt`-mode tool call
//! a free pass when the user has already approved the exact same invocation recently.
//!
//! Stored as `TrustRecordInternal` (with `Instant`s) so sweeps are O(1) per entry; converted to
//! the canonical `arborist_types::mcp::McpTrustRecord` (with RFC3339 strings) only when
//! crossing the IPC boundary via `list_for_session_wire`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use time::OffsetDateTime;
use uuid::Uuid;

use crate::mcp::types::{McpToolName, McpTrustRecord, TrustRecordInternal};

type InstantClock = Arc<dyn Fn() -> Instant + Send + Sync>;
type WallClock = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

pub struct TrustedRequestStore {
    inner: Mutex<HashMap<String, HashMap<[u8; 32], TrustRecordInternal>>>,
    clock: InstantClock,
    wall_clock: WallClock,
    default_ttl: Duration,
}

impl TrustedRequestStore {
    #[must_use]
    pub fn new(default_ttl: Duration) -> Self {
        Self::with_clocks(default_ttl, Arc::new(Instant::now), Arc::new(OffsetDateTime::now_utc))
    }

    fn with_clocks(default_ttl: Duration, clock: InstantClock, wall_clock: WallClock) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            clock,
            wall_clock,
            default_ttl,
        }
    }

    pub fn record(
        &self,
        session_id: impl Into<String>,
        tool: McpToolName,
        fingerprint: [u8; 32],
        summary: impl Into<String>,
        ttl: Option<Duration>,
    ) -> TrustRecordInternal {
        let session_id = session_id.into();
        let summary = summary.into();
        let now = (self.clock)();
        let now_wall = (self.wall_clock)();
        let ttl = ttl.unwrap_or(self.default_ttl);
        let expires_at = now + ttl;
        let expires_at_wall = now_wall + ttl;
        let record = TrustRecordInternal {
            id: Uuid::new_v4().as_simple().to_string(),
            session_id: session_id.clone(),
            tool,
            fingerprint,
            summary,
            created_at: now,
            created_at_wall: now_wall,
            expires_at,
            expires_at_wall,
        };

        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        sweep_expired(&mut guard, now);
        guard.entry(session_id).or_default().insert(fingerprint, record.clone());
        record
    }

    #[must_use]
    pub fn check(&self, session_id: &str, tool: McpToolName, fingerprint: &[u8; 32]) -> Option<TrustRecordInternal> {
        let now = (self.clock)();
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        sweep_expired(&mut guard, now);

        let record = guard.get(session_id)?.get(fingerprint)?.clone();
        if record.tool == tool {
            Some(record)
        } else {
            None
        }
    }

    #[must_use]
    pub fn list_for_session(&self, session_id: &str) -> Vec<TrustRecordInternal> {
        let now = (self.clock)();
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        sweep_expired(&mut guard, now);

        let mut records: Vec<_> = guard.get(session_id).into_iter().flat_map(|records| records.values().cloned()).collect();
        records.sort_by_key(|record| record.created_at);
        records
    }

    /// Wire-form view of the per-session trust list — used by the `mcp_trust_list` command and
    /// the activity panel. Drops any record whose RFC3339 conversion fails (should be
    /// unreachable for `OffsetDateTime::now_utc`-derived values).
    #[must_use]
    pub fn list_for_session_wire(&self, session_id: &str) -> Vec<McpTrustRecord> {
        self.list_for_session(session_id)
            .iter()
            .filter_map(TrustRecordInternal::to_wire)
            .collect()
    }

    pub fn revoke(&self, session_id: &str, id: &str) -> bool {
        let now = (self.clock)();
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        sweep_expired(&mut guard, now);

        let Some(records) = guard.get_mut(session_id) else {
            return false;
        };
        let Some(fingerprint) = records.iter().find_map(|(fingerprint, record)| (record.id == id).then_some(*fingerprint)) else {
            return false;
        };
        records.remove(&fingerprint).is_some()
    }

    pub fn clear_session(&self, session_id: &str) {
        let now = (self.clock)();
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        sweep_expired(&mut guard, now);
        guard.remove(session_id);
    }
}

fn sweep_expired(records_by_session: &mut HashMap<String, HashMap<[u8; 32], TrustRecordInternal>>, now: Instant) {
    records_by_session.retain(|_, records| {
        records.retain(|_, record| record.expires_at > now);
        !records.is_empty()
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct ManualClock {
        now: Arc<Mutex<Instant>>,
        wall: Arc<Mutex<OffsetDateTime>>,
    }

    impl ManualClock {
        fn new() -> Self {
            Self {
                now: Arc::new(Mutex::new(Instant::now())),
                wall: Arc::new(Mutex::new(OffsetDateTime::now_utc())),
            }
        }

        fn instant_reader(&self) -> InstantClock {
            let now = Arc::clone(&self.now);
            Arc::new(move || match now.lock() {
                Ok(guard) => *guard,
                Err(poisoned) => *poisoned.into_inner(),
            })
        }

        fn wall_reader(&self) -> WallClock {
            let wall = Arc::clone(&self.wall);
            Arc::new(move || match wall.lock() {
                Ok(guard) => *guard,
                Err(poisoned) => *poisoned.into_inner(),
            })
        }

        fn advance(&self, delta: Duration) {
            {
                let mut guard = match self.now.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                *guard += delta;
            }
            {
                let mut guard = match self.wall.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                *guard += delta;
            }
        }
    }

    fn store_with(clock: &ManualClock, ttl: Duration) -> TrustedRequestStore {
        TrustedRequestStore::with_clocks(ttl, clock.instant_reader(), clock.wall_reader())
    }

    #[test]
    fn record_and_check_returns_it_within_ttl() {
        let clock = ManualClock::new();
        let store = store_with(&clock, Duration::from_secs(60));
        let fingerprint = [7_u8; 32];
        let record = store.record("session-1", McpToolName::CreateWorktree, fingerprint, "create wt", None);

        let found = store
            .check("session-1", McpToolName::CreateWorktree, &fingerprint)
            .expect("record should still be trusted");
        assert_eq!(found.id, record.id);
    }

    #[test]
    fn expired_record_is_removed() {
        let clock = ManualClock::new();
        let store = store_with(&clock, Duration::from_secs(10));
        let fingerprint = [1_u8; 32];
        let _ = store.record("session-1", McpToolName::CreateWorktree, fingerprint, "create wt", None);

        clock.advance(Duration::from_secs(11));

        assert!(store.check("session-1", McpToolName::CreateWorktree, &fingerprint).is_none());
        assert!(store.list_for_session("session-1").is_empty());
    }

    #[test]
    fn revoke_removes_record() {
        let clock = ManualClock::new();
        let store = store_with(&clock, Duration::from_secs(60));
        let fingerprint = [3_u8; 32];
        let record = store.record("session-1", McpToolName::CreateWorktree, fingerprint, "create wt", None);

        assert!(store.revoke("session-1", &record.id));
        assert!(store.check("session-1", McpToolName::CreateWorktree, &fingerprint).is_none());
    }

    #[test]
    fn fingerprint_mismatch_returns_none() {
        let clock = ManualClock::new();
        let store = store_with(&clock, Duration::from_secs(60));
        let _ = store.record("session-1", McpToolName::CreateWorktree, [4_u8; 32], "create wt", None);

        assert!(store.check("session-1", McpToolName::CreateWorktree, &[9_u8; 32]).is_none());
    }

    #[test]
    fn wire_form_round_trips_summary_and_tool() {
        let clock = ManualClock::new();
        let store = store_with(&clock, Duration::from_secs(60));
        let fingerprint = [5_u8; 32];
        let _ = store.record("session-1", McpToolName::CreateWorktree, fingerprint, "create wt summary", None);

        let wire = store.list_for_session_wire("session-1");
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0].tool, "create_worktree");
        assert_eq!(wire[0].summary, "create wt summary");
        assert_eq!(wire[0].args_fingerprint_hex, hex::encode(fingerprint));
    }
}
