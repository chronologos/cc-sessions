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
//! Metadata is extracted directly from file contents (head + tail) rather than
//! relying on `sessions-index.json` which is often stale.

use crate::{Session, SessionSource};
use anyhow::{Context, Result};
use grep_regex::RegexMatcher;
use grep_searcher::Searcher;
use grep_searcher::sinks::UTF8;
use rayon::prelude::*;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

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

/// Find all sessions from local and cached remotes.
///
/// Combines:
/// - Local sessions from ~/.claude/projects
/// - Cached remote sessions from each configured remote
///
/// Sessions are sorted by modified time (most recent first).
pub fn find_all_sessions(
    config: &crate::remote::Config,
    remote_filter: Option<&str>,
) -> Result<Vec<crate::Session>> {
    use crate::remote;

    let mut all_sessions = Vec::new();

    // Load local sessions
    if should_include_source(remote_filter, "local") {
        let local_dir = get_claude_projects_dir()?;
        if local_dir.exists() {
            all_sessions.extend(find_sessions(&local_dir)?);
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
            Ok(sessions) => all_sessions.extend(sessions),
            Err(e) => eprintln!("Warning: Failed to load sessions from '{}': {}", name, e),
        }
    }

    all_sessions.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(all_sessions)
}

/// Search all sessions (local and cached remotes) for a pattern.
pub fn search_all_sessions(
    config: &crate::remote::Config,
    pattern: &str,
    remote_filter: Option<&str>,
) -> Result<Vec<crate::Session>> {
    use crate::remote;

    let mut all_sessions = Vec::new();

    // Search local sessions
    if should_include_source(remote_filter, "local") {
        let local_dir = get_claude_projects_dir()?;
        if local_dir.exists() {
            all_sessions.extend(search_sessions(&local_dir, pattern)?);
        }
    }

    // Search cached remote sessions
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

        match search_sessions_with_source(&cache_dir, pattern, source) {
            Ok(sessions) => all_sessions.extend(sessions),
            Err(e) => eprintln!("Warning: Failed to search sessions from '{}': {}", name, e),
        }
    }

    all_sessions.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(all_sessions)
}

// =============================================================================
// Session Loading
// =============================================================================

/// Find all sessions by scanning .jsonl files directly.
///
/// This replaces the previous two-phase approach (index + orphan) with a single
/// unified scan. All metadata is extracted directly from file contents.
pub fn find_sessions(projects_dir: &PathBuf) -> Result<Vec<Session>> {
    find_sessions_with_source(projects_dir, SessionSource::Local)
}

/// Find sessions with a specific source tag.
///
/// Used by both local discovery and remote cache discovery.
pub fn find_sessions_with_source(
    projects_dir: &PathBuf,
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

/// Check if a path is a valid session file (UUID.jsonl, not in subagents/)
fn is_valid_session_file(path: &Path) -> bool {
    // Must be .jsonl file
    if path.extension() != Some(std::ffi::OsStr::new("jsonl")) {
        return false;
    }
    // Skip subagents directory
    if path.to_string_lossy().contains("/subagents/") {
        return false;
    }
    // Must have valid UUID filename
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(is_valid_session_uuid)
        .unwrap_or(false)
}

/// Extract all session metadata from a .jsonl file.
///
/// Reads:
/// - HEAD (first ~50 lines): cwd, first user message, forkedFrom
/// - TAIL (last ~16KB): summary type entry, customTitle field
/// - Filesystem: created/modified timestamps
fn extract_session_metadata(
    filepath: &Path,
    parent_dir_name: &str,
    source: SessionSource,
) -> Option<Session> {
    let id = filepath.file_stem()?.to_string_lossy().to_string();

    // Get timestamps from file metadata
    let metadata = fs::metadata(filepath).ok()?;
    let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
    let created = metadata.created().unwrap_or(modified);

    // Extract metadata from file head
    let head = read_file_head(filepath);

    // Extract metadata from file tail (summary, customTitle)
    let (summary, custom_title) = read_file_tail(filepath);

    // Count conversation turns (user messages)
    let turn_count = count_turns(filepath);

    // Skip "empty" sessions that have no user content
    if head.project_path.is_empty() && head.first_prompt.is_none() && summary.is_none() {
        return None;
    }

    let project = extract_project_name(&head.project_path, parent_dir_name);

    Some(Session {
        id,
        project,
        project_path: head.project_path,
        filepath: filepath.to_path_buf(),
        created,
        modified,
        first_message: head.first_prompt,
        summary,
        name: custom_title,
        turn_count,
        source,
        forked_from: head.forked_from,
    })
}

/// Metadata extracted from session file head
#[derive(Default)]
struct HeadMetadata {
    project_path: String,
    first_prompt: Option<String>,
    forked_from: Option<String>,
}

/// Read the head of a session file to extract cwd, first user message, and fork parent
fn read_file_head(filepath: &Path) -> HeadMetadata {
    let mut project_path = String::new();
    let mut first_prompt = None;
    let mut forked_from = None;

    let Ok(file) = File::open(filepath) else {
        return HeadMetadata::default();
    };
    let reader = BufReader::new(file);

    for line in reader.lines().take(50) {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        if let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) {
            let entry_type = entry.get("type").and_then(|v| v.as_str());

            // Extract cwd (project path) from any message with it
            if project_path.is_empty() {
                if let Some(cwd) = entry.get("cwd").and_then(|v| v.as_str()) {
                    project_path = cwd.to_string();
                }
            }

            // Extract forkedFrom.sessionId (present on entries if this is a forked session)
            if forked_from.is_none() {
                if let Some(parent_id) = entry
                    .get("forkedFrom")
                    .and_then(|f| f.get("sessionId"))
                    .and_then(|v| v.as_str())
                {
                    forked_from = Some(parent_id.to_string());
                }
            }

            // Extract first user prompt
            if first_prompt.is_none() && entry_type == Some("user") {
                if let Some(content) = entry.get("message").and_then(|m| m.get("content")) {
                    if let Some(t) = extract_text_content(content) {
                        // Filter out system prompts and XML-tagged content
                        if !t.starts_with('/') && !t.starts_with("[Request") && !t.starts_with('<')
                        {
                            first_prompt = Some(crate::normalize_summary(&t, 50));
                        }
                    }
                }
            }

            // Stop early if we have all three
            if !project_path.is_empty() && first_prompt.is_some() && forked_from.is_some() {
                break;
            }
        }
    }

    HeadMetadata {
        project_path,
        first_prompt,
        forked_from,
    }
}

/// Read the tail of a session file to extract summary and customTitle
///
/// Strategy:
/// - Summary: Read last 16KB (summaries are always at end of compacted sessions)
/// - CustomTitle: Use grep to find `custom-title` type entry anywhere in file
fn read_file_tail(filepath: &Path) -> (Option<String>, Option<String>) {
    let summary = read_summary_from_tail(filepath);
    let custom_title = find_custom_title(filepath);
    (summary, custom_title)
}

/// Count conversation turns (real user messages) in a session file.
///
/// Only counts entries where:
/// - type == "user"
/// - message.content exists and is not system content (starts with <, [, or /)
fn count_turns(filepath: &Path) -> usize {
    use std::fs::File;
    use std::io::{BufRead, BufReader};

    let file = match File::open(filepath) {
        Ok(f) => f,
        Err(_) => return 0,
    };

    let reader = BufReader::new(file);
    let mut count = 0;

    for line in reader.lines().map_while(Result::ok) {
        let entry: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Only count type == "user" entries
        if entry.get("type").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }

        // Extract message content
        let content = match entry.get("message").and_then(|m| m.get("content")) {
            Some(c) => c,
            None => continue,
        };

        // Get text from content (string or array of blocks)
        let text = if let Some(s) = content.as_str() {
            s.to_string()
        } else if let Some(arr) = content.as_array() {
            arr.iter()
                .find_map(|c| {
                    if c.get("type")?.as_str()? == "text" {
                        Some(c.get("text")?.as_str()?.to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_default()
        } else {
            continue;
        };

        // Skip system content (starts with <, [, or /)
        if text.is_empty() || text.starts_with('<') || text.starts_with('[') || text.starts_with('/')
        {
            continue;
        }

        count += 1;
    }

    count
}

/// Read summary from the tail of the file (last 16KB)
fn read_summary_from_tail(filepath: &Path) -> Option<String> {
    const TAIL_SIZE: u64 = 16 * 1024; // 16KB

    let mut file = File::open(filepath).ok()?;
    let len = file.metadata().ok()?.len();

    // Seek to tail
    let start = len.saturating_sub(TAIL_SIZE);
    if start > 0 {
        file.seek(SeekFrom::Start(start)).ok()?;
    }

    let mut content = String::new();
    file.read_to_string(&mut content).ok()?;

    // If we seeked mid-file, skip first partial line
    if start > 0 {
        if let Some(newline) = content.find('\n') {
            content = content[newline + 1..].to_string();
        }
    }

    // Find summary type entry
    for line in content.lines() {
        if let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) {
            if entry.get("type").and_then(|v| v.as_str()) == Some("summary") {
                return entry
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .map(String::from);
            }
        }
    }

    None
}

/// Find customTitle by grepping the file for "custom-title" type entry
///
/// CustomTitle can appear anywhere in the file (when user runs /rename),
/// so we use grep to efficiently locate it rather than reading the whole file.
/// Returns the LAST match since users may rename multiple times.
fn find_custom_title(filepath: &Path) -> Option<String> {
    // Use grep to find custom-title lines efficiently
    let matcher = RegexMatcher::new_line_matcher(r#""type"\s*:\s*"custom-title""#).ok()?;
    let mut found_line = None;

    let _ = Searcher::new().search_path(
        &matcher,
        filepath,
        UTF8(|_, line| {
            found_line = Some(line.to_string());
            Ok(true) // Continue to find the last match (most recent rename)
        }),
    );

    // Parse the found line to extract customTitle
    let line = found_line?;
    let entry: serde_json::Value = serde_json::from_str(&line).ok()?;
    entry
        .get("customTitle")
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// Extract text from message content (string or array of content blocks)
fn extract_text_content(content: &serde_json::Value) -> Option<String> {
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

/// Search session transcripts for a pattern using grep crates.
///
/// Finds sessions containing the pattern and extracts metadata directly
/// from the matching files (no index dependency).
pub fn search_sessions(projects_dir: &PathBuf, pattern: &str) -> Result<Vec<Session>> {
    search_sessions_with_source(projects_dir, pattern, SessionSource::Local)
}

/// Search sessions with a specific source tag.
pub fn search_sessions_with_source(
    projects_dir: &PathBuf,
    pattern: &str,
    source: SessionSource,
) -> Result<Vec<Session>> {
    let matcher = RegexMatcher::new_line_matcher(pattern).context("Invalid search pattern")?;

    // Find all .jsonl files with valid UUID filenames
    let jsonl_files: Vec<PathBuf> = WalkDir::new(projects_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| is_valid_session_file(e.path()))
        .map(|e| e.path().to_path_buf())
        .collect();

    // Search files in parallel
    let matching_files: Vec<PathBuf> = jsonl_files
        .par_iter()
        .filter(|path| {
            let mut found = false;
            let _ = Searcher::new().search_path(
                &matcher,
                path,
                UTF8(|_, _| {
                    found = true;
                    Ok(false) // Stop after first match
                }),
            );
            found
        })
        .cloned()
        .collect();

    // Extract metadata directly from matching files
    let mut sessions: Vec<Session> = matching_files
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

// =============================================================================
// Helper Functions
// =============================================================================

/// Extract project name from path or directory name fallback
///
/// Claude Code uses directory names like `-Users-iantay-Documents-repos-foo`
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

    // Parse directory name: "-Users-iantay-Documents-repos-foo" -> "foo"
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
            extract_project_name("", "-Users-iantay-Documents-repos-cc-session"),
            "cc-session"
        );
        assert_eq!(
            extract_project_name("", "-Users-iantay-third-party-repos-foo"),
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
        let temp_dir = std::env::temp_dir().join(format!("cc-session-test-{}", std::process::id()));
        let project_dir = temp_dir.join("-Users-sirrobin-holy-grail");
        fs::create_dir_all(&project_dir).unwrap();

        let uuid1 = test_uuid(1);
        let uuid2 = test_uuid(2);

        let session1_path = project_dir.join(format!("{}.jsonl", uuid1));
        let session2_path = project_dir.join(format!("{}.jsonl", uuid2));

        // Session with cwd and user message
        fs::write(
            &session1_path,
            r#"{"type":"user","message":{"role":"user","content":"Tis but a scratch"},"cwd":"/Users/sirrobin/holy-grail"}"#,
        )
        .unwrap();

        // Session with summary in tail
        fs::write(
            &session2_path,
            r#"{"type":"user","message":{"role":"user","content":"Run away!"},"cwd":"/Users/sirrobin/holy-grail"}
{"type":"summary","summary":"Deploying Holy Hand Grenade of Antioch"}"#,
        )
        .unwrap();

        let sessions = find_sessions(&temp_dir).unwrap();

        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].project, "holy-grail");

        // Find session with summary
        let with_summary = sessions.iter().find(|s| s.summary.is_some()).unwrap();
        assert_eq!(
            with_summary.summary,
            Some("Deploying Holy Hand Grenade of Antioch".to_string())
        );

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn find_sessions_filters_non_uuid_files() {
        let temp_dir =
            std::env::temp_dir().join(format!("cc-session-test-filter-{}", std::process::id()));
        let project_dir = temp_dir.join("-Users-arthur-camelot");
        fs::create_dir_all(&project_dir).unwrap();

        let valid_uuid = test_uuid(42);

        // Valid UUID file - should be found
        let valid_path = project_dir.join(format!("{}.jsonl", valid_uuid));
        fs::write(
            &valid_path,
            r#"{"type":"user","message":{"role":"user","content":"What is your quest?"},"cwd":"/Users/arthur/camelot"}"#,
        )
        .unwrap();

        // Non-UUID files - should be filtered out
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

        let sessions = find_sessions(&temp_dir).unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, valid_uuid);

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn find_sessions_extracts_custom_title_from_tail() {
        let temp_dir =
            std::env::temp_dir().join(format!("cc-session-test-title-{}", std::process::id()));
        let project_dir = temp_dir.join("-Users-brian-life");
        fs::create_dir_all(&project_dir).unwrap();

        let uuid = test_uuid(99);
        let session_path = project_dir.join(format!("{}.jsonl", uuid));

        // Session with customTitle from /rename command
        // The real format is {"type":"custom-title","customTitle":"...","sessionId":"..."}
        fs::write(
            &session_path,
            r#"{"type":"user","message":{"role":"user","content":"Always look on the bright side"},"cwd":"/Users/brian/life"}
{"type":"assistant","message":"Indeed!"}
{"type":"custom-title","customTitle":"Important Session","sessionId":"00000063-0063-0063-0063-000000000063"}"#,
        )
        .unwrap();

        let sessions = find_sessions(&temp_dir).unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, Some("Important Session".to_string()));

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn find_sessions_handles_empty_sessions() {
        let temp_dir =
            std::env::temp_dir().join(format!("cc-session-test-empty-{}", std::process::id()));
        let project_dir = temp_dir.join("-Users-spam-eggs");
        fs::create_dir_all(&project_dir).unwrap();

        let uuid = test_uuid(7);
        let session_path = project_dir.join(format!("{}.jsonl", uuid));

        // Empty session with no cwd, no user message, no summary
        fs::write(&session_path, r#"{"type":"init"}"#).unwrap();

        let sessions = find_sessions(&temp_dir).unwrap();

        // Empty sessions should be filtered out
        assert_eq!(sessions.len(), 0);

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    // =========================================================================
    // Fork detection tests
    // =========================================================================

    #[test]
    fn find_sessions_extracts_forked_from() {
        let temp_dir =
            std::env::temp_dir().join(format!("cc-session-test-fork-{}", std::process::id()));
        let project_dir = temp_dir.join("-Users-patsy-camelot");
        fs::create_dir_all(&project_dir).unwrap();

        let parent_uuid = test_uuid(10);
        let fork_uuid = test_uuid(11);

        // Parent session (no forkedFrom)
        let parent_path = project_dir.join(format!("{}.jsonl", parent_uuid));
        fs::write(
            &parent_path,
            r#"{"type":"user","message":{"role":"user","content":"What is your quest?"},"cwd":"/Users/patsy/camelot","sessionId":"00000010-0010-0010-0010-00000000000a"}"#,
        )
        .unwrap();

        // Forked session (has forkedFrom pointing to parent)
        let fork_path = project_dir.join(format!("{}.jsonl", fork_uuid));
        fs::write(
            &fork_path,
            r#"{"type":"user","message":{"role":"user","content":"What is your quest?"},"cwd":"/Users/patsy/camelot","sessionId":"0000000b-000b-000b-000b-00000000000b","forkedFrom":{"sessionId":"00000010-0010-0010-0010-00000000000a","messageUuid":"abc123"}}"#,
        )
        .unwrap();

        let sessions = find_sessions(&temp_dir).unwrap();

        assert_eq!(sessions.len(), 2);

        // Find parent and fork
        let parent = sessions.iter().find(|s| s.id == parent_uuid).unwrap();
        let fork = sessions.iter().find(|s| s.id == fork_uuid).unwrap();

        // Parent should have no forked_from
        assert_eq!(parent.forked_from, None);

        // Fork should point to parent
        assert_eq!(
            fork.forked_from,
            Some("00000010-0010-0010-0010-00000000000a".to_string())
        );

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn find_sessions_multiple_forks_same_parent() {
        let temp_dir =
            std::env::temp_dir().join(format!("cc-session-test-multi-fork-{}", std::process::id()));
        let project_dir = temp_dir.join("-Users-tim-enchanter");
        fs::create_dir_all(&project_dir).unwrap();

        let parent_uuid = test_uuid(20);
        let fork1_uuid = test_uuid(21);
        let fork2_uuid = test_uuid(22);

        // Parent session
        let parent_path = project_dir.join(format!("{}.jsonl", parent_uuid));
        fs::write(
            &parent_path,
            r#"{"type":"user","message":{"role":"user","content":"What manner of man are you?"},"cwd":"/Users/tim/enchanter"}"#,
        )
        .unwrap();

        // Fork 1
        let fork1_path = project_dir.join(format!("{}.jsonl", fork1_uuid));
        fs::write(
            &fork1_path,
            r#"{"type":"user","message":{"role":"user","content":"What manner of man are you?"},"cwd":"/Users/tim/enchanter","forkedFrom":{"sessionId":"00000014-0014-0014-0014-000000000014","messageUuid":"msg1"}}"#,
        )
        .unwrap();

        // Fork 2
        let fork2_path = project_dir.join(format!("{}.jsonl", fork2_uuid));
        fs::write(
            &fork2_path,
            r#"{"type":"user","message":{"role":"user","content":"What manner of man are you?"},"cwd":"/Users/tim/enchanter","forkedFrom":{"sessionId":"00000014-0014-0014-0014-000000000014","messageUuid":"msg2"}}"#,
        )
        .unwrap();

        let sessions = find_sessions(&temp_dir).unwrap();

        assert_eq!(sessions.len(), 3);

        // Both forks should point to the same parent
        let fork1 = sessions.iter().find(|s| s.id == fork1_uuid).unwrap();
        let fork2 = sessions.iter().find(|s| s.id == fork2_uuid).unwrap();

        assert_eq!(fork1.forked_from, fork2.forked_from);
        assert_eq!(
            fork1.forked_from,
            Some("00000014-0014-0014-0014-000000000014".to_string())
        );

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn read_file_head_prefers_first_forked_from() {
        let temp_dir = std::env::temp_dir()
            .join(format!("cc-session-test-fork-order-{}", std::process::id()));
        fs::create_dir_all(&temp_dir).unwrap();

        let session_path = temp_dir.join("test.jsonl");
        fs::write(
            &session_path,
            r#"{"type":"user","message":{"role":"user","content":"hello"},"forkedFrom":{"sessionId":"parent-1","messageUuid":"m1"}}
{"type":"assistant","message":"hi"}
{"type":"user","message":{"role":"user","content":"later"},"forkedFrom":{"sessionId":"parent-2","messageUuid":"m2"}}"#,
        )
        .unwrap();

        let head = read_file_head(&session_path);
        assert_eq!(head.forked_from, Some("parent-1".to_string()));

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn read_file_head_extracts_forked_from_early() {
        // Test that forkedFrom is extracted even if it appears on a later entry
        // (not just the first line)
        let temp_dir = std::env::temp_dir()
            .join(format!("cc-session-test-fork-pos-{}", std::process::id()));
        fs::create_dir_all(&temp_dir).unwrap();

        let session_path = temp_dir.join("test.jsonl");
        fs::write(
            &session_path,
            r#"{"type":"progress","data":"starting"}
{"type":"progress","cwd":"/Users/test/project","data":"hook"}
{"type":"user","message":{"role":"user","content":"hello"},"forkedFrom":{"sessionId":"parent-session-id","messageUuid":"msg1"}}
{"type":"assistant","message":"hi"}"#,
        )
        .unwrap();

        let head = read_file_head(&session_path);

        assert_eq!(head.project_path, "/Users/test/project");
        assert_eq!(head.forked_from, Some("parent-session-id".to_string()));
        assert_eq!(head.first_prompt, Some("hello".to_string()));

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    // =========================================================================
    // Turn counting - only real user messages, not system content
    // =========================================================================

    #[test]
    fn count_turns_real_user_messages() {
        let temp_dir =
            std::env::temp_dir().join(format!("cc-session-test-turns-{}", std::process::id()));
        fs::create_dir_all(&temp_dir).unwrap();

        let session_path = temp_dir.join("test.jsonl");
        fs::write(
            &session_path,
            r#"{"type":"user","message":{"role":"user","content":"Hello, how are you?"}}
{"type":"assistant","message":{"role":"assistant","content":"I'm good!"}}
{"type":"user","message":{"role":"user","content":"What is Rust?"}}
{"type":"assistant","message":{"role":"assistant","content":"A programming language."}}"#,
        )
        .unwrap();

        let count = count_turns(&session_path);
        assert_eq!(count, 2); // Two real user messages

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn count_turns_excludes_system_content() {
        let temp_dir = std::env::temp_dir()
            .join(format!("cc-session-test-turns-sys-{}", std::process::id()));
        fs::create_dir_all(&temp_dir).unwrap();

        let session_path = temp_dir.join("test.jsonl");
        fs::write(
            &session_path,
            r#"{"type":"user","message":{"role":"user","content":"<command-message>init</command-message>"}}
{"type":"user","message":{"role":"user","content":"Real message here"}}
{"type":"user","message":{"role":"user","content":"<local-command-stdout>output</local-command-stdout>"}}
{"type":"user","message":{"role":"user","content":"/help"}}
{"type":"user","message":{"role":"user","content":"[some bracketed thing]"}}
{"type":"user","message":{"role":"user","content":"Another real message"}}"#,
        )
        .unwrap();

        let count = count_turns(&session_path);
        assert_eq!(count, 2); // Only "Real message here" and "Another real message"

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn count_turns_handles_content_blocks() {
        let temp_dir = std::env::temp_dir()
            .join(format!("cc-session-test-turns-blocks-{}", std::process::id()));
        fs::create_dir_all(&temp_dir).unwrap();

        let session_path = temp_dir.join("test.jsonl");
        // Content as array of blocks (common format)
        fs::write(
            &session_path,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Hello from blocks"}]}}
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"<command-name>/init</command-name>"}]}}
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Real question?"}]}}"#,
        )
        .unwrap();

        let count = count_turns(&session_path);
        assert_eq!(count, 2); // "Hello from blocks" and "Real question?"

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn count_turns_empty_file() {
        let temp_dir = std::env::temp_dir()
            .join(format!("cc-session-test-turns-empty-{}", std::process::id()));
        fs::create_dir_all(&temp_dir).unwrap();

        let session_path = temp_dir.join("test.jsonl");
        fs::write(&session_path, "").unwrap();

        let count = count_turns(&session_path);
        assert_eq!(count, 0);

        fs::remove_dir_all(&temp_dir).unwrap();
    }
}
