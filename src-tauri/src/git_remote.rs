//! Pure parsing of a git `origin` remote URL into a [`GitProvider`] plus a normalised repository web URL.
//!
//! This is a pure function of the remote URL string — no subprocess spawning, no IO. The provider drives which CLI (`gh` / `glab` / `az repos`) the
//! PR-info lookup shells out to (see `plugins::dashboard_widget::git_status::pr_info`); the web URL is the fallback link shown when no provider CLI
//! is installed.
//!
//! Supported remote shapes:
//! * HTTPS: `https://github.com/owner/repo.git`, `https://gitlab.com/group/sub/repo.git`
//! * SCP-like SSH: `git@github.com:owner/repo.git`
//! * `ssh://` SSH: `ssh://git@github.com/owner/repo.git`
//! * Azure DevOps HTTPS: `https://dev.azure.com/org/project/_git/repo`, `https://org@dev.azure.com/org/project/_git/repo`
//! * Azure DevOps SSH: `git@ssh.dev.azure.com:v3/org/project/repo`
//! * Legacy Azure DevOps: `https://org.visualstudio.com/project/_git/repo`
//!
//! Enterprise / self-hosted hosts are matched best-effort via host heuristics (`github.*`, `gitlab.*`); anything unrecognised maps to
//! [`GitProvider::Unknown`].

use crate::types::GitProvider;

/// Result of parsing a remote URL: the detected provider and a best-effort https URL to the repository's web page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteInfo {
    pub provider: GitProvider,
    /// Normalised https URL to the repository web page, or `None` when the URL could not be parsed into one.
    pub repo_web_url: Option<String>,
}

/// Split a remote URL into `(host, path)` where `path` has any leading `/`, trailing `/`, and trailing `.git` stripped. Returns `None` for input
/// we can't make sense of. Handles `scheme://[user@]host[:port]/path` and the scp-like `[user@]host:path` SSH shorthand.
fn split_host_path(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }

    let (host_port, raw_path) = if let Some(rest) = url.split_once("://").map(|(_, r)| r) {
        // scheme://[user@]host[:port]/path
        let (authority, path) = match rest.split_once('/') {
            Some((a, p)) => (a, p),
            None => (rest, ""),
        };
        let host_port = authority.rsplit('@').next().unwrap_or(authority);
        (host_port.to_string(), path.to_string())
    } else if let Some((before, after)) = url.split_once(':') {
        // scp-like: `[user@]host:path`. scp syntax has no port, so a numeric first path segment (e.g. `host:2222/team/repo`) means this is really a
        // scheme-less `host:port/...` we can't reliably parse — bail rather than emit a wrong web URL (use `ssh://host:port/...` for ports).
        let host = before.rsplit('@').next().unwrap_or(before);
        let first_seg = after.split('/').next().unwrap_or(after);
        if !first_seg.is_empty() && first_seg.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        (host.to_string(), after.to_string())
    } else {
        return None;
    };

    // Strip an optional `:port` from the host.
    let host = host_port.split(':').next().unwrap_or(&host_port).to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }

    let path = raw_path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let path = path.trim_matches('/');
    if path.is_empty() {
        return None;
    }
    Some((host, path.to_string()))
}

/// Map a (lowercased) host to a [`GitProvider`] using best-effort heuristics that also catch common enterprise / self-hosted naming.
fn provider_for_host(host: &str) -> GitProvider {
    if host == "github.com" || host.starts_with("github.") {
        GitProvider::GitHub
    } else if host == "gitlab.com" || host.starts_with("gitlab.") || host.contains(".gitlab.") {
        GitProvider::GitLab
    } else if host == "dev.azure.com" || host == "ssh.dev.azure.com" || host == "vs-ssh.visualstudio.com" || host.ends_with(".visualstudio.com") {
        GitProvider::AzureDevOps
    } else {
        GitProvider::Unknown
    }
}

/// Build the repository web URL for an Azure DevOps remote from its already-split `(host, path)`.
///
/// Normalises the two SSH-ish encodings into the canonical `https://dev.azure.com/{org}/{project}/_git/{repo}` web form:
/// * `ssh.dev.azure.com` + `v3/org/project/repo` → strip the `v3` segment and splice `_git`.
/// * `dev.azure.com` + `org/project/_git/repo` → already web-shaped, pass through.
/// * `{org}.visualstudio.com` + `project/_git/repo` (optionally collection-prefixed) → keep the host, pass the path through.
fn azure_repo_web_url(host: &str, path: &str) -> Option<String> {
    if host == "ssh.dev.azure.com" || host == "vs-ssh.visualstudio.com" {
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        // Expect `v3/org/project/repo` (vs-ssh uses `v3/org/project/repo` as well).
        let trimmed: &[&str] = if segments.first() == Some(&"v3") {
            &segments[1..]
        } else {
            &segments[..]
        };
        if let [org, project, repo] = trimmed {
            return Some(format!("https://dev.azure.com/{org}/{project}/_git/{repo}"));
        }
        return None;
    }
    // dev.azure.com (web) and *.visualstudio.com already carry a web-shaped path that includes `_git`.
    Some(format!("https://{host}/{path}"))
}

/// Parse a git remote URL into a [`RemoteInfo`]. Best-effort: an unparseable URL yields `RemoteInfo { provider: Unknown, repo_web_url: None }`.
pub fn parse_remote_url(url: &str) -> RemoteInfo {
    let Some((host, path)) = split_host_path(url) else {
        return RemoteInfo {
            provider: GitProvider::Unknown,
            repo_web_url: None,
        };
    };
    let provider = provider_for_host(&host);
    let repo_web_url = match provider {
        GitProvider::GitHub | GitProvider::GitLab => Some(format!("https://{host}/{path}")),
        GitProvider::AzureDevOps => azure_repo_web_url(&host, &path),
        GitProvider::Unknown => None,
    };
    RemoteInfo { provider, repo_web_url }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_https() {
        let info = parse_remote_url("https://github.com/owner/repo.git");
        assert_eq!(info.provider, GitProvider::GitHub);
        assert_eq!(info.repo_web_url.as_deref(), Some("https://github.com/owner/repo"));
    }

    #[test]
    fn github_scp_ssh() {
        let info = parse_remote_url("git@github.com:owner/repo.git");
        assert_eq!(info.provider, GitProvider::GitHub);
        assert_eq!(info.repo_web_url.as_deref(), Some("https://github.com/owner/repo"));
    }

    #[test]
    fn github_ssh_scheme() {
        let info = parse_remote_url("ssh://git@github.com/owner/repo.git");
        assert_eq!(info.provider, GitProvider::GitHub);
        assert_eq!(info.repo_web_url.as_deref(), Some("https://github.com/owner/repo"));
    }

    #[test]
    fn github_https_no_dotgit() {
        let info = parse_remote_url("https://github.com/owner/repo");
        assert_eq!(info.repo_web_url.as_deref(), Some("https://github.com/owner/repo"));
    }

    #[test]
    fn github_enterprise_host() {
        let info = parse_remote_url("git@github.acme-corp.com:team/repo.git");
        assert_eq!(info.provider, GitProvider::GitHub);
        assert_eq!(info.repo_web_url.as_deref(), Some("https://github.acme-corp.com/team/repo"));
    }

    #[test]
    fn gitlab_subgroups() {
        let info = parse_remote_url("https://gitlab.com/group/subgroup/repo.git");
        assert_eq!(info.provider, GitProvider::GitLab);
        assert_eq!(info.repo_web_url.as_deref(), Some("https://gitlab.com/group/subgroup/repo"));
    }

    #[test]
    fn gitlab_self_hosted() {
        let info = parse_remote_url("git@gitlab.internal.example.com:team/repo.git");
        assert_eq!(info.provider, GitProvider::GitLab);
        assert_eq!(info.repo_web_url.as_deref(), Some("https://gitlab.internal.example.com/team/repo"));
    }

    #[test]
    fn ssh_with_port() {
        let info = parse_remote_url("ssh://git@gitlab.example.com:2222/team/repo.git");
        assert_eq!(info.provider, GitProvider::GitLab);
        assert_eq!(info.repo_web_url.as_deref(), Some("https://gitlab.example.com/team/repo"));
    }

    #[test]
    fn azure_https() {
        let info = parse_remote_url("https://dev.azure.com/myorg/myproject/_git/myrepo");
        assert_eq!(info.provider, GitProvider::AzureDevOps);
        assert_eq!(info.repo_web_url.as_deref(), Some("https://dev.azure.com/myorg/myproject/_git/myrepo"));
    }

    #[test]
    fn azure_https_with_org_user() {
        let info = parse_remote_url("https://myorg@dev.azure.com/myorg/myproject/_git/myrepo");
        assert_eq!(info.provider, GitProvider::AzureDevOps);
        assert_eq!(info.repo_web_url.as_deref(), Some("https://dev.azure.com/myorg/myproject/_git/myrepo"));
    }

    #[test]
    fn azure_ssh() {
        let info = parse_remote_url("git@ssh.dev.azure.com:v3/myorg/myproject/myrepo");
        assert_eq!(info.provider, GitProvider::AzureDevOps);
        assert_eq!(info.repo_web_url.as_deref(), Some("https://dev.azure.com/myorg/myproject/_git/myrepo"));
    }

    #[test]
    fn azure_visualstudio_legacy() {
        let info = parse_remote_url("https://myorg.visualstudio.com/myproject/_git/myrepo");
        assert_eq!(info.provider, GitProvider::AzureDevOps);
        assert_eq!(info.repo_web_url.as_deref(), Some("https://myorg.visualstudio.com/myproject/_git/myrepo"));
    }

    #[test]
    fn unknown_host() {
        let info = parse_remote_url("https://example.com/owner/repo.git");
        assert_eq!(info.provider, GitProvider::Unknown);
        assert_eq!(info.repo_web_url, None);
    }

    #[test]
    fn empty_and_garbage() {
        assert_eq!(parse_remote_url("").provider, GitProvider::Unknown);
        assert_eq!(parse_remote_url("not-a-url").provider, GitProvider::Unknown);
        assert_eq!(parse_remote_url("https://github.com/").provider, GitProvider::Unknown);
    }

    #[test]
    fn scheme_less_host_port_is_not_parsed_as_scp() {
        // `host:2222/team/repo` (no scheme) is a scheme-less host:port, not scp-like — git itself requires `ssh://` for ports. We must not emit a
        // bogus web URL from the digits-as-path misparse; it maps to Unknown with no web URL.
        let info = parse_remote_url("gitlab.example.com:2222/team/repo.git");
        assert_eq!(info.provider, GitProvider::Unknown);
        assert_eq!(info.repo_web_url, None);
    }
}
