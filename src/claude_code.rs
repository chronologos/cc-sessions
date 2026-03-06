//! Claude Code session storage format handling.
//!
//! This module contains all code that knows about Claude Code's specific
//! file formats and directory structure. If Claude Code changes its storage
//! format, changes should be isolated to this module.
//!
//! ## Storage Structure
//!
//! ```text
//! ~/.claude/projects/
//!   -Users-you-project-a/
//!     abc12345-1234-1234-1234-123456789abc.jsonl   # Session transcript (UUID filename)
//!     def45678-5678-5678-5678-567890123def.jsonl
//!   -Users-you-project-b/
//!     ghi78901-9012-9012-9012-901234567890.jsonl
//! ```
//!
//! Sessions are discovered by scanning for `.jsonl` files with valid UUID filenames.
//! All metadata is extracted via a single full-file pass per session.

use crate::message_classification::{counts_as_turn, is_first_prompt_candidate};
use crate::session::{Session, SessionSource};
use anyhow::{Context, Result};
use rayon::prelude::*;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

/// Failure details for a single session discovery source.
#[derive(Debug)]
pub struct DiscoveryFailure {
    pub source_name: String,
    pub reason: String,
}

/// Aggregated discovery outcome across local + remote sources.
#[derive(Debug, Default)]
pub struct DiscoverySummary {
    pub sessions: Vec<Session>,
    pub failures: Vec<DiscoveryFailure>,
}

impl DiscoverySummary {
    pub fn failure_count(&self) -> usize {
        self.failures.len()
    }
}

// =============================================================================
// Path Discovery
// =============================================================================

pub fn get_claude_projects_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not find home directory")?;
    Ok(home.join(".claude").join("projects"))
}

/// Check if a source should be included based on the filter.
fn should_include_source(remote_filter: Option<&str>, source_name: &str) -> bool {
    match remote_filter {
        None => true,
        Some(filter) => source_name == filter,
    }
}

/// Find all sessions from local and cached remotes with source-level failures.
pub fn find_all_sessions_with_summary(
    config: &crate::remote::Config,
    remote_filter: Option<&str>,
) -> Result<DiscoverySummary> {
    use crate::remote;

    let mut summary = DiscoverySummary::default();

    // Load local sessions
    if should_include_source(remote_filter, "local") {
        let local_dir = get_claude_projects_dir()?;
        if local_dir.exists() {
            summary.sessions.extend(find_sessions(&local_dir)?);
        }
    }

    // Load cached remote sessions
    for (name, remote_config) in &config.remotes {
        if !should_include_source(remote_filter, name) {
            continue;
        }
        // "local" filter should not include remotes
        if remote_filter == Some("local") {
            continue;
        }

        let cache_dir = match remote::get_remote_cache_dir(&config.settings, name) {
            Ok(dir) if dir.exists() => dir,
            _ => continue,
        };

        let source = SessionSource::Remote {
            name: name.clone(),
            host: remote_config.host.clone(),
            user: remote_config.user.clone(),
        };

        match find_sessions_with_source(&cache_dir, source) {
            Ok(sessions) => summary.sessions.extend(sessions),
            Err(e) => summary.failures.push(DiscoveryFailure {
                source_name: name.clone(),
                reason: e.to_string(),
            }),
        }
    }

    summary.sessions.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(summary)
}

// =============================================================================
// Session Loading
// =============================================================================

/// Find all sessions by scanning .jsonl files directly.
///
/// This replaces the previous two-phase approach (index + orphan) with a single
/// unified scan. All metadata is extracted directly from file contents.
pub fn find_sessions(projects_dir: &Path) -> Result<Vec<Session>> {
    find_sessions_with_source(projects_dir, SessionSource::Local)
}

/// Find sessions with a specific source tag.
///
/// Used by both local discovery and remote cache discovery.
pub fn find_sessions_with_source(
    projects_dir: &Path,
    source: SessionSource,
) -> Result<Vec<Session>> {
    // Find all .jsonl files with valid UUID filenames
    let jsonl_files: Vec<PathBuf> = WalkDir::new(projects_dir)
        .min_depth(2)
        .max_depth(2)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| is_valid_session_file(e.path()))
        .map(|e| e.path().to_path_buf())
        .collect();

    // Process files in parallel, extracting metadata from each
    let mut sessions: Vec<Session> = jsonl_files
        .par_iter()
        .filter_map(|filepath| {
            let parent_dir = filepath
                .parent()?
                .file_name()?
                .to_string_lossy()
                .to_string();
            extract_session_metadata(filepath, &parent_dir, source.clone())
        })
        .collect();

    sessions.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(sessions)
}

/// Check if a string is a valid UUID (8-4-4-4-12 format with hex chars)
fn is_valid_session_uuid(s: &str) -> bool {
    const SEGMENT_LENGTHS: [usize; 5] = [8, 4, 4, 4, 12];

    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts
            .iter()
            .zip(SEGMENT_LENGTHS)
            .all(|(part, len)| part.len() == len)
        && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Check if a path is a valid session file (UUID-named .jsonl).
///
/// UUID validation alone excludes subagent transcripts: those are named
/// `agent-{hex}.jsonl` and live in `{session}/subagents/` (depth 3, which
/// the WalkDir depth-2 cap doesn't traverse anyway).
fn is_valid_session_file(path: &Path) -> bool {
    path.extension() == Some(std::ffi::OsStr::new("jsonl"))
        && path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(is_valid_session_uuid)
            .unwrap_or(false)
}

/// Extract all session metadata from a .jsonl file in a single pass.
fn extract_session_metadata(
    filepath: &Path,
    parent_dir_name: &str,
    source: SessionSource,
) -> Option<Session> {
    let id = filepath.file_stem()?.to_string_lossy().to_string();

    let metadata = fs::metadata(filepath).ok()?;
    let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
    let created = metadata.created().unwrap_or(modified);

    let scan = scan_session_file(filepath);

    if scan.skip {
        return None;
    }

    // Skip "empty" sessions that have no user content
    if scan.project_path.is_empty() && scan.first_prompt.is_none() && scan.summary.is_none() {
        return None;
    }

    let project = extract_project_name(&scan.project_path, parent_dir_name);

    Some(Session {
        id,
        project,
        project_path: scan.project_path,
        filepath: filepath.to_path_buf(),
        created,
        modified,
        first_message: scan.first_prompt,
        summary: scan.summary,
        name: scan.custom_title,
        tag: scan.tag,
        turn_count: scan.turn_count,
        source,
        forked_from: scan.forked_from,
        search_text_lower: scan.search_text_lower,
    })
}

/// Output of single-pass scan over a session file.
#[derive(Default)]
struct SessionScan {
    project_path: String,
    first_prompt: Option<String>,
    forked_from: Option<String>,
    turn_count: usize,
    search_text_lower: String,
    summary: Option<String>,
    custom_title: Option<String>,
    tag: Option<String>,
    /// Session should be excluded from the picker (sidechain or swarm-teammate).
    skip: bool,
}

/// Scan a session file once to collect all metadata, turn count, and search text.
///
/// Single file open, single pass. Summary and custom-title entries can appear
/// anywhere (compaction, /rename mid-session); last well-formed occurrence wins.
fn scan_session_file(filepath: &Path) -> SessionScan {
    let mut scan = SessionScan::default();
    let mut search_chunks = Vec::new();

    let Ok(file) = File::open(filepath) else {
        return scan;
    };

    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let entry: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Sidechain (subagent) and teammate (swarm) sessions can both land in
        // the main project dir as UUID-named files. Bail early — they can be
        // large and we're discarding them anyway.
        if entry.get("isSidechain").and_then(|v| v.as_bool()) == Some(true)
            || entry.get("teamName").and_then(|v| v.as_str()).is_some()
        {
            scan.skip = true;
            return scan;
        }

        let entry_type = entry.get("type").and_then(|v| v.as_str());

        match entry_type {
            Some("summary") => {
                if let Some(s) = entry.get("summary").and_then(|v| v.as_str()) {
                    scan.summary = Some(s.to_string());
                }
                continue;
            }
            Some("custom-title") => {
                if let Some(t) = entry.get("customTitle").and_then(|v| v.as_str()) {
                    scan.custom_title = Some(t.to_string());
                }
                continue;
            }
            Some("tag") => {
                // Empty tag = explicit removal (/tag followed by same name clears it)
                scan.tag = entry
                    .get("tag")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from);
                continue;
            }
            _ => {}
        }

        if scan.project_path.is_empty()
            && let Some(cwd) = entry.get("cwd").and_then(|v| v.as_str())
        {
            scan.project_path = cwd.to_string();
        }

        if scan.forked_from.is_none()
            && let Some(parent_id) = entry
                .get("forkedFrom")
                .and_then(|f| f.get("sessionId"))
                .and_then(|v| v.as_str())
        {
            scan.forked_from = Some(parent_id.to_string());
        }

        // isMeta/isCompactSummary mark synthetic user messages (attachment
        // context, post-compaction summaries). They carry cwd/forkedFrom like
        // any entry, but their content is never real user input.
        if entry.get("isMeta").and_then(|v| v.as_bool()) == Some(true)
            || entry.get("isCompactSummary").and_then(|v| v.as_bool()) == Some(true)
        {
            continue;
        }

        if entry_type == Some("user")
            && let Some(content) = entry.get("message").and_then(|m| m.get("content"))
            && let Some(text) = extract_text_content(content)
        {
            if scan.first_prompt.is_none() && is_first_prompt_candidate(&text) {
                scan.first_prompt = Some(crate::normalize_summary(&text, 50));
            }
            if counts_as_turn(&text) {
                scan.turn_count += 1;
            }
        }

        if matches!(entry_type, Some("user") | Some("assistant"))
            && let Some(text) = extract_message_text_for_search(&entry)
            && !text.is_empty()
        {
            search_chunks.push(text);
        }
    }

    scan.search_text_lower = search_chunks.join("\n").to_lowercase();
    scan
}

/// Extract first text from message content (string or array of content blocks).
/// Takes the `content` value directly (caller navigates to it).
pub fn extract_text_content(content: &serde_json::Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }

    content.as_array()?.iter().find_map(|c| {
        if c.get("type")?.as_str()? == "text" {
            Some(c.get("text")?.as_str()?.to_string())
        } else {
            None
        }
    })
}

// =============================================================================
// Transcript Search
// =============================================================================

/// Extract text from message content for search purposes.
/// Unlike `extract_text_content` (which returns the first text block),
/// this joins ALL text blocks — a search match could be in any block.
fn extract_message_text_for_search(entry: &serde_json::Value) -> Option<String> {
    let content = entry.get("message")?.get("content")?;

    // Content can be a string or array of content blocks
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }

    // For arrays, collect all text blocks
    if let Some(arr) = content.as_array() {
        let texts: Vec<String> = arr
            .iter()
            .filter_map(|c| {
                if c.get("type")?.as_str()? == "text" {
                    Some(c.get("text")?.as_str()?.to_string())
                } else {
                    None
                }
            })
            .collect();
        if !texts.is_empty() {
            return Some(texts.join(" "));
        }
    }

    None
}

// =============================================================================
// Helper Functions
// =============================================================================

/// Extract project name from path or directory name fallback
///
/// Claude Code uses directory names like `-Users-alice-Documents-repos-foo`
fn extract_project_name(project_path: &str, fallback_dir: &str) -> String {
    // Prefer cwd-based project name
    if !project_path.is_empty() {
        return project_path
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown")
            .to_string();
    }

    // Parse directory name: "-Users-alice-Documents-repos-foo" -> "foo"
    // Strip "-Users-<username>-" prefix dynamically
    let stripped = fallback_dir
        .strip_prefix("-Users-")
        .and_then(|s| s.split_once('-').map(|(_, rest)| rest))
        .unwrap_or(fallback_dir);

    const PATH_PREFIXES: &[&str] = &[
        "Documents-repos-",
        "Documents-",
        "repos-",
        "third-party-repos-",
    ];

    PATH_PREFIXES
        .iter()
        .find_map(|p| stripped.strip_prefix(p))
        .unwrap_or(stripped)
        .to_string()
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Write JSONL content to a tempfile and return (guard, path).
    /// The TempDir guard cleans up on drop — no manual remove_dir_all needed.
    fn scan_fixture(content: &str) -> (TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.jsonl");
        fs::write(&path, content).unwrap();
        (tmp, path)
    }

    /// Create a temp projects dir with a single UUID-named session file.
    /// Returns (guard, projects_root) for passing to find_sessions().
    fn project_fixture(dir_name: &str, uuid: &str, content: &str) -> (TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let project_dir = root.join(dir_name);
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(project_dir.join(format!("{}.jsonl", uuid)), content).unwrap();
        (tmp, root)
    }

    // =========================================================================
    // UUID validation - Critical for filtering non-session files
    // =========================================================================

    #[test]
    fn uuid_validation_valid_uuids() {
        assert!(is_valid_session_uuid(
            "12345678-1234-1234-1234-123456789abc"
        ));
        assert!(is_valid_session_uuid(
            "abcdef00-abcd-abcd-abcd-abcdef123456"
        ));
        assert!(is_valid_session_uuid(
            "ABCDEF00-ABCD-ABCD-ABCD-ABCDEF123456"
        ));
    }

    #[test]
    fn uuid_validation_invalid_formats() {
        // Wrong segment lengths
        assert!(!is_valid_session_uuid(
            "1234567-1234-1234-1234-123456789abc"
        )); // 7 chars
        assert!(!is_valid_session_uuid(
            "12345678-123-1234-1234-123456789abc"
        )); // 3 chars
        assert!(!is_valid_session_uuid(
            "12345678-1234-1234-1234-123456789ab"
        )); // 11 chars

        // Wrong number of segments
        assert!(!is_valid_session_uuid("12345678-1234-1234-123456789abc"));
        assert!(!is_valid_session_uuid(
            "12345678-1234-1234-1234-1234-123456789abc"
        ));

        // Non-hex characters
        assert!(!is_valid_session_uuid(
            "1234567g-1234-1234-1234-123456789abc"
        ));

        // Old-style names
        assert!(!is_valid_session_uuid("agent-12345"));
        assert!(!is_valid_session_uuid("black-knight-battle"));
        assert!(!is_valid_session_uuid("sessions-index"));
    }

    // =========================================================================
    // Project name extraction
    // =========================================================================

    #[test]
    fn extract_project_name_from_path() {
        assert_eq!(
            extract_project_name("/Users/foo/my-project", "ignored"),
            "my-project"
        );
        assert_eq!(
            extract_project_name("/home/user/code/bike-power", "ignored"),
            "bike-power"
        );
    }

    #[test]
    fn extract_project_name_from_dir_fallback() {
        assert_eq!(
            extract_project_name("", "-Users-alice-Documents-repos-cc-session"),
            "cc-session"
        );
        assert_eq!(
            extract_project_name("", "-Users-alice-third-party-repos-foo"),
            "foo"
        );
        assert_eq!(
            extract_project_name("", "-Users-someone-Documents-bar"),
            "bar"
        );
    }

    // =========================================================================
    // Integration tests with fake data
    // =========================================================================

    /// Helper to generate a valid UUID for test files
    fn test_uuid(n: u8) -> String {
        format!(
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            n as u32, n as u16, n as u16, n as u16, n as u64
        )
    }

    #[test]
    fn find_sessions_with_uuid_files() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("-Users-sirrobin-holy-grail");
        fs::create_dir_all(&project_dir).unwrap();

        let uuid1 = test_uuid(1);
        let uuid2 = test_uuid(2);

        fs::write(
            project_dir.join(format!("{}.jsonl", uuid1)),
            r#"{"type":"user","message":{"role":"user","content":"Tis but a scratch"},"cwd":"/Users/sirrobin/holy-grail"}"#,
        )
        .unwrap();
        fs::write(
            project_dir.join(format!("{}.jsonl", uuid2)),
            r#"{"type":"user","message":{"role":"user","content":"Run away!"},"cwd":"/Users/sirrobin/holy-grail"}
{"type":"summary","summary":"Deploying Holy Hand Grenade of Antioch"}"#,
        )
        .unwrap();

        let sessions = find_sessions(tmp.path()).unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].project, "holy-grail");

        let with_summary = sessions.iter().find(|s| s.summary.is_some()).unwrap();
        assert_eq!(
            with_summary.summary,
            Some("Deploying Holy Hand Grenade of Antioch".to_string())
        );
    }

    #[test]
    fn find_sessions_filters_non_uuid_files() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("-Users-arthur-camelot");
        fs::create_dir_all(&project_dir).unwrap();

        let valid_uuid = test_uuid(42);
        fs::write(
            project_dir.join(format!("{}.jsonl", valid_uuid)),
            r#"{"type":"user","message":{"role":"user","content":"What is your quest?"},"cwd":"/Users/arthur/camelot"}"#,
        )
        .unwrap();
        fs::write(
            project_dir.join("agent-12345.jsonl"),
            r#"{"type":"user","message":"I am an agent"}"#,
        )
        .unwrap();
        fs::write(
            project_dir.join("black-knight.jsonl"),
            r#"{"type":"user","message":"None shall pass"}"#,
        )
        .unwrap();

        let sessions = find_sessions(tmp.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, valid_uuid);
    }

    #[test]
    fn find_sessions_extracts_custom_title() {
        let (_tmp, root) = project_fixture(
            "-Users-brian-life",
            &test_uuid(99),
            r#"{"type":"user","message":{"role":"user","content":"Always look on the bright side"},"cwd":"/Users/brian/life"}
{"type":"assistant","message":"Indeed!"}
{"type":"custom-title","customTitle":"Important Session","sessionId":"00000063-0063-0063-0063-000000000063"}"#,
        );

        let sessions = find_sessions(&root).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, Some("Important Session".to_string()));
    }

    #[test]
    fn find_sessions_handles_empty_sessions() {
        let (_tmp, root) = project_fixture("-Users-spam-eggs", &test_uuid(7), r#"{"type":"init"}"#);
        assert_eq!(find_sessions(&root).unwrap().len(), 0);
    }

    // =========================================================================
    // Fork detection tests
    // =========================================================================

    #[test]
    fn find_sessions_extracts_forked_from() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("-Users-patsy-camelot");
        fs::create_dir_all(&project_dir).unwrap();

        let parent_uuid = test_uuid(10);
        let fork_uuid = test_uuid(11);

        fs::write(
            project_dir.join(format!("{}.jsonl", parent_uuid)),
            r#"{"type":"user","message":{"role":"user","content":"What is your quest?"},"cwd":"/Users/patsy/camelot","sessionId":"00000010-0010-0010-0010-00000000000a"}"#,
        )
        .unwrap();
        fs::write(
            project_dir.join(format!("{}.jsonl", fork_uuid)),
            r#"{"type":"user","message":{"role":"user","content":"What is your quest?"},"cwd":"/Users/patsy/camelot","sessionId":"0000000b-000b-000b-000b-00000000000b","forkedFrom":{"sessionId":"00000010-0010-0010-0010-00000000000a","messageUuid":"abc123"}}"#,
        )
        .unwrap();

        let sessions = find_sessions(tmp.path()).unwrap();
        assert_eq!(sessions.len(), 2);

        let parent = sessions.iter().find(|s| s.id == parent_uuid).unwrap();
        let fork = sessions.iter().find(|s| s.id == fork_uuid).unwrap();
        assert_eq!(parent.forked_from, None);
        assert_eq!(
            fork.forked_from,
            Some("00000010-0010-0010-0010-00000000000a".to_string())
        );
    }

    #[test]
    fn find_sessions_multiple_forks_same_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("-Users-tim-enchanter");
        fs::create_dir_all(&project_dir).unwrap();

        let (p, f1, f2) = (test_uuid(20), test_uuid(21), test_uuid(22));
        fs::write(
            project_dir.join(format!("{}.jsonl", p)),
            r#"{"type":"user","message":{"role":"user","content":"What manner of man are you?"},"cwd":"/Users/tim/enchanter"}"#,
        )
        .unwrap();
        fs::write(
            project_dir.join(format!("{}.jsonl", f1)),
            r#"{"type":"user","message":{"role":"user","content":"What manner of man are you?"},"cwd":"/Users/tim/enchanter","forkedFrom":{"sessionId":"00000014-0014-0014-0014-000000000014","messageUuid":"msg1"}}"#,
        )
        .unwrap();
        fs::write(
            project_dir.join(format!("{}.jsonl", f2)),
            r#"{"type":"user","message":{"role":"user","content":"What manner of man are you?"},"cwd":"/Users/tim/enchanter","forkedFrom":{"sessionId":"00000014-0014-0014-0014-000000000014","messageUuid":"msg2"}}"#,
        )
        .unwrap();

        let sessions = find_sessions(tmp.path()).unwrap();
        assert_eq!(sessions.len(), 3);

        let fork1 = sessions.iter().find(|s| s.id == f1).unwrap();
        let fork2 = sessions.iter().find(|s| s.id == f2).unwrap();
        assert_eq!(fork1.forked_from, fork2.forked_from);
        assert_eq!(
            fork1.forked_from,
            Some("00000014-0014-0014-0014-000000000014".to_string())
        );
    }

    #[test]
    fn scan_prefers_first_forked_from() {
        let (_tmp, path) = scan_fixture(
            r#"{"type":"user","message":{"role":"user","content":"hello"},"forkedFrom":{"sessionId":"parent-1","messageUuid":"m1"}}
{"type":"assistant","message":"hi"}
{"type":"user","message":{"role":"user","content":"later"},"forkedFrom":{"sessionId":"parent-2","messageUuid":"m2"}}"#,
        );
        assert_eq!(
            scan_session_file(&path).forked_from,
            Some("parent-1".to_string())
        );
    }

    #[test]
    fn scan_extracts_forked_from_on_later_line() {
        let (_tmp, path) = scan_fixture(
            r#"{"type":"progress","data":"starting"}
{"type":"progress","cwd":"/Users/test/project","data":"hook"}
{"type":"user","message":{"role":"user","content":"hello"},"forkedFrom":{"sessionId":"parent-session-id","messageUuid":"msg1"}}
{"type":"assistant","message":"hi"}"#,
        );
        let scan = scan_session_file(&path);
        assert_eq!(scan.project_path, "/Users/test/project");
        assert_eq!(scan.forked_from, Some("parent-session-id".to_string()));
        assert_eq!(scan.first_prompt, Some("hello".to_string()));
    }

    // =========================================================================
    // Turn counting - only real user messages, not system content
    // =========================================================================

    #[test]
    fn count_turns_real_user_messages() {
        let (_tmp, path) = scan_fixture(
            r#"{"type":"user","message":{"role":"user","content":"Hello, how are you?"}}
{"type":"assistant","message":{"role":"assistant","content":"I'm good!"}}
{"type":"user","message":{"role":"user","content":"What is Rust?"}}
{"type":"assistant","message":{"role":"assistant","content":"A programming language."}}"#,
        );
        assert_eq!(scan_session_file(&path).turn_count, 2);
    }

    #[test]
    fn count_turns_excludes_system_content() {
        let (_tmp, path) = scan_fixture(
            r#"{"type":"user","message":{"role":"user","content":"<command-message>init</command-message>"}}
{"type":"user","message":{"role":"user","content":"Real message here"}}
{"type":"user","message":{"role":"user","content":"<local-command-stdout>output</local-command-stdout>"}}
{"type":"user","message":{"role":"user","content":"/help"}}
{"type":"user","message":{"role":"user","content":"[some bracketed thing]"}}
{"type":"user","message":{"role":"user","content":"Another real message"}}"#,
        );
        assert_eq!(scan_session_file(&path).turn_count, 2);
    }

    #[test]
    fn count_turns_handles_content_blocks() {
        let (_tmp, path) = scan_fixture(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Hello from blocks"}]}}
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"<command-name>/init</command-name>"}]}}
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Real question?"}]}}"#,
        );
        assert_eq!(scan_session_file(&path).turn_count, 2);
    }

    #[test]
    fn count_turns_empty_file() {
        let (_tmp, path) = scan_fixture("");
        assert_eq!(scan_session_file(&path).turn_count, 0);
    }

    #[test]
    fn first_prompt_and_turn_count_current_filter_behavior() {
        let (_tmp, path) = scan_fixture(
            r#"{"type":"user","message":{"role":"user","content":"[tool output that is not Request]"},"cwd":"/Users/test/project"}
{"type":"user","message":{"role":"user","content":"Real user question"}}"#,
        );
        let scan = scan_session_file(&path);
        // first prompt excludes [Request... but not all bracketed text
        assert_eq!(
            scan.first_prompt,
            Some("[tool output that is not Request]".to_string())
        );
        // turn counting excludes all bracketed entries
        assert_eq!(scan.turn_count, 1);
    }

    #[test]
    fn discovery_summary_tracks_source_failures() {
        let summary = DiscoverySummary {
            sessions: Vec::new(),
            failures: vec![DiscoveryFailure {
                source_name: "devbox".to_string(),
                reason: "cache unreadable".to_string(),
            }],
        };
        assert_eq!(summary.failure_count(), 1);
        assert_eq!(summary.failures.len(), 1);
    }

    #[test]
    fn classify_user_text_for_metrics_table() {
        use crate::message_classification::{MessageKind, classify_user_text_for_metrics};
        let cases = [
            ("normal user text", MessageKind::UserContent),
            ("/help", MessageKind::SlashCommand),
            (
                "<command-message>init</command-message>",
                MessageKind::SystemTag,
            ),
            ("[local command output]", MessageKind::BracketedOutput),
            ("", MessageKind::Empty),
        ];
        for (text, expected) in cases {
            assert_eq!(classify_user_text_for_metrics(text), expected);
        }
    }

    #[test]
    fn scan_once_produces_equivalent_session_metadata() {
        let (_tmp, path) = scan_fixture(
            r#"{"type":"user","message":{"role":"user","content":"Real prompt"},"cwd":"/Users/test/project"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"assistant reply"}]}}
{"type":"user","message":{"role":"user","content":"/help"}}
{"type":"user","message":{"role":"user","content":"Second real prompt"}}"#,
        );
        let scan = scan_session_file(&path);
        assert_eq!(scan.project_path, "/Users/test/project");
        assert_eq!(scan.first_prompt, Some("Real prompt".to_string()));
        assert_eq!(scan.turn_count, 2);
    }

    #[test]
    fn scan_filters_sidechain_sessions() {
        let (_tmp, root) = project_fixture(
            "-Users-test-proj",
            &test_uuid(50),
            r#"{"type":"user","message":{"role":"user","content":"agent work"},"cwd":"/Users/test/proj","isSidechain":true}"#,
        );
        let session_path = root
            .join("-Users-test-proj")
            .join(format!("{}.jsonl", test_uuid(50)));
        assert!(scan_session_file(&session_path).skip);
        assert_eq!(find_sessions(&root).unwrap().len(), 0);
    }

    #[test]
    fn scan_ignores_sidechain_false() {
        let (_tmp, path) = scan_fixture(
            r#"{"type":"user","message":{"role":"user","content":"hi"},"cwd":"/tmp","isSidechain":false}"#,
        );
        let scan = scan_session_file(&path);
        assert!(!scan.skip);
        assert_eq!(scan.project_path, "/tmp");
    }

    #[test]
    fn scan_filters_teammate_sessions() {
        let (_tmp, root) = project_fixture(
            "-Users-test-proj",
            &test_uuid(51),
            r#"{"type":"user","message":{"role":"user","content":"swarm work"},"cwd":"/tmp","teamName":"my-team","isSidechain":false}"#,
        );
        let session_path = root
            .join("-Users-test-proj")
            .join(format!("{}.jsonl", test_uuid(51)));
        assert!(scan_session_file(&session_path).skip);
        assert_eq!(find_sessions(&root).unwrap().len(), 0);
    }

    #[test]
    fn scan_skips_meta_entries_for_first_prompt_and_turns() {
        let (_tmp, path) = scan_fixture(
            r#"{"type":"user","message":{"role":"user","content":"synthetic attachment context"},"cwd":"/tmp","isMeta":true}
{"type":"user","message":{"role":"user","content":"real user prompt"}}"#,
        );
        let scan = scan_session_file(&path);
        assert_eq!(scan.project_path, "/tmp");
        assert_eq!(scan.first_prompt, Some("real user prompt".to_string()));
        assert_eq!(scan.turn_count, 1);
        assert!(!scan.search_text_lower.contains("synthetic"));
    }

    #[test]
    fn scan_skips_compact_summary_entries() {
        let (_tmp, path) = scan_fixture(
            r#"{"type":"user","message":{"role":"user","content":"This session covers X and Y"},"cwd":"/tmp","isCompactSummary":true}
{"type":"user","message":{"role":"user","content":"actual question"}}"#,
        );
        let scan = scan_session_file(&path);
        assert_eq!(scan.first_prompt, Some("actual question".to_string()));
        assert_eq!(scan.turn_count, 1);
    }

    #[test]
    fn scan_takes_last_summary() {
        let (_tmp, path) = scan_fixture(
            r#"{"type":"summary","summary":"Early compaction"}
{"type":"user","message":{"role":"user","content":"more work"},"cwd":"/tmp"}
{"type":"summary","summary":"Final summary"}"#,
        );
        assert_eq!(
            scan_session_file(&path).summary,
            Some("Final summary".to_string())
        );
    }

    #[test]
    fn scan_keeps_valid_summary_when_later_entry_malformed() {
        let (_tmp, path) = scan_fixture(
            r#"{"type":"summary","summary":"Valid"}
{"type":"summary"}"#,
        );
        assert_eq!(scan_session_file(&path).summary, Some("Valid".to_string()));
    }

    #[test]
    fn scan_takes_last_custom_title() {
        let (_tmp, path) = scan_fixture(
            r#"{"type":"user","message":{"role":"user","content":"hello"},"cwd":"/tmp"}
{"type":"custom-title","customTitle":"Old Name","sessionId":"x"}
{"type":"assistant","message":{"role":"assistant","content":"hi"}}
{"type":"custom-title","customTitle":"New Name","sessionId":"x"}"#,
        );
        assert_eq!(
            scan_session_file(&path).custom_title,
            Some("New Name".to_string())
        );
    }

    #[test]
    fn scan_search_text_includes_user_and_assistant_text() {
        let (_tmp, path) = scan_fixture(
            r#"{"type":"user","message":{"role":"user","content":"API status"}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Service healthy"}]}}
{"type":"summary","summary":"ignored summary"}"#,
        );
        let text = scan_session_file(&path).search_text_lower;
        assert!(text.contains("api status"));
        assert!(text.contains("service healthy"));
    }

    #[test]
    fn scan_tag_empty_string_clears_previous() {
        let (_tmp, path) = scan_fixture(
            r#"{"type":"tag","tag":"important","sessionId":"x"}
{"type":"user","message":{"role":"user","content":"work"},"cwd":"/tmp"}
{"type":"tag","tag":"","sessionId":"x"}"#,
        );
        assert_eq!(scan_session_file(&path).tag, None);
    }

    #[test]
    fn scan_tag_takes_last_non_empty() {
        let (_tmp, path) = scan_fixture(
            r#"{"type":"tag","tag":"old","sessionId":"x"}
{"type":"tag","tag":"new","sessionId":"x"}"#,
        );
        assert_eq!(scan_session_file(&path).tag, Some("new".to_string()));
    }
}
