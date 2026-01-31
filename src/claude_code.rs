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

use crate::Session;
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

// =============================================================================
// Session Loading
// =============================================================================

/// Find all sessions by scanning .jsonl files directly.
///
/// This replaces the previous two-phase approach (index + orphan) with a single
/// unified scan. All metadata is extracted directly from file contents.
pub fn find_sessions(projects_dir: &PathBuf) -> Result<Vec<Session>> {
    // Find all .jsonl files with valid UUID filenames
    let jsonl_files: Vec<PathBuf> = WalkDir::new(projects_dir)
        .min_depth(2)
        .max_depth(2)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let path = e.path();
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
        })
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
            extract_session_metadata(filepath, &parent_dir)
        })
        .collect();

    sessions.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(sessions)
}

/// Check if a string is a valid UUID (8-4-4-4-12 format with hex chars)
fn is_valid_session_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    if parts[0].len() != 8
        || parts[1].len() != 4
        || parts[2].len() != 4
        || parts[3].len() != 4
        || parts[4].len() != 12
    {
        return false;
    }
    s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Extract all session metadata from a .jsonl file.
///
/// Reads:
/// - HEAD (first ~50 lines): cwd, first user message
/// - TAIL (last ~16KB): summary type entry, customTitle field
/// - Filesystem: created/modified timestamps
fn extract_session_metadata(filepath: &Path, parent_dir_name: &str) -> Option<Session> {
    let id = filepath.file_stem()?.to_string_lossy().to_string();

    // Get timestamps from file metadata
    let metadata = fs::metadata(filepath).ok()?;
    let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
    let created = metadata.created().unwrap_or(modified);

    // Extract metadata from file head
    let (project_path, first_message) = read_file_head(filepath);

    // Extract metadata from file tail (summary, customTitle)
    let (summary, custom_title) = read_file_tail(filepath);

    // Skip "empty" sessions that have no user content
    if project_path.is_empty() && first_message.is_none() && summary.is_none() {
        return None;
    }

    let project = extract_project_name(&project_path, parent_dir_name);

    Some(Session {
        id,
        project,
        project_path,
        filepath: filepath.to_path_buf(),
        created,
        modified,
        first_message,
        summary,
        name: custom_title,
    })
}

/// Read the head of a session file to extract cwd and first user message
fn read_file_head(filepath: &Path) -> (String, Option<String>) {
    let mut project_path = String::new();
    let mut first_prompt = None;

    let file = match File::open(filepath) {
        Ok(f) => f,
        Err(_) => return (project_path, first_prompt),
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

            // Stop early if we have both
            if !project_path.is_empty() && first_prompt.is_some() {
                break;
            }
        }
    }

    (project_path, first_prompt)
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
                return entry.get("summary").and_then(|v| v.as_str()).map(String::from);
            }
        }
    }

    None
}

/// Find customTitle by grepping the file for "custom-title" type entry
///
/// CustomTitle can appear anywhere in the file (when user runs /rename),
/// so we use grep to efficiently locate it rather than reading the whole file.
fn find_custom_title(filepath: &Path) -> Option<String> {
    // Use grep to find custom-title lines efficiently
    let matcher = RegexMatcher::new_line_matcher(r#""type"\s*:\s*"custom-title""#).ok()?;
    let mut found_line = None;

    let _ = Searcher::new().search_path(
        &matcher,
        filepath,
        UTF8(|_, line| {
            found_line = Some(line.to_string());
            Ok(false) // Stop after first match (there should only be one)
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
    let matcher = RegexMatcher::new_line_matcher(pattern).context("Invalid search pattern")?;

    // Find all .jsonl files with valid UUID filenames
    let jsonl_files: Vec<PathBuf> = WalkDir::new(projects_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let path = e.path();
            if !path.extension().is_some_and(|ext| ext == "jsonl") {
                return false;
            }
            if path.to_string_lossy().contains("/subagents/") {
                return false;
            }
            // Must have valid UUID filename
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(is_valid_session_uuid)
                .unwrap_or(false)
        })
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
            extract_session_metadata(filepath, &parent_dir)
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
        assert!(is_valid_session_uuid("12345678-1234-1234-1234-123456789abc"));
        assert!(is_valid_session_uuid("abcdef00-abcd-abcd-abcd-abcdef123456"));
        assert!(is_valid_session_uuid("ABCDEF00-ABCD-ABCD-ABCD-ABCDEF123456"));
    }

    #[test]
    fn uuid_validation_invalid_formats() {
        // Wrong segment lengths
        assert!(!is_valid_session_uuid("1234567-1234-1234-1234-123456789abc")); // 7 chars
        assert!(!is_valid_session_uuid("12345678-123-1234-1234-123456789abc")); // 3 chars
        assert!(!is_valid_session_uuid("12345678-1234-1234-1234-123456789ab")); // 11 chars

        // Wrong number of segments
        assert!(!is_valid_session_uuid("12345678-1234-1234-123456789abc"));
        assert!(!is_valid_session_uuid("12345678-1234-1234-1234-1234-123456789abc"));

        // Non-hex characters
        assert!(!is_valid_session_uuid("1234567g-1234-1234-1234-123456789abc"));

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
}
