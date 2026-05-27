use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::mcp::error::{error, McpInternalError};
use crate::mcp::types::{ConfirmationToken as ConfirmationTokenWire, McpErrorCode, McpPendingAction as McpPendingActionWire, McpToolName};

type InstantClock = Arc<dyn Fn() -> Instant + Send + Sync>;
type WallClock = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

#[derive(Debug, Clone)]
pub struct PendingMcpAction {
    pub id: String,
    pub session_id: String,
    pub tool: McpToolName,
    pub summary: String,
    pub args_fingerprint: [u8; 32],
    pub created_at: Instant,
    pub created_at_wall: OffsetDateTime,
    pub expires_at: Instant,
    pub expires_at_wall: OffsetDateTime,
    pub payload: Value,
}

impl PendingMcpAction {
    #[must_use]
    pub fn to_wire(&self) -> Option<McpPendingActionWire> {
        // why: Finding 16 from the UX review — confirmation summaries can balloon (e.g. a
        // 60-worktree cleanup listing every path) and overwhelm the inline banner. We cap the
        // wire `summary` to ~200 chars at create() time and expose the full args payload as
        // `details` so the UI can render a "View full request" expander without losing fidelity.
        let details = if self.payload.is_null() { None } else { Some(self.payload.clone()) };
        Some(McpPendingActionWire {
            id: self.id.clone(),
            session_id: self.session_id.clone(),
            tool: self.tool.as_id().to_owned(),
            summary: self.summary.clone(),
            details,
            args_fingerprint_hex: hex::encode(self.args_fingerprint),
            created_at: self.created_at_wall.format(&Rfc3339).ok()?,
            expires_at: self.expires_at_wall.format(&Rfc3339).ok()?,
        })
    }
}

#[derive(Debug, Clone)]
struct ApprovedAction {
    token: String,
    action: PendingMcpAction,
}

#[derive(Debug, Clone)]
pub struct ConsumedAction {
    pub action: PendingMcpAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumeError {
    Unknown,
    Expired,
    FingerprintMismatch,
}

#[derive(Debug, Default)]
struct RegistryState {
    pending: HashMap<String, PendingMcpAction>,
    approved: HashMap<String, ApprovedAction>,
}

pub struct PendingMcpActionRegistry {
    inner: Mutex<RegistryState>,
    clock: InstantClock,
    wall_clock: WallClock,
    ttl: Duration,
    max_per_session: usize,
}

impl PendingMcpActionRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::with_clocks(Duration::from_secs(60), 32, Arc::new(Instant::now), Arc::new(OffsetDateTime::now_utc))
    }

    fn with_clocks(ttl: Duration, max_per_session: usize, clock: InstantClock, wall_clock: WallClock) -> Self {
        Self {
            inner: Mutex::new(RegistryState::default()),
            clock,
            wall_clock,
            ttl,
            max_per_session,
        }
    }

    /// Creates a fresh pending action. Sweeps expired entries first so the per-session cap
    /// doesn't lock the session out after a flurry of unanswered prompts.
    pub fn create(
        &self,
        session_id: impl Into<String>,
        tool: McpToolName,
        summary: impl Into<String>,
        args_fingerprint: [u8; 32],
        payload: Value,
    ) -> Result<PendingMcpAction, McpInternalError> {
        let session_id = session_id.into();
        let now = (self.clock)();
        let now_wall = (self.wall_clock)();
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        sweep_expired(&mut guard, now);

        let pending_for_session = guard.pending.values().filter(|action| action.session_id == session_id).count();
        if pending_for_session >= self.max_per_session {
            let message = format!("session '{session_id}' already has {} pending MCP actions", self.max_per_session);
            return Err(error(McpErrorCode::TooManyPendingActions, message));
        }

        let action = PendingMcpAction {
            id: Uuid::new_v4().as_simple().to_string(),
            session_id,
            tool,
            summary: truncate_summary(summary.into()),
            args_fingerprint,
            created_at: now,
            created_at_wall: now_wall,
            expires_at: now + self.ttl,
            expires_at_wall: now_wall + self.ttl,
            payload,
        };
        guard.pending.insert(action.id.clone(), action.clone());
        Ok(action)
    }

    /// Turns a pending action into a short-lived replay token consumed by `try_consume`.
    ///
    /// // why: `create()` already models "the agent wants approval" in the existing Phase 1
    /// scaffolding, so Phase 2 keeps that meaning stable and adds a distinct `approve()` step for
    /// the user-driven transition from pending request -> consumable confirmation token.
    #[must_use]
    pub fn approve(&self, action_id: &str) -> Option<ConfirmationTokenWire> {
        let now = (self.clock)();
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        sweep_expired(&mut guard, now);

        let action = guard.pending.remove(action_id)?;
        let approved = ApprovedAction {
            token: Uuid::new_v4().as_simple().to_string(),
            action,
        };
        let wire = ConfirmationTokenWire {
            token: approved.token.clone(),
            expires_at: approved.action.expires_at_wall.format(&Rfc3339).ok()?,
        };
        guard.approved.insert(approved.token.clone(), approved);
        Some(wire)
    }

    pub fn deny(&self, action_id: &str) -> bool {
        let now = (self.clock)();
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        sweep_expired(&mut guard, now);
        guard.pending.remove(action_id).is_some()
    }

    /// Atomically consumes an approved confirmation token, checking expiry and fingerprint match
    /// before returning the underlying action to the caller.
    pub fn try_consume(&self, token: &str, expected_fingerprint: &[u8; 32]) -> Result<ConsumedAction, ConsumeError> {
        let now = (self.clock)();
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        let approved = match guard.approved.get(token) {
            Some(action) => action.clone(),
            None => {
                sweep_expired(&mut guard, now);
                return Err(ConsumeError::Unknown);
            }
        };
        if now >= approved.action.expires_at {
            guard.approved.remove(token);
            sweep_expired(&mut guard, now);
            return Err(ConsumeError::Expired);
        }
        if &approved.action.args_fingerprint != expected_fingerprint {
            sweep_expired(&mut guard, now);
            return Err(ConsumeError::FingerprintMismatch);
        }
        guard.approved.remove(token);
        sweep_expired(&mut guard, now);
        Ok(ConsumedAction { action: approved.action })
    }

    #[must_use]
    pub fn list_for_session(&self, session_id: &str) -> Vec<PendingMcpAction> {
        let now = (self.clock)();
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        sweep_expired(&mut guard, now);

        let mut actions: Vec<_> = guard.pending.values().filter(|action| action.session_id == session_id).cloned().collect();
        actions.sort_by_key(|action| action.created_at);
        actions
    }

    #[must_use]
    pub fn list_for_session_wire(&self, session_id: &str) -> Vec<McpPendingActionWire> {
        self.list_for_session(session_id).iter().filter_map(PendingMcpAction::to_wire).collect()
    }

    #[must_use]
    pub fn list_all(&self) -> Vec<PendingMcpAction> {
        let now = (self.clock)();
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        sweep_expired(&mut guard, now);

        let mut actions: Vec<_> = guard.pending.values().cloned().collect();
        actions.sort_by_key(|action| action.created_at);
        actions
    }

    #[must_use]
    pub fn list_all_wire(&self) -> Vec<McpPendingActionWire> {
        self.list_all().iter().filter_map(PendingMcpAction::to_wire).collect()
    }

    pub fn clear_session(&self, session_id: &str) {
        let now = (self.clock)();
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        sweep_expired(&mut guard, now);
        guard.pending.retain(|_, action| action.session_id != session_id);
        guard.approved.retain(|_, action| action.action.session_id != session_id);
    }
}

impl Default for PendingMcpActionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[must_use]
pub fn fingerprint_args(canonical_json: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(canonical_json.as_bytes());
    hasher.finalize().into()
}

fn sweep_expired(state: &mut RegistryState, now: Instant) {
    state.pending.retain(|_, action| action.expires_at > now);
    state.approved.retain(|_, action| action.action.expires_at > now);
}

/// Maximum length (in chars, not bytes) of a confirmation `summary` shown inline in the banner.
///
/// Long summaries (e.g. cleanup listing 60 worktrees) overwhelm the inline banner and bury the
/// approve/deny buttons. The full args payload is preserved in `details` so the UI can render a
/// "View full request" expander without losing fidelity.
const SUMMARY_MAX_CHARS: usize = 200;

fn truncate_summary(input: String) -> String {
    // why: char-boundary safe truncation so we never split a multi-byte UTF-8 codepoint mid-way.
    // The ellipsis indicates more detail is available in the `details` payload.
    if input.chars().count() <= SUMMARY_MAX_CHARS {
        return input;
    }
    let mut out = String::with_capacity(SUMMARY_MAX_CHARS * 4 + 1);
    for ch in input.chars().take(SUMMARY_MAX_CHARS.saturating_sub(1)) {
        out.push(ch);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    fn registry_with(clock: &ManualClock, ttl: Duration, max_per_session: usize) -> PendingMcpActionRegistry {
        PendingMcpActionRegistry::with_clocks(ttl, max_per_session, clock.instant_reader(), clock.wall_reader())
    }

    #[test]
    fn create_approve_and_consume_happy_path() {
        let clock = ManualClock::new();
        let registry = registry_with(&clock, Duration::from_secs(60), 32);
        let fingerprint = fingerprint_args("{\"name\":\"demo\"}");

        let pending = registry
            .create(
                "session-1",
                McpToolName::CreateWorktree,
                "create demo",
                fingerprint,
                json!({"name": "demo"}),
            )
            .expect("create should succeed");
        let token = registry.approve(&pending.id).expect("approve should mint a token");
        let consumed = registry.try_consume(&token.token, &fingerprint).expect("consume should succeed");

        assert_eq!(consumed.action.id, pending.id);
        assert!(registry.list_for_session("session-1").is_empty());
    }

    #[test]
    fn deny_removes_pending_action() {
        let clock = ManualClock::new();
        let registry = registry_with(&clock, Duration::from_secs(60), 32);
        let pending = registry
            .create(
                "session-1",
                McpToolName::CleanupMergedWorktrees,
                "cleanup",
                fingerprint_args("{}"),
                json!({}),
            )
            .expect("create should succeed");

        assert!(registry.deny(&pending.id));
        assert!(registry.list_for_session("session-1").is_empty());
    }

    #[test]
    fn consume_twice_returns_unknown_second_time() {
        let clock = ManualClock::new();
        let registry = registry_with(&clock, Duration::from_secs(60), 32);
        let fingerprint = fingerprint_args("{}");
        let pending = registry
            .create("session-1", McpToolName::CleanupMergedWorktrees, "cleanup", fingerprint, json!({}))
            .expect("create should succeed");
        let token = registry.approve(&pending.id).expect("approve should succeed");

        registry.try_consume(&token.token, &fingerprint).expect("first consume should succeed");
        let err = registry.try_consume(&token.token, &fingerprint).expect_err("second consume should fail");

        assert_eq!(err, ConsumeError::Unknown);
    }

    #[test]
    fn expired_token_returns_expired() {
        let clock = ManualClock::new();
        let registry = registry_with(&clock, Duration::from_secs(10), 32);
        let fingerprint = fingerprint_args("{}");
        let pending = registry
            .create("session-1", McpToolName::CreateWorktree, "create", fingerprint, json!({}))
            .expect("create should succeed");
        let token = registry.approve(&pending.id).expect("approve should succeed");

        clock.advance(Duration::from_secs(11));

        let err = registry
            .try_consume(&token.token, &fingerprint)
            .expect_err("consume should fail once expired");
        assert_eq!(err, ConsumeError::Expired);

        let second = registry
            .try_consume(&token.token, &fingerprint)
            .expect_err("second consume after expiry should fail as unknown");
        assert_eq!(second, ConsumeError::Unknown);
    }

    #[test]
    fn mismatched_fingerprint_returns_fingerprint_mismatch_and_does_not_consume_token() {
        let clock = ManualClock::new();
        let registry = registry_with(&clock, Duration::from_secs(60), 32);
        let fingerprint = fingerprint_args("{\"source\":\"main\"}");
        let pending = registry
            .create(
                "session-1",
                McpToolName::MergeMainIntoWorktrees,
                "merge",
                fingerprint,
                json!({"source": "main"}),
            )
            .expect("create should succeed");
        let token = registry.approve(&pending.id).expect("approve should succeed");

        let err = registry
            .try_consume(&token.token, &fingerprint_args("{\"source\":\"release\"}"))
            .expect_err("consume should reject mismatched fingerprint");
        assert_eq!(err, ConsumeError::FingerprintMismatch);

        let consumed = registry
            .try_consume(&token.token, &fingerprint)
            .expect("retry with correct fingerprint should succeed");
        assert_eq!(consumed.action.id, pending.id);
    }

    #[test]
    fn per_session_cap_returns_too_many_pending_actions() {
        let clock = ManualClock::new();
        let registry = registry_with(&clock, Duration::from_secs(60), 1);
        let fingerprint = fingerprint_args("{}");
        registry
            .create("session-1", McpToolName::CreateWorktree, "first", fingerprint, json!({}))
            .expect("first action should fit under cap");

        let err = registry
            .create("session-1", McpToolName::CreateWorktree, "second", fingerprint, json!({}))
            .expect_err("second action should exceed cap");

        assert_eq!(err.code(), McpErrorCode::TooManyPendingActions);
    }

    #[test]
    fn wire_list_uses_rfc3339_timestamps() {
        let clock = ManualClock::new();
        let registry = registry_with(&clock, Duration::from_secs(60), 32);
        registry
            .create("session-1", McpToolName::CreateWorktree, "create", fingerprint_args("{}"), json!({}))
            .expect("create should succeed");

        let wire = registry.list_for_session_wire("session-1");
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0].tool, "create_worktree");
        assert!(!wire[0].created_at.is_empty());
        assert!(!wire[0].expires_at.is_empty());
    }

    #[test]
    fn long_summary_is_truncated_with_ellipsis_and_details_carries_full_payload() {
        let clock = ManualClock::new();
        let registry = registry_with(&clock, Duration::from_secs(60), 32);
        let long_summary = "Remove 60 merged worktrees: ".to_owned() + &"path-name/".repeat(80);
        let payload = json!({"wouldRemove": vec!["path-a", "path-b", "path-c"]});
        registry
            .create(
                "session-1",
                McpToolName::CleanupMergedWorktrees,
                long_summary,
                fingerprint_args("{}"),
                payload.clone(),
            )
            .expect("create should succeed");

        let wire = registry.list_for_session_wire("session-1");
        assert_eq!(wire.len(), 1);
        assert!(
            wire[0].summary.chars().count() <= SUMMARY_MAX_CHARS,
            "summary should be capped at SUMMARY_MAX_CHARS"
        );
        assert!(wire[0].summary.ends_with('…'), "truncated summary should end with ellipsis");
        assert_eq!(wire[0].details.as_ref(), Some(&payload), "full payload should be preserved in details");
    }

    #[test]
    fn short_summary_is_unchanged_and_null_payload_yields_no_details() {
        let clock = ManualClock::new();
        let registry = registry_with(&clock, Duration::from_secs(60), 32);
        registry
            .create(
                "session-1",
                McpToolName::CreateWorktree,
                "create feature/foo",
                fingerprint_args("{}"),
                Value::Null,
            )
            .expect("create should succeed");

        let wire = registry.list_for_session_wire("session-1");
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0].summary, "create feature/foo", "short summaries are preserved verbatim");
        assert_eq!(wire[0].details, None, "null payload yields no details to avoid noise in the UI");
    }

    #[test]
    fn truncate_summary_handles_multi_byte_codepoint_boundary() {
        // why: a naive byte-slice would corrupt UTF-8 if the cap landed mid-codepoint. Use a
        // string of 4-byte emojis to verify the boundary-safe truncation.
        let emoji_summary = "🚀".repeat(SUMMARY_MAX_CHARS + 50);
        let result = truncate_summary(emoji_summary);
        assert!(result.chars().count() <= SUMMARY_MAX_CHARS);
        assert!(result.ends_with('…'));
        // Validate it's still valid UTF-8 (round-trip through str).
        let _: &str = &result;
    }
}
