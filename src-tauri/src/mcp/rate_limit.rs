//! Layered MCP rate limiter (per-session + per-workspace + per-host) on a token-bucket basis,
//! with the per-workspace state persisted atomically across host restarts so a malicious
//! client cannot reset its budget by restarting the sidecar.
//!
//! Input is the canonical `arborist_types::mcp::McpRateLimitsConfig` (flat `_per_min` /
//! `_per_hour` / `_per_60s` counts the user sees in `AppConfig.mcp.rateLimits`). We convert
//! to an internal `BucketConfig { capacity, window }` representation for the runtime token
//! bucket — that keeps the canonical wire shape stable while letting the limiter do its math
//! in a single representation.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tracing::warn;

use arborist_types::mcp::{McpRateLimits, McpRateLimitsConfig};

use crate::mcp::error::McpInternalError;
use crate::mcp::types::McpRateScope;

const RATE_FILE_NAME: &str = "mcp-rate.json";

/// Fine-grained bucket selector used by the rate limiter when a tool call comes in. This is
/// host-internal because the bucket layout is a runtime detail; the canonical wire form only
/// exposes the per-scope rate caps (`McpRateLimits`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpRateKind {
    StructuralRead,
    ExpensiveRead,
    Destructive,
    Total,
    CreateWorktree,
    Fetch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Consumed {
    pub budget_remaining: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimited {
    pub retry_after: Duration,
    pub budget_remaining: u32,
    pub scope: McpRateScope,
    pub bucket: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateOk {
    pub budget_remaining: u32,
    pub scope: McpRateScope,
    pub kind: McpRateKind,
    pub checked_buckets: Vec<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BucketConfig {
    capacity: u32,
    window: Duration,
}

impl BucketConfig {
    const fn new(capacity: u32, window_secs: u64) -> Self {
        Self {
            capacity,
            window: Duration::from_secs(window_secs),
        }
    }

    /// `None` (i.e., "no bucket for this kind at this scope") when capacity is 0. The
    /// canonical config encodes "disabled" as `0`; we collapse that to `Option::None`
    /// internally so the consume path can short-circuit without checking the count.
    fn or_disabled(self) -> Option<Self> {
        if self.capacity == 0 {
            None
        } else {
            Some(self)
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ScopeBucketsConfig {
    structural_read: Option<BucketConfig>,
    expensive_read: Option<BucketConfig>,
    destructive: Option<BucketConfig>,
    total: Option<BucketConfig>,
    create_worktree: Option<BucketConfig>,
    fetch: Option<BucketConfig>,
}

impl ScopeBucketsConfig {
    fn from_canonical(limits: &McpRateLimits) -> Self {
        Self {
            structural_read: BucketConfig::new(limits.structural_read_per_min, 60).or_disabled(),
            expensive_read: BucketConfig::new(limits.expensive_read_per_min, 60).or_disabled(),
            destructive: BucketConfig::new(limits.destructive_per_min, 60).or_disabled(),
            total: BucketConfig::new(limits.total_per_min, 60).or_disabled(),
            create_worktree: BucketConfig::new(limits.create_worktree_per_hour, 60 * 60).or_disabled(),
            fetch: BucketConfig::new(limits.fetch_per_60s, 60).or_disabled(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum BucketName {
    StructuralRead,
    ExpensiveRead,
    Destructive,
    Total,
    CreateWorktree,
    Fetch,
}

impl BucketName {
    #[must_use]
    pub const fn as_id(self) -> &'static str {
        match self {
            Self::StructuralRead => "structural-read",
            Self::ExpensiveRead => "expensive-read",
            Self::Destructive => "destructive",
            Self::Total => "total",
            Self::CreateWorktree => "create_worktree",
            Self::Fetch => "fetch",
        }
    }
}

#[derive(Debug, Clone)]
struct TokenBucket {
    config: BucketConfig,
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(config: BucketConfig, now: Instant) -> Self {
        Self {
            config,
            tokens: f64::from(config.capacity),
            last_refill: now,
        }
    }

    fn from_persisted(config: BucketConfig, state: PersistedBucketState, elapsed_since_last: Duration, now: Instant) -> Self {
        let mut bucket = Self {
            config,
            tokens: state.tokens.min(f64::from(config.capacity)),
            last_refill: now,
        };
        bucket.refill_by(elapsed_since_last);
        bucket.last_refill = now;
        bucket
    }

    fn refill_by(&mut self, elapsed: Duration) {
        if elapsed.is_zero() || self.config.capacity == 0 || self.config.window.is_zero() {
            return;
        }
        let refill_rate = f64::from(self.config.capacity) / self.config.window.as_secs_f64();
        self.tokens = (self.tokens + elapsed.as_secs_f64() * refill_rate).min(f64::from(self.config.capacity));
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last_refill);
        self.refill_by(elapsed);
        self.last_refill = now;
    }

    fn consume(&mut self, amount: u32, now: Instant) -> Result<Consumed, RateLimitedPreview> {
        self.refill(now);
        let needed = f64::from(amount);
        if self.tokens >= needed {
            self.tokens -= needed;
            return Ok(Consumed {
                budget_remaining: self.tokens.floor() as u32,
            });
        }

        let refill_rate = f64::from(self.config.capacity) / self.config.window.as_secs_f64();
        let missing = (needed - self.tokens).max(0.0);
        let retry_after = if refill_rate <= f64::EPSILON {
            self.config.window
        } else {
            Duration::from_secs_f64(missing / refill_rate)
        };

        Err(RateLimitedPreview {
            retry_after,
            budget_remaining: self.tokens.floor() as u32,
        })
    }

    fn to_persisted(&self, origin: Instant) -> PersistedBucketState {
        PersistedBucketState {
            tokens: self.tokens,
            last_refill_ms: u64::try_from(self.last_refill.saturating_duration_since(origin).as_millis()).unwrap_or(u64::MAX),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RateLimitedPreview {
    retry_after: Duration,
    budget_remaining: u32,
}

#[derive(Debug, Clone)]
struct ScopeBuckets {
    structural_read: Option<TokenBucket>,
    expensive_read: Option<TokenBucket>,
    destructive: Option<TokenBucket>,
    total: Option<TokenBucket>,
    create_worktree: Option<TokenBucket>,
    fetch: Option<TokenBucket>,
}

impl ScopeBuckets {
    fn new(config: ScopeBucketsConfig, now: Instant) -> Self {
        Self {
            structural_read: config.structural_read.map(|bucket| TokenBucket::new(bucket, now)),
            expensive_read: config.expensive_read.map(|bucket| TokenBucket::new(bucket, now)),
            destructive: config.destructive.map(|bucket| TokenBucket::new(bucket, now)),
            total: config.total.map(|bucket| TokenBucket::new(bucket, now)),
            create_worktree: config.create_worktree.map(|bucket| TokenBucket::new(bucket, now)),
            fetch: config.fetch.map(|bucket| TokenBucket::new(bucket, now)),
        }
    }

    fn from_persisted(
        config: ScopeBucketsConfig,
        persisted: PersistedScopeBuckets,
        origin_wall: OffsetDateTime,
        now_wall: OffsetDateTime,
        now_instant: Instant,
    ) -> Self {
        Self {
            structural_read: rebuild_bucket(config.structural_read, persisted.structural_read, origin_wall, now_wall, now_instant),
            expensive_read: rebuild_bucket(config.expensive_read, persisted.expensive_read, origin_wall, now_wall, now_instant),
            destructive: rebuild_bucket(config.destructive, persisted.destructive, origin_wall, now_wall, now_instant),
            total: rebuild_bucket(config.total, persisted.total, origin_wall, now_wall, now_instant),
            create_worktree: rebuild_bucket(config.create_worktree, persisted.create_worktree, origin_wall, now_wall, now_instant),
            fetch: rebuild_bucket(config.fetch, persisted.fetch, origin_wall, now_wall, now_instant),
        }
    }

    fn to_persisted(&self, origin: Instant) -> PersistedScopeBuckets {
        PersistedScopeBuckets {
            structural_read: self.structural_read.as_ref().map(|bucket| bucket.to_persisted(origin)),
            expensive_read: self.expensive_read.as_ref().map(|bucket| bucket.to_persisted(origin)),
            destructive: self.destructive.as_ref().map(|bucket| bucket.to_persisted(origin)),
            total: self.total.as_ref().map(|bucket| bucket.to_persisted(origin)),
            create_worktree: self.create_worktree.as_ref().map(|bucket| bucket.to_persisted(origin)),
            fetch: self.fetch.as_ref().map(|bucket| bucket.to_persisted(origin)),
        }
    }

    fn bucket_mut(&mut self, bucket: BucketName) -> Option<&mut TokenBucket> {
        match bucket {
            BucketName::StructuralRead => self.structural_read.as_mut(),
            BucketName::ExpensiveRead => self.expensive_read.as_mut(),
            BucketName::Destructive => self.destructive.as_mut(),
            BucketName::Total => self.total.as_mut(),
            BucketName::CreateWorktree => self.create_worktree.as_mut(),
            BucketName::Fetch => self.fetch.as_mut(),
        }
    }

    fn try_consume(&mut self, kind: McpRateKind, scope: McpRateScope, now: Instant) -> Result<RateOk, RateLimited> {
        let buckets = required_buckets(kind);
        let mut trial = self.clone();
        let mut checked_buckets = Vec::new();
        let mut budget_remaining = u32::MAX;

        for bucket in buckets {
            let bucket = *bucket;
            let Some(bucket_state) = trial.bucket_mut(bucket) else {
                continue;
            };
            checked_buckets.push(bucket.as_id());
            match bucket_state.consume(1, now) {
                Ok(consumed) => {
                    budget_remaining = budget_remaining.min(consumed.budget_remaining);
                }
                Err(rate_limited) => {
                    return Err(RateLimited {
                        retry_after: rate_limited.retry_after,
                        budget_remaining: rate_limited.budget_remaining,
                        scope,
                        bucket: bucket.as_id(),
                    });
                }
            }
        }

        *self = trial;
        Ok(RateOk {
            budget_remaining: if checked_buckets.is_empty() { u32::MAX } else { budget_remaining },
            scope,
            kind,
            checked_buckets,
        })
    }
}

#[derive(Debug)]
struct RateLimitState {
    per_session: HashMap<String, ScopeBuckets>,
    per_workspace: ScopeBuckets,
    per_host: ScopeBuckets,
}

pub struct LayeredRateLimiter {
    per_session_config: ScopeBucketsConfig,
    workspace_state_dir: PathBuf,
    origin_instant: Instant,
    origin_wall: OffsetDateTime,
    state: Mutex<RateLimitState>,
}

impl LayeredRateLimiter {
    /// Construct from the canonical `McpRateLimitsConfig` the user persists in
    /// `AppConfig.mcp.rateLimits`. The per-workspace state is loaded from `mcp-rate.json` in
    /// `workspace_state_dir` if present; missing / unparseable files are treated as fresh.
    #[must_use]
    pub fn new(config: McpRateLimitsConfig, workspace_state_dir: PathBuf) -> Self {
        let origin_instant = Instant::now();
        let origin_wall = OffsetDateTime::now_utc();
        let per_session_config = ScopeBucketsConfig::from_canonical(&config.per_session);
        let per_workspace_config = ScopeBucketsConfig::from_canonical(&config.per_workspace);
        let per_host_config = ScopeBucketsConfig::from_canonical(&config.per_host);

        let per_workspace = load_workspace_buckets(
            &workspace_state_dir.join(RATE_FILE_NAME),
            per_workspace_config,
            origin_wall,
            origin_instant,
        );
        let per_host = ScopeBuckets::new(per_host_config, origin_instant);

        Self {
            per_session_config,
            workspace_state_dir,
            origin_instant,
            origin_wall,
            state: Mutex::new(RateLimitState {
                per_session: HashMap::new(),
                per_workspace,
                per_host,
            }),
        }
    }

    /// Attempt to consume one token of `kind` against the bucket(s) for `scope` keyed by `key`.
    /// Returns `Ok(RateOk { budget_remaining, .. })` if the bucket(s) had budget, or an
    /// `McpInternalError::RateLimited` carrying the retry-after hint and remaining budget so
    /// the IPC layer can render an `MCPError` with structured `retryAfterMs` /
    /// `budgetRemaining` fields.
    pub fn check_and_consume(&self, scope: McpRateScope, key: &str, kind: McpRateKind, now: Instant) -> Result<RateOk, McpInternalError> {
        let mut guard = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        let result = match scope {
            McpRateScope::PerSession => guard
                .per_session
                .entry(key.to_owned())
                .or_insert_with(|| ScopeBuckets::new(self.per_session_config, now))
                .try_consume(kind, scope, now),
            McpRateScope::PerWorkspace => guard.per_workspace.try_consume(kind, scope, now),
            McpRateScope::PerHost => guard.per_host.try_consume(kind, scope, now),
        };

        match result {
            Ok(ok) => {
                if matches!(scope, McpRateScope::PerWorkspace) {
                    if let Err(err) = self.persist_workspace_state(&guard) {
                        warn!(path = %self.persisted_path().display(), error = %err, "failed to persist workspace MCP rate state");
                    }
                }
                Ok(ok)
            }
            Err(limited) => Err(McpInternalError::RateLimited {
                message: format!(
                    "{} {} budget exhausted; retry after {} ms",
                    scope_id(limited.scope),
                    limited.bucket,
                    limited.retry_after.as_millis()
                ),
                retry_after: limited.retry_after,
                budget_remaining: limited.budget_remaining,
            }),
        }
    }

    pub fn clear_session(&self, session_id: &str) {
        let mut guard = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.per_session.remove(session_id);
    }

    fn persisted_path(&self) -> PathBuf {
        self.workspace_state_dir.join(RATE_FILE_NAME)
    }

    fn persist_workspace_state(&self, state: &RateLimitState) -> io::Result<()> {
        let snapshot_key = format_rfc3339(self.origin_wall)?;
        let mut root = BTreeMap::new();
        root.insert(
            snapshot_key,
            PersistedWorkspaceSnapshot {
                buckets: state.per_workspace.to_persisted(self.origin_instant),
            },
        );
        write_atomic_json(&self.persisted_path(), &root)
    }
}

const fn scope_id(scope: McpRateScope) -> &'static str {
    match scope {
        McpRateScope::PerSession => "per-session",
        McpRateScope::PerWorkspace => "per-workspace",
        McpRateScope::PerHost => "per-host",
    }
}

fn required_buckets(kind: McpRateKind) -> &'static [BucketName] {
    match kind {
        McpRateKind::StructuralRead => &[BucketName::StructuralRead, BucketName::Total],
        McpRateKind::ExpensiveRead => &[BucketName::ExpensiveRead, BucketName::Total],
        McpRateKind::Destructive => &[BucketName::Destructive, BucketName::Total],
        McpRateKind::Total => &[BucketName::Total],
        McpRateKind::CreateWorktree => &[BucketName::CreateWorktree, BucketName::Total],
        McpRateKind::Fetch => &[BucketName::Fetch, BucketName::Total],
    }
}

fn rebuild_bucket(
    config: Option<BucketConfig>,
    persisted: Option<PersistedBucketState>,
    origin_wall: OffsetDateTime,
    now_wall: OffsetDateTime,
    now_instant: Instant,
) -> Option<TokenBucket> {
    let config = config?;
    let persisted = match persisted {
        Some(persisted) => persisted,
        None => return Some(TokenBucket::new(config, now_instant)),
    };
    let last_refill_wall = origin_wall + time::Duration::milliseconds(i64::try_from(persisted.last_refill_ms).unwrap_or(i64::MAX));
    let elapsed = positive_elapsed(now_wall - last_refill_wall);
    Some(TokenBucket::from_persisted(config, persisted, elapsed, now_instant))
}

fn load_workspace_buckets(path: &Path, config: ScopeBucketsConfig, now_wall: OffsetDateTime, now_instant: Instant) -> ScopeBuckets {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return ScopeBuckets::new(config, now_instant),
        Err(err) => {
            warn!(path = %path.display(), error = %err, "failed to read persisted workspace MCP rate state; starting fresh");
            return ScopeBuckets::new(config, now_instant);
        }
    };

    let snapshots: BTreeMap<String, PersistedWorkspaceSnapshot> = match serde_json::from_str(&raw) {
        Ok(snapshots) => snapshots,
        Err(err) => {
            warn!(path = %path.display(), error = %err, "failed to parse persisted workspace MCP rate state; starting fresh");
            return ScopeBuckets::new(config, now_instant);
        }
    };
    // We only ever write a single snapshot, but if multiple ever appear (older host wrote
    // multiple keys), take the most recent. `next_back` instead of `last` to avoid walking the
    // whole iterator on a sorted `BTreeMap`.
    let Some((origin_wall, snapshot)) = snapshots
        .into_iter()
        .next_back()
        .and_then(|(key, snapshot)| parse_rfc3339(&key).map(|parsed| (parsed, snapshot)))
    else {
        return ScopeBuckets::new(config, now_instant);
    };

    ScopeBuckets::from_persisted(config, snapshot.buckets, origin_wall, now_wall, now_instant)
}

fn parse_rfc3339(value: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value, &Rfc3339).ok()
}

fn format_rfc3339(value: OffsetDateTime) -> io::Result<String> {
    value.format(&Rfc3339).map_err(io::Error::other)
}

fn positive_elapsed(delta: time::Duration) -> Duration {
    if delta.is_negative() {
        Duration::ZERO
    } else {
        Duration::from_secs_f64(delta.as_seconds_f64())
    }
}

fn write_atomic_json<T: Serialize>(target: &Path, value: &T) -> io::Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("target has no parent: {}", target.display())))?;
    fs::create_dir_all(parent)?;

    let payload = serde_json::to_vec_pretty(value)?;
    let mut tmp = NamedTempFile::new_in(parent)?;
    tmp.write_all(&payload)?;
    tmp.flush()?;
    tmp.as_file().sync_all()?;
    tmp.persist(target).map_err(|err| err.error)?;

    #[cfg(unix)]
    {
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct PersistedBucketState {
    tokens: f64,
    last_refill_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedScopeBuckets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    structural_read: Option<PersistedBucketState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expensive_read: Option<PersistedBucketState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    destructive: Option<PersistedBucketState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    total: Option<PersistedBucketState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    create_worktree: Option<PersistedBucketState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fetch: Option<PersistedBucketState>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PersistedWorkspaceSnapshot {
    buckets: PersistedScopeBuckets,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn token_bucket_capacity_respected_and_refills_over_time() {
        let t0 = Instant::now();
        let mut bucket = TokenBucket::new(BucketConfig::new(10, 60), t0);

        bucket.consume(10, t0).expect("initial burst should fit");
        assert!(bucket.consume(1, t0).is_err());
        bucket
            .consume(5, t0 + Duration::from_secs(30))
            .expect("half-window refill should restore five tokens");
        assert!(bucket.consume(1, t0 + Duration::from_secs(30)).is_err());
        bucket
            .consume(5, t0 + Duration::from_secs(60))
            .expect("full window should refill another five tokens");
    }

    #[test]
    fn retry_after_ms_matches_refill_rate() {
        let t0 = Instant::now();
        let mut bucket = TokenBucket::new(BucketConfig::new(5, 60), t0);
        bucket.consume(5, t0).expect("initial consume should fit");

        let limited = bucket.consume(1, t0).expect_err("bucket should be empty");
        assert_eq!(limited.retry_after, Duration::from_secs(12));
        assert_eq!(limited.budget_remaining, 0);
    }

    fn small_workspace_only_config() -> McpRateLimitsConfig {
        // Disable everything except per_workspace.total=2 to make the test deterministic. The
        // `0` values collapse to `None` internally so we never hit them.
        McpRateLimitsConfig {
            per_session: McpRateLimits::new(0, 0, 0, 0, 0, 0),
            per_workspace: McpRateLimits::new(0, 0, 0, 2, 0, 0),
            per_host: McpRateLimits::new(0, 0, 0, 0, 0, 0),
        }
    }

    #[test]
    fn persistence_round_trip_preserves_workspace_state() {
        let temp_dir = TempDir::new().expect("tempdir");
        let limiter = LayeredRateLimiter::new(small_workspace_only_config(), temp_dir.path().to_path_buf());
        let now = Instant::now();
        limiter
            .check_and_consume(McpRateScope::PerWorkspace, "workspace", McpRateKind::Total, now)
            .expect("first workspace token should fit");
        limiter
            .check_and_consume(McpRateScope::PerWorkspace, "workspace", McpRateKind::Total, now)
            .expect("second workspace token should fit");

        let reloaded = LayeredRateLimiter::new(small_workspace_only_config(), temp_dir.path().to_path_buf());
        let err = reloaded
            .check_and_consume(McpRateScope::PerWorkspace, "workspace", McpRateKind::Total, Instant::now())
            .expect_err("persisted workspace bucket should still be exhausted");

        assert_eq!(err.code(), arborist_types::mcp::McpErrorCode::RateLimited);
    }

    #[test]
    fn defaults_match_overview_section_5_2_2() {
        // The canonical defaults (per-min / per-hour / per-60s) match the §5.2.2 caps in
        // `dev/ai/00-mcp-server-overview.md`. We translate them through `from_canonical` and
        // re-check the runtime view — keeping the test wired against the canonical numbers
        // rather than the internal `BucketConfig` representation.
        let defaults = McpRateLimitsConfig::default();
        let per_session = ScopeBucketsConfig::from_canonical(&defaults.per_session);
        let per_workspace = ScopeBucketsConfig::from_canonical(&defaults.per_workspace);
        let per_host = ScopeBucketsConfig::from_canonical(&defaults.per_host);

        assert_eq!(per_session.total.map(|bucket| bucket.capacity), Some(30));
        assert_eq!(per_session.destructive.map(|bucket| bucket.capacity), Some(5));
        assert_eq!(per_workspace.total.map(|bucket| bucket.capacity), Some(100));
        assert_eq!(per_workspace.destructive.map(|bucket| bucket.capacity), Some(15));
        assert_eq!(per_workspace.create_worktree.map(|bucket| bucket.capacity), Some(30));
        assert_eq!(per_host.total.map(|bucket| bucket.capacity), Some(500));
    }
}
