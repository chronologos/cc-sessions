//! Remote SSH session support.
//!
//! This module handles syncing Claude Code sessions from remote machines
//! via SSH/rsync, caching them locally for fast interactive browsing.
//!
//! ## Architecture
//!
//! Sessions are synced to a local cache using rsync over SSH:
//! ```text
//! remote:~/.claude/projects/  -->  ~/.cache/cc-sessions/remotes/<name>/
//! ```
//!
//! This enables sub-100ms response times for preview and search, which
//! would be impossible with network round-trips per operation.
//!
//! ## Config Format
//!
//! ```toml
//! [remotes.devbox]
//! host = "devbox"  # SSH config alias
//!
//! [remotes.workstation]
//! host = "192.168.1.100"
//! user = "ec2-user"  # Optional for raw hosts
//!
//! [settings]
//! cache_dir = "~/.cache/cc-sessions/remotes"
//! stale_threshold = 3600  # Seconds before auto-sync
//! ```

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// =============================================================================
// Configuration
// =============================================================================

/// Top-level config file structure
#[derive(Debug, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub remotes: HashMap<String, RemoteConfig>,
    #[serde(default)]
    pub settings: Settings,
}

/// Configuration for a single remote machine
#[derive(Debug, Deserialize, Clone)]
pub struct RemoteConfig {
    /// SSH host (alias from ~/.ssh/config or raw hostname/IP)
    pub host: String,
    /// Optional user for raw hosts (not needed if using SSH config alias)
    pub user: Option<String>,
    /// Override for non-standard projects directory
    pub projects_dir: Option<String>,
}

/// Global settings
#[derive(Debug, Deserialize)]
pub struct Settings {
    /// Directory to cache remote sessions
    #[serde(default = "default_cache_dir")]
    pub cache_dir: String,
    /// Seconds before a cache is considered stale (default: 1 hour)
    #[serde(default = "default_stale_threshold")]
    pub stale_threshold: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            cache_dir: default_cache_dir(),
            stale_threshold: default_stale_threshold(),
        }
    }
}

fn default_cache_dir() -> String {
    "~/.cache/cc-sessions/remotes".to_string()
}

fn default_stale_threshold() -> u64 {
    3600 // 1 hour
}

// =============================================================================
// Config Loading
// =============================================================================

/// Load remote configuration from ~/.config/cc-sessions/remotes.toml
pub fn load_config() -> Result<Config> {
    let config_path = get_config_path()?;

    if !config_path.exists() {
        // No config file = no remotes configured
        return Ok(Config::default());
    }

    let content = fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;

    let config: Config = toml::from_str(&content)
        .with_context(|| format!("Failed to parse config file: {}", config_path.display()))?;

    Ok(config)
}

/// Get the config file path
fn get_config_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not find home directory")?;
    Ok(home.join(".config/cc-sessions/remotes.toml"))
}

// =============================================================================
// Path Helpers
// =============================================================================

/// Expand ~ in paths to home directory
pub fn expand_path(path: &str) -> Result<PathBuf> {
    let expanded = shellexpand::tilde(path);
    Ok(PathBuf::from(expanded.as_ref()))
}

/// Get the cache directory for a specific remote
pub fn get_remote_cache_dir(settings: &Settings, remote_name: &str) -> Result<PathBuf> {
    let cache_base = expand_path(&settings.cache_dir)?;
    Ok(cache_base.join(remote_name))
}

/// Build SSH target string: "user@host" or just "host"
pub fn ssh_target(remote: &RemoteConfig) -> String {
    match &remote.user {
        Some(user) => format!("{}@{}", user, remote.host),
        None => remote.host.clone(),
    }
}

/// Get the remote projects directory (or default ~/.claude/projects)
pub fn remote_projects_dir(remote: &RemoteConfig) -> &str {
    remote
        .projects_dir
        .as_deref()
        .unwrap_or("~/.claude/projects")
}

// =============================================================================
// Sync Operations
// =============================================================================

/// Sync a remote's sessions to local cache using rsync
///
/// Uses rsync with:
/// - `-a`: Archive mode (preserves timestamps, permissions)
/// - `-z`: Compression for transfer
/// - `--delete`: Remove files deleted on remote
/// - `-e ssh`: Use SSH transport
pub fn sync_remote(
    remote_name: &str,
    remote: &RemoteConfig,
    settings: &Settings,
) -> Result<SyncResult> {
    let cache_dir = get_remote_cache_dir(settings, remote_name)?;

    // Ensure cache directory exists
    fs::create_dir_all(&cache_dir)
        .with_context(|| format!("Failed to create cache dir: {}", cache_dir.display()))?;

    let target = ssh_target(remote);
    let remote_path = remote_projects_dir(remote);

    // rsync source: user@host:~/.claude/projects/
    // The trailing slash is important - it copies contents, not the directory itself
    let source = format!("{}:{}/", target, remote_path);
    let dest = format!("{}/", cache_dir.display());

    let start = std::time::Instant::now();

    let output = Command::new("rsync")
        .args([
            "-az",
            "--delete",
            "-e",
            "ssh",
            "--exclude",
            "*.lock", // Don't sync lock files
            &source,
            &dest,
        ])
        .output()
        .context("Failed to execute rsync")?;

    let duration = start.elapsed();

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "rsync failed for remote '{}': {}",
            remote_name,
            stderr.trim()
        );
    }

    // Update last sync timestamp
    update_last_sync(&cache_dir)?;

    Ok(SyncResult {
        remote_name: remote_name.to_string(),
        duration,
    })
}

/// Result of a sync operation
#[derive(Debug)]
pub struct SyncResult {
    pub remote_name: String,
    pub duration: Duration,
}

// =============================================================================
// Staleness Tracking
// =============================================================================

const LAST_SYNC_FILE: &str = ".last_sync";

/// Check if a remote's cache is stale (older than threshold)
pub fn is_stale(remote_name: &str, settings: &Settings) -> Result<bool> {
    let cache_dir = get_remote_cache_dir(settings, remote_name)?;
    let last_sync_path = cache_dir.join(LAST_SYNC_FILE);

    if !last_sync_path.exists() {
        return Ok(true); // Never synced = stale
    }

    let last_sync = get_last_sync_time(&last_sync_path)?;
    let now = SystemTime::now();
    let age = now.duration_since(last_sync).unwrap_or(Duration::MAX);

    Ok(age.as_secs() > settings.stale_threshold)
}

/// Get the last sync time for a remote
#[allow(dead_code)] // Useful for future staleness display
pub fn get_last_sync(remote_name: &str, settings: &Settings) -> Result<Option<SystemTime>> {
    let cache_dir = get_remote_cache_dir(settings, remote_name)?;
    let last_sync_path = cache_dir.join(LAST_SYNC_FILE);

    if !last_sync_path.exists() {
        return Ok(None);
    }

    get_last_sync_time(&last_sync_path).map(Some)
}

/// Read the timestamp from .last_sync file
fn get_last_sync_time(path: &PathBuf) -> Result<SystemTime> {
    let content = fs::read_to_string(path).context("Failed to read .last_sync file")?;
    let secs: u64 = content
        .trim()
        .parse()
        .context("Invalid timestamp in .last_sync")?;
    Ok(UNIX_EPOCH + Duration::from_secs(secs))
}

/// Update the .last_sync timestamp file
fn update_last_sync(cache_dir: &Path) -> Result<()> {
    let last_sync_path = cache_dir.join(LAST_SYNC_FILE);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    fs::write(&last_sync_path, now.to_string()).context("Failed to update .last_sync file")?;
    Ok(())
}

/// Sync remotes, optionally checking staleness first.
fn sync_remotes(config: &Config, check_staleness: bool) -> Result<Vec<SyncResult>> {
    let mut results = Vec::new();

    for (name, remote) in &config.remotes {
        // Skip non-stale remotes if checking staleness
        if check_staleness && !is_stale(name, &config.settings)? {
            continue;
        }

        match sync_remote(name, remote, &config.settings) {
            Ok(result) => results.push(result),
            Err(e) => eprintln!("Warning: Failed to sync '{}': {}", name, e),
        }
    }

    Ok(results)
}

/// Sync remotes if they are stale
///
/// Returns the list of remotes that were synced
pub fn sync_if_stale(config: &Config) -> Result<Vec<SyncResult>> {
    sync_remotes(config, true)
}

/// Sync all configured remotes regardless of staleness
pub fn sync_all(config: &Config) -> Result<Vec<SyncResult>> {
    sync_remotes(config, false)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_target_with_user() {
        let remote = RemoteConfig {
            host: "192.168.1.100".to_string(),
            user: Some("ec2-user".to_string()),
            projects_dir: None,
        };
        assert_eq!(ssh_target(&remote), "ec2-user@192.168.1.100");
    }

    #[test]
    fn ssh_target_without_user() {
        let remote = RemoteConfig {
            host: "devbox".to_string(),
            user: None,
            projects_dir: None,
        };
        assert_eq!(ssh_target(&remote), "devbox");
    }

    #[test]
    fn remote_projects_dir_default() {
        let remote = RemoteConfig {
            host: "test".to_string(),
            user: None,
            projects_dir: None,
        };
        assert_eq!(remote_projects_dir(&remote), "~/.claude/projects");
    }

    #[test]
    fn remote_projects_dir_custom() {
        let remote = RemoteConfig {
            host: "test".to_string(),
            user: None,
            projects_dir: Some("/home/custom/.claude/projects".to_string()),
        };
        assert_eq!(
            remote_projects_dir(&remote),
            "/home/custom/.claude/projects"
        );
    }

    #[test]
    fn parse_empty_config() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.remotes.is_empty());
        assert_eq!(config.settings.stale_threshold, 3600);
    }

    #[test]
    fn parse_full_config() {
        let toml = r#"
[remotes.devbox]
host = "devbox"

[remotes.workstation]
host = "192.168.1.100"
user = "ec2-user"
projects_dir = "/home/ian/.claude/projects"

[settings]
cache_dir = "~/.cache/my-cache"
stale_threshold = 7200
"#;
        let config: Config = toml::from_str(toml).unwrap();

        assert_eq!(config.remotes.len(), 2);
        assert_eq!(config.remotes["devbox"].host, "devbox");
        assert!(config.remotes["devbox"].user.is_none());

        assert_eq!(config.remotes["workstation"].host, "192.168.1.100");
        assert_eq!(
            config.remotes["workstation"].user,
            Some("ec2-user".to_string())
        );

        assert_eq!(config.settings.cache_dir, "~/.cache/my-cache");
        assert_eq!(config.settings.stale_threshold, 7200);
    }
}
