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
//!     sessions-index.json    # Metadata for indexed sessions
//!     abc123.jsonl           # Session transcript
//!     def456.jsonl
//!   -Users-you-project-b/
//!     sessions-index.json
//!     ghi789.jsonl
//! ```

use crate::{Session, SessionSource};
use anyhow::{Context, Result};
use grep_regex::RegexMatcher;
use grep_searcher::Searcher;
use grep_searcher::sinks::UTF8;
use rayon::prelude::*;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

// =============================================================================
// Claude Code Index Schema
// =============================================================================

#[derive(Deserialize)]
pub struct SessionsIndex {
    pub entries: Vec<IndexEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexEntry {
    pub session_id: String,
    pub full_path: String,
    pub first_prompt: Option<String>,
    pub summary: Option<String>,
    pub custom_title: Option<String>,
    pub created: Option<String>,
    pub modified: Option<String>,
    pub project_path: Option<String>,
}

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

pub fn find_sessions(projects_dir: &PathBuf) -> Result<Vec<Session>> {
    // Find all sessions-index.json files
    let index_files: Vec<_> = WalkDir::new(projects_dir)
        .min_depth(2)
        .max_depth(2)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() == "sessions-index.json")
        .map(|e| e.path().to_path_buf())
        .collect();

    // Track indexed session IDs to find orphans later
    let mut indexed_ids: HashSet<String> = HashSet::new();

    // Process in parallel with rayon
    let sessions: Vec<Session> = index_files
        .par_iter()
        .flat_map(|index_path| {
            let content = fs::read_to_string(index_path).ok()?;
            let index: SessionsIndex = serde_json::from_str(&content).ok()?;

            Some(
                index
                    .entries
                    .into_iter()
                    .filter_map(|entry| session_from_index_entry(entry))
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
        .collect();

    // Collect indexed IDs
    for s in &sessions {
        indexed_ids.insert(s.id.clone());
    }

    // Find orphaned .jsonl files (not in any index)
    let orphaned_sessions: Vec<Session> = WalkDir::new(projects_dir)
        .min_depth(2)
        .max_depth(2)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();

            // Filter by extension and skip subagents directory
            if path.extension() != Some(std::ffi::OsStr::new("jsonl")) {
                return None;
            }
            if path.to_string_lossy().contains("/subagents/") {
                return None;
            }

            let id = path.file_stem()?.to_string_lossy();

            // Skip indexed and agent-* sessions
            if indexed_ids.contains(id.as_ref()) || id.starts_with("agent-") {
                return None;
            }

            let filepath = path.to_path_buf();
            let parent_dir = filepath
                .parent()?
                .file_name()?
                .to_string_lossy()
                .to_string();
            session_from_orphan_file(&filepath, &parent_dir)
        })
        .collect();

    let mut sessions = sessions;
    sessions.extend(orphaned_sessions);
    sessions.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(sessions)
}

/// Create a Session from an IndexEntry
fn session_from_index_entry(entry: IndexEntry) -> Option<Session> {
    let filepath = PathBuf::from(&entry.full_path);
    if !filepath.exists() {
        return None;
    }

    let project_path = entry.project_path.unwrap_or_default();
    let project = project_path
        .split('/')
        .last()
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown")
        .to_string();

    let created = parse_timestamp(entry.created.as_deref());

    // Use file mtime if newer than index timestamp (index may be stale)
    let modified = std::cmp::max(
        parse_timestamp(entry.modified.as_deref()),
        file_mtime(&filepath),
    );

    let first_message = entry.first_prompt.as_ref().and_then(|p| {
        if p == "No prompt" || p.starts_with("[Request") || p.starts_with("/") {
            None
        } else {
            Some(crate::normalize_summary(p, 50))
        }
    });

    Some(Session {
        id: entry.session_id,
        source: SessionSource::Indexed,
        project,
        project_path,
        filepath,
        created,
        modified,
        first_message,
        summary: entry.summary,
        name: entry.custom_title,
    })
}

/// Create a Session from an orphaned .jsonl file (no index entry)
fn session_from_orphan_file(filepath: &PathBuf, parent_dir_name: &str) -> Option<Session> {
    let id = filepath.file_stem()?.to_string_lossy().to_string();

    // Get timestamps from file metadata
    let metadata = fs::metadata(filepath).ok()?;
    let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
    let created = metadata.created().unwrap_or(modified);

    // Extract project path, first prompt, and summary from file content
    let (project_path, first_message, summary) = extract_orphan_metadata(filepath);

    // Skip "empty" sessions that have no user content
    if project_path.is_empty() && first_message.is_none() && summary.is_none() {
        return None;
    }

    let project = extract_project_name(&project_path, parent_dir_name);

    Some(Session {
        id,
        source: SessionSource::Orphan,
        project,
        project_path,
        filepath: filepath.clone(),
        created,
        modified,
        first_message,
        summary,
        name: None, // Orphans don't have customTitle
    })
}

/// Extract project path, first prompt, and summary from a session JSONL file
fn extract_orphan_metadata(filepath: &PathBuf) -> (String, Option<String>, Option<String>) {
    use std::io::{BufRead, BufReader};

    let mut project_path = String::new();
    let mut first_prompt = None;
    let mut summary = None;

    let file = match fs::File::open(filepath) {
        Ok(f) => f,
        Err(_) => return (project_path, first_prompt, summary),
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

            // Extract summary from compacted sessions
            if summary.is_none() && entry_type == Some("summary") {
                if let Some(s) = entry.get("summary").and_then(|v| v.as_str()) {
                    summary = Some(s.to_string());
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

            // Stop early if we have all we need
            if !project_path.is_empty() && (first_prompt.is_some() || summary.is_some()) {
                break;
            }
        }
    }

    (project_path, first_prompt, summary)
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

/// Search session transcripts for a pattern using grep crates
pub fn search_sessions(projects_dir: &PathBuf, pattern: &str) -> Result<Vec<Session>> {
    let matcher = RegexMatcher::new_line_matcher(pattern).context("Invalid search pattern")?;

    // Find all .jsonl files
    let jsonl_files: Vec<PathBuf> = WalkDir::new(projects_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path().extension().is_some_and(|ext| ext == "jsonl")
                && !e.path().to_string_lossy().contains("/subagents/")
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

    // Load session metadata for matching files
    let sessions: Vec<Session> = matching_files
        .par_iter()
        .filter_map(|filepath| {
            let session_id = filepath.file_stem()?.to_string_lossy().to_string();
            let index_path = filepath.parent()?.join("sessions-index.json");
            let content = fs::read_to_string(&index_path).ok()?;
            let index: SessionsIndex = serde_json::from_str(&content).ok()?;

            index.entries.into_iter().find_map(|entry| {
                if entry.session_id != session_id {
                    return None;
                }

                let project_path = entry.project_path.unwrap_or_default();
                let project = project_path
                    .split('/')
                    .last()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("unknown")
                    .to_string();

                let created = entry
                    .created
                    .as_deref()
                    .and_then(parse_iso_time)
                    .unwrap_or(UNIX_EPOCH);
                let modified = entry
                    .modified
                    .as_deref()
                    .and_then(parse_iso_time)
                    .unwrap_or(UNIX_EPOCH);

                Some(Session {
                    id: entry.session_id,
                    source: SessionSource::Indexed,
                    project,
                    project_path,
                    filepath: filepath.clone(),
                    created,
                    modified,
                    first_message: entry.first_prompt,
                    summary: entry.summary,
                    name: entry.custom_title,
                })
            })
        })
        .collect();

    let mut sessions = sessions;
    sessions.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(sessions)
}

// =============================================================================
// Helper Functions
// =============================================================================

/// Parse ISO 8601 timestamp (Claude Code format: 2026-01-15T06:15:58.913Z)
pub fn parse_iso_time(s: &str) -> Option<SystemTime> {
    let s = s.trim_end_matches('Z');
    let (date, time) = s.split_once('T')?;
    let date_parts: Vec<&str> = date.split('-').collect();
    let time_parts: Vec<&str> = time.split(':').collect();

    if date_parts.len() != 3 || time_parts.len() < 2 {
        return None;
    }

    let year: i64 = date_parts[0].parse().ok()?;
    let month: u32 = date_parts[1].parse().ok()?;
    let day: u32 = date_parts[2].parse().ok()?;

    let hour: u32 = time_parts[0].parse().ok()?;
    let min: u32 = time_parts[1].parse().ok()?;
    let sec: f64 = time_parts
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);

    // Days from epoch to year
    let mut days: i64 = 0;
    for y in 1970..year {
        days += if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
    }

    // Days from month
    let month_days = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    days += month_days[month as usize - 1] as i64;
    if month > 2 && year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
        days += 1;
    }
    days += day as i64 - 1;

    let secs = days * 86400 + hour as i64 * 3600 + min as i64 * 60 + sec as i64;
    Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// Parse optional ISO timestamp string, defaulting to UNIX_EPOCH
fn parse_timestamp(s: Option<&str>) -> SystemTime {
    s.and_then(parse_iso_time).unwrap_or(UNIX_EPOCH)
}

/// Get file modification time, defaulting to UNIX_EPOCH on error
fn file_mtime(path: &PathBuf) -> SystemTime {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(UNIX_EPOCH)
}

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
    // parse_iso_time - Critical for session sorting
    // =========================================================================

    #[test]
    fn parse_iso_time_standard_format() {
        let result = parse_iso_time("2026-01-15T06:15:58.913Z");
        assert!(result.is_some());

        let secs = result
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let year_2025 = 55 * 365 * 86400;
        let year_2027 = 57 * 365 * 86400;
        assert!(secs > year_2025 && secs < year_2027);
    }

    #[test]
    fn parse_iso_time_without_milliseconds() {
        let result = parse_iso_time("2026-01-15T06:15:58Z");
        assert!(result.is_some());
    }

    #[test]
    fn parse_iso_time_ordering_preserved() {
        let earlier = parse_iso_time("2026-01-15T06:00:00Z").unwrap();
        let later = parse_iso_time("2026-01-15T07:00:00Z").unwrap();
        assert!(earlier < later);

        let day1 = parse_iso_time("2026-01-15T12:00:00Z").unwrap();
        let day2 = parse_iso_time("2026-01-16T12:00:00Z").unwrap();
        assert!(day1 < day2);
    }

    #[test]
    fn parse_iso_time_leap_year() {
        let result = parse_iso_time("2024-02-29T12:00:00Z");
        assert!(result.is_some());

        let feb29 = parse_iso_time("2024-02-29T12:00:00Z").unwrap();
        let mar1 = parse_iso_time("2024-03-01T12:00:00Z").unwrap();
        let diff = mar1.duration_since(feb29).unwrap().as_secs();
        assert_eq!(diff, 86400);
    }

    #[test]
    fn parse_iso_time_invalid_formats() {
        assert!(parse_iso_time("not a date").is_none());
        assert!(parse_iso_time("2026-01-15").is_none());
        assert!(parse_iso_time("06:15:58Z").is_none());
        assert!(parse_iso_time("").is_none());
    }

    // =========================================================================
    // IndexEntry deserialization
    // =========================================================================

    #[test]
    fn index_entry_deserialize_full() {
        let json = r#"{
            "sessionId": "abc-123",
            "fullPath": "/home/user/.claude/sessions/abc.jsonl",
            "firstPrompt": "Hello world",
            "summary": "Test summary",
            "created": "2026-01-15T06:00:00Z",
            "modified": "2026-01-15T07:00:00Z",
            "projectPath": "/home/user/my-project"
        }"#;

        let entry: IndexEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.session_id, "abc-123");
        assert_eq!(entry.full_path, "/home/user/.claude/sessions/abc.jsonl");
        assert_eq!(entry.first_prompt, Some("Hello world".to_string()));
        assert_eq!(entry.summary, Some("Test summary".to_string()));
        assert_eq!(
            entry.project_path,
            Some("/home/user/my-project".to_string())
        );
    }

    #[test]
    fn index_entry_deserialize_minimal() {
        let json = r#"{
            "sessionId": "abc-123",
            "fullPath": "/path/to/session.jsonl"
        }"#;

        let entry: IndexEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.session_id, "abc-123");
        assert!(entry.first_prompt.is_none());
        assert!(entry.summary.is_none());
        assert!(entry.project_path.is_none());
    }

    #[test]
    fn sessions_index_deserialize() {
        let json = r#"{
            "entries": [
                {"sessionId": "sess-1", "fullPath": "/path/1.jsonl"},
                {"sessionId": "sess-2", "fullPath": "/path/2.jsonl"}
            ]
        }"#;

        let index: SessionsIndex = serde_json::from_str(json).unwrap();
        assert_eq!(index.entries.len(), 2);
        assert_eq!(index.entries[0].session_id, "sess-1");
        assert_eq!(index.entries[1].session_id, "sess-2");
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

    #[test]
    fn find_sessions_with_fake_data() {
        let temp_dir = std::env::temp_dir().join(format!("cc-session-test-{}", std::process::id()));
        let project_dir = temp_dir.join("-Users-sirrobin-holy-grail");
        fs::create_dir_all(&project_dir).unwrap();

        let session1_path = project_dir.join("black-knight-battle.jsonl");
        let session2_path = project_dir.join("killer-rabbit-encounter.jsonl");
        fs::write(
            &session1_path,
            r#"{"type":"user","message":"Tis but a scratch"}"#,
        )
        .unwrap();
        fs::write(&session2_path, r#"{"type":"user","message":"Run away!"}"#).unwrap();

        let index_json = format!(
            r#"{{
                "entries": [
                    {{
                        "sessionId": "black-knight-battle",
                        "fullPath": "{}",
                        "projectPath": "/Users/sirrobin/holy-grail",
                        "summary": "Losing limbs but staying positive",
                        "created": "1975-04-03T10:00:00Z",
                        "modified": "1975-04-03T11:00:00Z"
                    }},
                    {{
                        "sessionId": "killer-rabbit-encounter",
                        "fullPath": "{}",
                        "projectPath": "/Users/sirrobin/holy-grail",
                        "summary": "Deploying Holy Hand Grenade of Antioch",
                        "created": "1975-04-03T14:00:00Z",
                        "modified": "1975-04-03T15:00:00Z"
                    }}
                ]
            }}"#,
            session1_path.display(),
            session2_path.display()
        );
        fs::write(project_dir.join("sessions-index.json"), index_json).unwrap();

        let sessions = find_sessions(&temp_dir).unwrap();

        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "killer-rabbit-encounter");
        assert_eq!(sessions[1].id, "black-knight-battle");
        assert_eq!(sessions[0].project, "holy-grail");
        assert_eq!(
            sessions[0].summary,
            Some("Deploying Holy Hand Grenade of Antioch".to_string())
        );

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn find_sessions_filters_missing_files() {
        let temp_dir =
            std::env::temp_dir().join(format!("cc-session-test-parrot-{}", std::process::id()));
        let project_dir = temp_dir.join("-Users-shopkeeper-ministry-of-silly-walks");
        fs::create_dir_all(&project_dir).unwrap();

        let resting_parrot = project_dir.join("resting-parrot.jsonl");
        fs::write(
            &resting_parrot,
            r#"{"type":"user","message":"Beautiful plumage!"}"#,
        )
        .unwrap();

        let index_json = format!(
            r#"{{
                "entries": [
                    {{
                        "sessionId": "resting-parrot",
                        "fullPath": "{}",
                        "projectPath": "/Users/shopkeeper/ministry-of-silly-walks",
                        "summary": "Pining for the fjords"
                    }},
                    {{
                        "sessionId": "ex-parrot",
                        "fullPath": "/nonexistent/this/parrot/has/ceased/to/be.jsonl",
                        "projectPath": "/Users/shopkeeper/ministry-of-silly-walks",
                        "summary": "Has joined the choir invisible"
                    }}
                ]
            }}"#,
            resting_parrot.display()
        );
        fs::write(project_dir.join("sessions-index.json"), index_json).unwrap();

        let sessions = find_sessions(&temp_dir).unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "resting-parrot");
        assert_eq!(
            sessions[0].summary,
            Some("Pining for the fjords".to_string())
        );

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn find_sessions_handles_corrupted_index() {
        let temp_dir =
            std::env::temp_dir().join(format!("cc-session-test-ni-{}", std::process::id()));
        let knights_dir = temp_dir.join("-Users-knight-shrubbery");
        let good_dir = temp_dir.join("-Users-arthur-camelot");
        fs::create_dir_all(&knights_dir).unwrap();
        fs::create_dir_all(&good_dir).unwrap();

        fs::write(
            knights_dir.join("sessions-index.json"),
            "NI! NI! NI! We demand a shrubbery!",
        )
        .unwrap();

        let quest_path = good_dir.join("seek-holy-grail.jsonl");
        fs::write(
            &quest_path,
            r#"{"type":"user","message":"What is your quest?"}"#,
        )
        .unwrap();
        let good_index = format!(
            r#"{{
                "entries": [{{
                    "sessionId": "seek-holy-grail",
                    "fullPath": "{}",
                    "projectPath": "/Users/arthur/camelot",
                    "summary": "To seek the Holy Grail"
                }}]
            }}"#,
            quest_path.display()
        );
        fs::write(good_dir.join("sessions-index.json"), good_index).unwrap();

        let sessions = find_sessions(&temp_dir).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "seek-holy-grail");

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn find_sessions_discovers_orphaned_files() {
        let temp_dir = std::env::temp_dir().join(format!(
            "cc-session-test-inquisition-{}",
            std::process::id()
        ));
        let inquisition_dir = temp_dir.join("-Users-cardinal-inquisition");
        fs::create_dir_all(&inquisition_dir).unwrap();

        let orphan_session = inquisition_dir.join("unexpected-session.jsonl");
        fs::write(
            &orphan_session,
            r#"{"type":"user","message":{"role":"user","content":"Nobody expects the Spanish Inquisition!"},"cwd":"/Users/cardinal/inquisition"}"#,
        )
        .unwrap();

        fs::write(
            inquisition_dir.join("sessions-index.json"),
            r#"{"entries": []}"#,
        )
        .unwrap();

        let sessions = find_sessions(&temp_dir).unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "unexpected-session");
        assert_eq!(sessions[0].project, "inquisition");
        assert_eq!(sessions[0].project_path, "/Users/cardinal/inquisition");
        assert!(sessions[0].summary.is_none());
        assert_eq!(
            sessions[0].first_message,
            Some("Nobody expects the Spanish Inquisition!".to_string())
        );

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn find_sessions_merges_indexed_and_orphaned() {
        let temp_dir =
            std::env::temp_dir().join(format!("cc-session-test-realm-{}", std::process::id()));
        let realm_dir = temp_dir.join("-Users-arthur-realm");
        fs::create_dir_all(&realm_dir).unwrap();

        let indexed_session = realm_dir.join("indexed-quest.jsonl");
        fs::write(
            &indexed_session,
            r#"{"type":"user","message":"I seek the grail"}"#,
        )
        .unwrap();

        let orphan_session = realm_dir.join("orphan-quest.jsonl");
        fs::write(
            &orphan_session,
            r#"{"type":"user","message":{"role":"user","content":"A wild quest appears!"},"cwd":"/Users/arthur/realm"}"#,
        )
        .unwrap();

        let index_json = format!(
            r#"{{
                "entries": [{{
                    "sessionId": "indexed-quest",
                    "fullPath": "{}",
                    "projectPath": "/Users/arthur/realm",
                    "summary": "The indexed quest",
                    "modified": "1975-04-03T10:00:00Z"
                }}]
            }}"#,
            indexed_session.display()
        );
        fs::write(realm_dir.join("sessions-index.json"), index_json).unwrap();

        let sessions = find_sessions(&temp_dir).unwrap();

        assert_eq!(sessions.len(), 2);

        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"indexed-quest"));
        assert!(ids.contains(&"orphan-quest"));

        let indexed = sessions.iter().find(|s| s.id == "indexed-quest").unwrap();
        assert_eq!(indexed.summary, Some("The indexed quest".to_string()));

        let orphan = sessions.iter().find(|s| s.id == "orphan-quest").unwrap();
        assert!(orphan.summary.is_none());
        assert_eq!(
            orphan.first_message,
            Some("A wild quest appears!".to_string())
        );

        fs::remove_dir_all(&temp_dir).unwrap();
    }
}
