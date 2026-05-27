use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::commands::AppContext;
use crate::mcp::audit::AuditLog;
use crate::mcp::confirm::PendingMcpActionRegistry;
use crate::mcp::rate_limit::LayeredRateLimiter;
use crate::mcp::trust::TrustedRequestStore;
use crate::mcp::types::McpContextConfig;

pub struct McpContext {
    pub app: Arc<AppContext>,
    pub workspace_state_dir: PathBuf,
    pub rate: Arc<LayeredRateLimiter>,
    pub audit: Arc<AuditLog>,
    pub confirm: Arc<PendingMcpActionRegistry>,
    pub trust: Arc<TrustedRequestStore>,
    pub sidecar_hash: [u8; 32],
}

impl McpContext {
    pub fn new(app: Arc<AppContext>, config: McpContextConfig, workspace_state_dir: PathBuf) -> io::Result<Self> {
        let rate = Arc::new(LayeredRateLimiter::new(config.rate_limits, workspace_state_dir.clone()));
        let audit = Arc::new(AuditLog::new(workspace_state_dir.clone())?);
        let confirm = Arc::new(PendingMcpActionRegistry::new());
        let trust = Arc::new(TrustedRequestStore::new(config.trust_ttl));
        let sidecar_hash = resolve_sidecar_hash()?;

        Ok(Self {
            app,
            workspace_state_dir,
            rate,
            audit,
            confirm,
            trust,
            sidecar_hash,
        })
    }
}

fn resolve_sidecar_hash() -> io::Result<[u8; 32]> {
    let Ok(current_exe) = std::env::current_exe() else {
        return Ok([0; 32]);
    };
    let Some(parent) = current_exe.parent() else {
        return Ok([0; 32]);
    };
    let sidecar = parent.join(format!("arborist-mcp{}", std::env::consts::EXE_SUFFIX));
    if !sidecar.is_file() {
        return Ok([0; 32]);
    }
    sha256_file(&sidecar)
}

fn sha256_file(path: &std::path::Path) -> io::Result<[u8; 32]> {
    let mut file = File::open(path)?;
    let mut buffer = [0_u8; 8 * 1024];
    let mut hasher = Sha256::new();

    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hasher.finalize().into())
}
