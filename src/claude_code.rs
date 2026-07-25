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

use crate::message_classification::{
    counts_as_turn, is_first_prompt_candidate, is_system_content_for_preview,
};
use crate::session::{Session, SessionSource};
use anyhow::{Context, Result};
use memchr::memmem;
use rayon::prelude::*;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
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

    // Load local sessions (unsorted — final sort happens once at the end)
    if should_include_source(remote_filter, "local") {
        let local_dir = get_claude_projects_dir()?;
        if local_dir.exists() {
            summary
                .sessions
                .extend(find_sessions_with_source(&local_dir, SessionSource::Local)?);
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

    summary
        .sessions
        .sort_by_key(|s| std::cmp::Reverse(s.modified));
    Ok(summary)
}

// =============================================================================
// Session Loading
// =============================================================================

/// Find all sessions by scanning .jsonl files directly (sorted newest-first).
#[cfg(test)]
pub fn find_sessions(projects_dir: &Path) -> Result<Vec<Session>> {
    let mut sessions = find_sessions_with_source(projects_dir, SessionSource::Local)?;
    sessions.sort_by_key(|s| std::cmp::Reverse(s.modified));
    Ok(sessions)
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
        .map(|e| e.into_path())
        .collect();

    // File sizes are wildly skewed (sessions range from a few KB to hundreds of
    // MB). Force per-item task granularity so rayon can steal individual files;
    // the default recursive-split chunking bundles multiple large files into one
    // unstealable range and stalls other workers.
    let sessions: Vec<Session> = jsonl_files
        .into_par_iter()
        .with_max_len(1)
        .filter_map(|filepath| extract_session_metadata(filepath, &source))
        .collect();

    Ok(sessions)
}

/// Check if a string is a valid UUID (8-4-4-4-12 format with hex chars)
fn is_valid_session_uuid(s: &str) -> bool {
    const DASH_POSITIONS: [usize; 4] = [8, 13, 18, 23];
    let bytes = s.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, &b)| {
            if DASH_POSITIONS.contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
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
fn extract_session_metadata(filepath: PathBuf, source: &SessionSource) -> Option<Session> {
    let id = filepath.file_stem()?.to_string_lossy().into_owned();

    let metadata = fs::metadata(&filepath).ok()?;
    let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
    // Birthtime is meaningless for rsynced cache copies (it's when the local
    // file was written, not when the remote session began). Fall back to mtime.
    let created = match source {
        SessionSource::Local => metadata.created().unwrap_or(modified),
        SessionSource::Remote { .. } => modified,
    };

    let scan = scan_session_file(&filepath);

    if scan.skip {
        return None;
    }

    // Skip "empty" sessions that have no user content. `ai_title` deliberately
    // does not count: background-agent stubs carry one with no conversation at
    // all, and listing those is what this filter exists to prevent.
    if scan.project_path.is_empty() && scan.first_prompt.is_none() && scan.summary.is_none() {
        return None;
    }

    let parent_dir_name = filepath.parent()?.file_name()?.to_string_lossy();
    let project = extract_project_name(&scan.project_path, &parent_dir_name);

    Some(Session {
        id,
        project,
        project_path: scan.project_path,
        filepath,
        created,
        modified,
        first_message: scan.first_prompt,
        summary: scan.summary,
        ai_title: scan.ai_title,
        name: scan.custom_title,
        tag: scan.tag,
        turn_count: scan.turn_count,
        source: source.clone(),
        forked_from: scan.forked_from,
    })
}

/// Output of single-pass scan over a session file.
#[derive(Default)]
struct SessionScan {
    project_path: String,
    first_prompt: Option<String>,
    forked_from: Option<String>,
    turn_count: usize,
    summary: Option<String>,
    ai_title: Option<String>,
    custom_title: Option<String>,
    tag: Option<String>,
    /// Session should be excluded from the picker (sidechain or swarm-teammate).
    skip: bool,
}

/// Number of lines to parse fully before the byte-level prefilter engages.
/// Session-level metadata (cwd, forkedFrom, isSidechain, teamName) is stamped on
/// every entry, so it is reliably present within the first handful of lines.
const HEADER_SCAN_LINES: usize = 16;

/// Whether an entry is a synthetic user message (attachment context, proactive
/// ticks, post-compaction summaries) rather than real user input. Such entries
/// still carry session-level metadata (cwd, forkedFrom), but their content is
/// excluded from first-prompt, turn-count, and search text.
fn is_synthetic_entry(entry: &serde_json::Value) -> bool {
    entry.get("isMeta").and_then(|v| v.as_bool()) == Some(true)
        || entry.get("isCompactSummary").and_then(|v| v.as_bool()) == Some(true)
}

impl SessionScan {
    /// Fold one raw transcript line (1-based `line_no`) into the accumulator.
    ///
    /// This is the pure, I/O-free core of the session scanner: it borrows the
    /// line only for the duration of the call, so the caller can drive it with a
    /// single reused read buffer (zero per-line allocation) or, in tests, with a
    /// plain `&str` split into lines. `ControlFlow::Break` signals an early stop
    /// (the session is a sidechain/teammate transcript we discard wholesale).
    fn ingest_line(&mut self, line: &str, line_no: usize) -> ControlFlow<()> {
        // Past the header window, only parse lines that mention a content-bearing
        // entry type. This skips ~99% of lines in progress-heavy sessions.
        if line_no > HEADER_SCAN_LINES && !line_mentions_content_type(line.as_bytes()) {
            return ControlFlow::Continue(());
        }

        let entry: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return ControlFlow::Continue(()),
        };

        // Sidechain (subagent) and teammate (swarm) sessions can both land in
        // the main project dir as UUID-named files. Bail early — they can be
        // large and we're discarding them anyway.
        if entry.get("isSidechain").and_then(|v| v.as_bool()) == Some(true)
            || entry.get("teamName").and_then(|v| v.as_str()).is_some()
        {
            self.skip = true;
            return ControlFlow::Break(());
        }

        let entry_type = entry.get("type").and_then(|v| v.as_str());

        match entry_type {
            Some("summary") => {
                if let Some(s) = entry.get("summary").and_then(|v| v.as_str()) {
                    self.summary = Some(s.to_owned());
                }
                return ControlFlow::Continue(());
            }
            Some("ai-title") => {
                // Empty title would blank the row and mask the first message,
                // so it is treated as absent rather than as a label.
                if let Some(t) = entry.get("aiTitle").and_then(|v| v.as_str())
                    && !t.is_empty()
                {
                    self.ai_title = Some(t.to_owned());
                }
                return ControlFlow::Continue(());
            }
            Some("custom-title") => {
                if let Some(t) = entry.get("customTitle").and_then(|v| v.as_str()) {
                    self.custom_title = Some(t.to_owned());
                }
                return ControlFlow::Continue(());
            }
            Some("tag") => {
                // Empty string = explicit removal. Missing field = malformed,
                // preserve existing (matches summary/custom-title semantics).
                if let Some(t) = entry.get("tag").and_then(|v| v.as_str()) {
                    self.tag = (!t.is_empty()).then(|| t.to_owned());
                }
                return ControlFlow::Continue(());
            }
            _ => {}
        }

        if self.project_path.is_empty()
            && let Some(cwd) = entry.get("cwd").and_then(|v| v.as_str())
        {
            self.project_path = cwd.to_owned();
        }

        if self.forked_from.is_none()
            && let Some(parent_id) = entry
                .get("forkedFrom")
                .and_then(|f| f.get("sessionId"))
                .and_then(|v| v.as_str())
        {
            self.forked_from = Some(parent_id.to_owned());
        }

        if is_synthetic_entry(&entry) {
            return ControlFlow::Continue(());
        }

        if entry_type == Some("user")
            && let Some(content) = entry.get("message").and_then(|m| m.get("content"))
            && let Some(first) = iter_text_blocks(content).next()
        {
            if self.first_prompt.is_none() && is_first_prompt_candidate(first) {
                self.first_prompt = Some(crate::normalize_summary(first, 120));
            }
            if counts_as_turn(first) {
                self.turn_count += 1;
            }
        }

        ControlFlow::Continue(())
    }
}

/// Scan transcript content from any buffered reader.
///
/// This is the single production drive loop (line numbering, one reused line
/// buffer, early stop) shared by the file shell and the unit tests, so both
/// exercise identical semantics; only `File::open` differs between them.
fn scan_from_reader(mut reader: impl BufRead) -> SessionScan {
    let mut scan = SessionScan::default();
    let mut line = String::new();
    let mut line_no = 0usize;

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        line_no += 1;

        if scan.ingest_line(&line, line_no).is_break() {
            break;
        }
    }

    scan
}

/// Scan a session file once to collect all metadata and turn count.
/// Thin I/O shell over [`scan_from_reader`]: single file open, single pass.
fn scan_session_file(filepath: &Path) -> SessionScan {
    let Ok(file) = File::open(filepath) else {
        return SessionScan::default();
    };
    scan_from_reader(BufReader::with_capacity(64 * 1024, file))
}

// =============================================================================
// Search Index (built lazily, off the discovery hot path)
// =============================================================================

/// Lowercase transcript text keyed by session ID, for Ctrl+S filtering.
pub type SearchIndex = std::collections::HashMap<String, String>;

/// Build the transcript search index for the given sessions in parallel.
/// Intended to run on a background thread after the picker has rendered.
pub fn build_search_index(targets: Vec<(String, PathBuf)>) -> SearchIndex {
    targets
        .into_par_iter()
        .with_max_len(1)
        .map(|(id, path)| (id, scan_search_text(&path)))
        .collect()
}

/// Fold one raw transcript line into the lowercase search buffer.
///
/// Pure, I/O-free core of the search-index builder (mirrors
/// [`SessionScan::ingest_line`]): borrows the line only for the call so the
/// driver can reuse a single read buffer.
fn ingest_search_line(out: &mut String, line: &str) {
    if !line_mentions_content_type(line.as_bytes()) {
        return;
    }
    let entry: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return,
    };

    if is_synthetic_entry(&entry) {
        return;
    }

    let entry_type = entry.get("type").and_then(|v| v.as_str());
    let is_user = entry_type == Some("user");
    let content = match entry_type {
        Some("user") | Some("assistant") => entry.get("message").and_then(|m| m.get("content")),
        _ => None,
    };
    let Some(content) = content else { return };

    let mut blocks = iter_text_blocks(content);
    let Some(first) = blocks.next() else { return };

    // Keep search index aligned with preview: skip system-tag user
    // payloads so Ctrl+S matches only what the preview will show.
    if is_user && is_system_content_for_preview(first) {
        return;
    }

    append_lowercase(out, first);
    for text in blocks {
        append_lowercase(out, text);
    }
}

/// Build lowercase search text from any buffered reader.
///
/// Single production drive loop shared by the file shell and the unit tests
/// (see [`scan_from_reader`] for the rationale).
fn search_text_from_reader(mut reader: impl BufRead) -> String {
    let mut line = String::new();
    let mut out = String::new();

    while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
        ingest_search_line(&mut out, &line);
        line.clear();
    }

    out.shrink_to_fit();
    out
}

/// Extract lowercase transcript text from a single session file.
/// Thin I/O shell over [`search_text_from_reader`].
fn scan_search_text(filepath: &Path) -> String {
    let Ok(file) = File::open(filepath) else {
        return String::new();
    };
    search_text_from_reader(BufReader::with_capacity(64 * 1024, file))
}

static TYPE_KEY_FINDER: LazyLock<memmem::Finder<'static>> =
    LazyLock::new(|| memmem::Finder::new(br#""type":""#));

/// Cheap scan for a content-bearing entry type. SIMD-accelerated walk of
/// `"type":"` markers left to right; the entry-level type appears before any
/// nested `data.type`, so the first hit usually decides the line. False
/// positives are harmless — we'd just parse that line unnecessarily.
fn line_mentions_content_type(line: &[u8]) -> bool {
    let needle_len = TYPE_KEY_FINDER.needle().len();
    let mut haystack = line;
    while let Some(pos) = TYPE_KEY_FINDER.find(haystack) {
        let after = &haystack[pos + needle_len..];
        let is_content = match after.first() {
            Some(&b'u') => after.starts_with(b"user\""),
            Some(&b'a') => after.starts_with(b"assistant\"") || after.starts_with(b"ai-title\""),
            Some(&b's') => after.starts_with(b"summary\""),
            Some(&b'c') => after.starts_with(b"custom-title\""),
            Some(&b't') => after.starts_with(b"tag\""),
            _ => false,
        };
        if is_content {
            return true;
        }
        haystack = after;
    }
    false
}

/// Append `text` to `buf` lowercased, separated by a newline. Avoids the
/// intermediate `Vec<String>` + `join` + `to_lowercase` triple-allocation
/// the previous implementation performed per file.
fn append_lowercase(buf: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    if !buf.is_empty() {
        buf.push('\n');
    }
    let start = buf.len();
    buf.push_str(text);
    // SAFETY: make_ascii_lowercase only flips bit 0x20 on uppercase ASCII and
    // leaves all other bytes (including UTF-8 continuation bytes) untouched,
    // so the buffer remains valid UTF-8. Non-ASCII uppercase is left as-is —
    // a reasonable trade since Ctrl+S search compares against the user's own
    // transcript text and the quadratic cost of full Unicode case-folding
    // dominates on multi-GB corpora.
    unsafe { buf.as_bytes_mut()[start..].make_ascii_lowercase() };
}

/// Iterate over all text blocks in a message content value (string or array
/// of content blocks), borrowing from the underlying JSON.
fn iter_text_blocks(content: &serde_json::Value) -> impl Iterator<Item = &str> {
    let single = content.as_str();
    let blocks = content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c.get("type").and_then(|v| v.as_str()) == Some("text"))
        .filter_map(|c| c.get("text").and_then(|v| v.as_str()));
    single.into_iter().chain(blocks)
}

/// Extract the first text block from message content, borrowing from the JSON.
fn first_text_block(content: &serde_json::Value) -> Option<&str> {
    iter_text_blocks(content).next()
}

// =============================================================================
// Transcript Messages (input for preview/search rendering)
// =============================================================================

/// Role of a transcript message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

/// A displayable user/assistant message extracted from a session transcript.
#[derive(Debug)]
pub struct Message {
    pub role: Role,
    pub text: String,
}

/// How much of each message's text [`read_messages`] retains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextDetail {
    /// Only the first line. The scrollback preview shows one line per message,
    /// so cloning full bodies (which can be very large) would be wasted work
    /// on skim's per-keystroke preview path.
    FirstLine,
    /// Full text. The search preview must match and display every line.
    Full,
}

/// Fold one raw transcript line into the message list. Pure — no I/O.
///
/// Keeps only user/assistant entries with a text payload, and drops
/// system-generated user payloads (slash commands, command tags,
/// request-interrupt notices) so renderers operate on clean conversational
/// text.
fn ingest_message_line(messages: &mut Vec<Message>, line: &str, detail: TextDetail) {
    if !line_mentions_content_type(line.as_bytes()) {
        return;
    }
    let entry: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return,
    };

    let role = match entry.get("type").and_then(|v| v.as_str()) {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        _ => return,
    };

    let Some(text) = entry
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(first_text_block)
    else {
        return;
    };
    if role == Role::User && is_system_content_for_preview(text) {
        return;
    }

    let text = match detail {
        TextDetail::FirstLine => text.lines().next().unwrap_or(text),
        TextDetail::Full => text,
    };
    messages.push(Message {
        role,
        text: text.to_owned(),
    });
}

/// Read user/assistant messages from transcript content (shared production
/// loop for the file shell and tests).
///
/// `limit` caps how many displayable messages are returned: `Some(n)` stops
/// reading after `n` (scrollback preview), `None` reads the whole transcript
/// (search, which must find matches anywhere).
pub fn read_messages_from_reader(
    mut reader: impl BufRead,
    limit: Option<usize>,
    detail: TextDetail,
) -> Vec<Message> {
    let mut messages = Vec::new();
    let mut line = String::new();

    while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
        if limit.is_some_and(|lim| messages.len() >= lim) {
            break;
        }
        ingest_message_line(&mut messages, &line, detail);
        line.clear();
    }

    messages
}

/// Read user/assistant messages from a session transcript file.
/// Thin I/O shell over [`read_messages_from_reader`].
pub fn read_messages(
    filepath: &Path,
    limit: Option<usize>,
    detail: TextDetail,
) -> Result<Vec<Message>> {
    let file = File::open(filepath).context("Could not open session file")?;
    Ok(read_messages_from_reader(
        BufReader::with_capacity(64 * 1024, file),
        limit,
        detail,
    ))
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
            .trim_end_matches('/')
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

    /// Scan inline JSONL content through the production reader loop — no disk
    /// I/O (a `&[u8]` is a `BufRead`), no duplicated test driver.
    fn scan(content: &str) -> SessionScan {
        scan_from_reader(content.as_bytes())
    }

    /// Build search text from inline JSONL content via the production loop.
    fn search_text_from_str(content: &str) -> String {
        search_text_from_reader(content.as_bytes())
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
    fn extract_project_name_handles_trailing_slash() {
        assert_eq!(
            extract_project_name("/Users/foo/my-project/", "ignored"),
            "my-project"
        );
        assert_eq!(
            extract_project_name("/Users/foo/my-project///", "ignored"),
            "my-project"
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
        let scan = scan(
            r#"{"type":"user","message":{"role":"user","content":"hello"},"forkedFrom":{"sessionId":"parent-1","messageUuid":"m1"}}
{"type":"assistant","message":"hi"}
{"type":"user","message":{"role":"user","content":"later"},"forkedFrom":{"sessionId":"parent-2","messageUuid":"m2"}}"#,
        );
        assert_eq!(scan.forked_from, Some("parent-1".to_string()));
    }

    #[test]
    fn scan_extracts_forked_from_on_later_line() {
        let scan = scan(
            r#"{"type":"progress","data":"starting"}
{"type":"progress","cwd":"/Users/test/project","data":"hook"}
{"type":"user","message":{"role":"user","content":"hello"},"forkedFrom":{"sessionId":"parent-session-id","messageUuid":"msg1"}}
{"type":"assistant","message":"hi"}"#,
        );
        assert_eq!(scan.project_path, "/Users/test/project");
        assert_eq!(scan.forked_from, Some("parent-session-id".to_string()));
        assert_eq!(scan.first_prompt, Some("hello".to_string()));
    }

    // =========================================================================
    // Turn counting - only real user messages, not system content
    // =========================================================================

    #[test]
    fn count_turns_real_user_messages() {
        let scan = scan(
            r#"{"type":"user","message":{"role":"user","content":"Hello, how are you?"}}
{"type":"assistant","message":{"role":"assistant","content":"I'm good!"}}
{"type":"user","message":{"role":"user","content":"What is Rust?"}}
{"type":"assistant","message":{"role":"assistant","content":"A programming language."}}"#,
        );
        assert_eq!(scan.turn_count, 2);
    }

    #[test]
    fn count_turns_excludes_system_content() {
        let scan = scan(
            r#"{"type":"user","message":{"role":"user","content":"<command-message>init</command-message>"}}
{"type":"user","message":{"role":"user","content":"Real message here"}}
{"type":"user","message":{"role":"user","content":"<local-command-stdout>output</local-command-stdout>"}}
{"type":"user","message":{"role":"user","content":"/help"}}
{"type":"user","message":{"role":"user","content":"[some bracketed thing]"}}
{"type":"user","message":{"role":"user","content":"Another real message"}}"#,
        );
        assert_eq!(scan.turn_count, 2);
    }

    #[test]
    fn count_turns_handles_content_blocks() {
        let scan = scan(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Hello from blocks"}]}}
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"<command-name>/init</command-name>"}]}}
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Real question?"}]}}"#,
        );
        assert_eq!(scan.turn_count, 2);
    }

    #[test]
    fn count_turns_empty_file() {
        assert_eq!(scan("").turn_count, 0);
    }

    #[test]
    fn first_prompt_and_turn_count_current_filter_behavior() {
        let scan = scan(
            r#"{"type":"user","message":{"role":"user","content":"[tool output that is not Request]"},"cwd":"/Users/test/project"}
{"type":"user","message":{"role":"user","content":"Real user question"}}"#,
        );
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
        let scan = scan(
            r#"{"type":"user","message":{"role":"user","content":"Real prompt"},"cwd":"/Users/test/project"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"assistant reply"}]}}
{"type":"user","message":{"role":"user","content":"/help"}}
{"type":"user","message":{"role":"user","content":"Second real prompt"}}"#,
        );
        assert_eq!(scan.project_path, "/Users/test/project");
        assert_eq!(scan.first_prompt, Some("Real prompt".to_string()));
        assert_eq!(scan.turn_count, 2);
    }

    #[test]
    fn scan_filters_sidechain_sessions() {
        let content = r#"{"type":"user","message":{"role":"user","content":"agent work"},"cwd":"/Users/test/proj","isSidechain":true}"#;
        // Pure: the line scanner flags the skip.
        assert!(scan(content).skip);
        // Integration: discovery drops the file entirely.
        let (_tmp, root) = project_fixture("-Users-test-proj", &test_uuid(50), content);
        assert_eq!(find_sessions(&root).unwrap().len(), 0);
    }

    #[test]
    fn ingest_line_break_short_circuits_remaining_lines() {
        // The sidechain entry returns ControlFlow::Break; any content after it
        // must never be folded into the accumulator.
        let scan = scan(
            r#"{"type":"user","message":{"role":"user","content":"agent work"},"isSidechain":true}
{"type":"user","message":{"role":"user","content":"should be ignored"},"cwd":"/tmp"}"#,
        );
        assert!(scan.skip);
        assert_eq!(scan.first_prompt, None);
        assert!(scan.project_path.is_empty());
    }

    #[test]
    fn scan_ignores_sidechain_false() {
        let scan = scan(
            r#"{"type":"user","message":{"role":"user","content":"hi"},"cwd":"/tmp","isSidechain":false}"#,
        );
        assert!(!scan.skip);
        assert_eq!(scan.project_path, "/tmp");
    }

    #[test]
    fn scan_filters_teammate_sessions() {
        let content = r#"{"type":"user","message":{"role":"user","content":"swarm work"},"cwd":"/tmp","teamName":"my-team","isSidechain":false}"#;
        // Pure: the line scanner flags the skip.
        assert!(scan(content).skip);
        // Integration: discovery drops the file entirely.
        let (_tmp, root) = project_fixture("-Users-test-proj", &test_uuid(51), content);
        assert_eq!(find_sessions(&root).unwrap().len(), 0);
    }

    #[test]
    fn scan_skips_meta_entries_for_first_prompt_and_turns() {
        let content = r#"{"type":"user","message":{"role":"user","content":"synthetic attachment context"},"cwd":"/tmp","isMeta":true}
{"type":"user","message":{"role":"user","content":"real user prompt"}}"#;
        let scan = scan(content);
        assert_eq!(scan.project_path, "/tmp");
        assert_eq!(scan.first_prompt, Some("real user prompt".to_string()));
        assert_eq!(scan.turn_count, 1);
        assert!(!search_text_from_str(content).contains("synthetic"));
    }

    #[test]
    fn scan_skips_compact_summary_entries() {
        let scan = scan(
            r#"{"type":"user","message":{"role":"user","content":"This session covers X and Y"},"cwd":"/tmp","isCompactSummary":true}
{"type":"user","message":{"role":"user","content":"actual question"}}"#,
        );
        assert_eq!(scan.first_prompt, Some("actual question".to_string()));
        assert_eq!(scan.turn_count, 1);
    }

    #[test]
    fn scan_takes_last_summary() {
        let scan = scan(
            r#"{"type":"summary","summary":"Early compaction"}
{"type":"user","message":{"role":"user","content":"more work"},"cwd":"/tmp"}
{"type":"summary","summary":"Final summary"}"#,
        );
        assert_eq!(scan.summary, Some("Final summary".to_string()));
    }

    #[test]
    fn scan_keeps_valid_summary_when_later_entry_malformed() {
        let scan = scan(
            r#"{"type":"summary","summary":"Valid"}
{"type":"summary"}"#,
        );
        assert_eq!(scan.summary, Some("Valid".to_string()));
    }

    #[test]
    fn scan_takes_last_ai_title() {
        let scan = scan(
            r#"{"type":"ai-title","aiTitle":"Early guess","sessionId":"x"}
{"type":"user","message":{"role":"user","content":"more work"},"cwd":"/tmp"}
{"type":"ai-title","aiTitle":"Claude Science research team findings","sessionId":"x"}"#,
        );
        assert_eq!(
            scan.ai_title,
            Some("Claude Science research team findings".to_string())
        );
    }

    /// `ai-title` entries land well past `HEADER_SCAN_LINES`, so the byte-level
    /// prefilter must recognise them or they are never parsed.
    #[test]
    fn scan_finds_ai_title_past_header_window() {
        let mut content = String::new();
        for i in 0..(HEADER_SCAN_LINES + 10) {
            content.push_str(&format!(
                r#"{{"type":"progress","step":{i},"cwd":"/tmp"}}
"#
            ));
        }
        content.push_str(r#"{"type":"ai-title","aiTitle":"Late title","sessionId":"x"}"#);

        let scan = scan(&content);
        assert_eq!(scan.ai_title, Some("Late title".to_string()));
    }

    #[test]
    fn scan_ignores_blank_ai_title() {
        let scan = scan(
            r#"{"type":"ai-title","aiTitle":"Real title","sessionId":"x"}
{"type":"ai-title","aiTitle":"","sessionId":"x"}"#,
        );
        assert_eq!(scan.ai_title, Some("Real title".to_string()));
    }

    /// An ai-title alone must not resurrect a contentless session: background
    /// agents write a title (and nothing else) into otherwise empty transcripts.
    #[test]
    fn ai_title_alone_does_not_qualify_empty_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("bd33d65e-a540-44ea-ac09-b4f46f8d035e.jsonl");
        fs::write(
            &path,
            r#"{"type":"ai-title","aiTitle":"resume-background-agent","sessionId":"bd33d65e"}
{"type":"agent-name","agentName":"resume-background-agent","sessionId":"bd33d65e"}"#,
        )
        .unwrap();

        assert!(extract_session_metadata(path, &SessionSource::Local).is_none());
    }

    /// A session whose only real prompt arrived via `/slash-command` args has no
    /// usable first message, but its ai-title still identifies it.
    #[test]
    fn ai_title_survives_slash_command_only_session() {
        let scan = scan(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"<command-name>/orchestrate</command-name>"}]},"cwd":"/tmp"}
{"type":"ai-title","aiTitle":"Deep research on Claude Science","sessionId":"x"}"#,
        );
        assert_eq!(scan.first_prompt, None);
        assert_eq!(
            scan.ai_title,
            Some("Deep research on Claude Science".to_string())
        );
    }

    #[test]
    fn scan_takes_last_custom_title() {
        let scan = scan(
            r#"{"type":"user","message":{"role":"user","content":"hello"},"cwd":"/tmp"}
{"type":"custom-title","customTitle":"Old Name","sessionId":"x"}
{"type":"assistant","message":{"role":"assistant","content":"hi"}}
{"type":"custom-title","customTitle":"New Name","sessionId":"x"}"#,
        );
        assert_eq!(scan.custom_title, Some("New Name".to_string()));
    }

    #[test]
    fn search_text_includes_user_and_assistant_text() {
        let text = search_text_from_str(
            r#"{"type":"user","message":{"role":"user","content":"API status"}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Service healthy"}]}}
{"type":"summary","summary":"ignored summary"}"#,
        );
        assert!(text.contains("api status"));
        assert!(text.contains("service healthy"));
    }

    #[test]
    fn scan_tag_empty_string_clears_previous() {
        let scan = scan(
            r#"{"type":"tag","tag":"important","sessionId":"x"}
{"type":"user","message":{"role":"user","content":"work"},"cwd":"/tmp"}
{"type":"tag","tag":"","sessionId":"x"}"#,
        );
        assert_eq!(scan.tag, None);
    }

    #[test]
    fn scan_tag_missing_field_preserves_previous() {
        let scan = scan(
            r#"{"type":"tag","tag":"important","sessionId":"x"}
{"type":"tag","sessionId":"x"}"#,
        );
        assert_eq!(scan.tag, Some("important".to_string()));
    }

    #[test]
    fn search_text_excludes_system_tag_user_content() {
        let text = search_text_from_str(
            r#"{"type":"user","message":{"role":"user","content":"<command-message>deploy</command-message>"},"cwd":"/tmp"}
{"type":"user","message":{"role":"user","content":"real question about API"}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"answer"}]}}"#,
        );
        assert!(!text.contains("deploy"));
        assert!(text.contains("api"));
        assert!(text.contains("answer"));
    }

    #[test]
    fn line_filter_accepts_content_types() {
        for ty in ["user", "assistant", "summary", "custom-title", "tag"] {
            let line = format!(r#"{{"parentUuid":"x","type":"{ty}","cwd":"/tmp"}}"#);
            assert!(line_mentions_content_type(line.as_bytes()), "type: {ty}");
        }
    }

    #[test]
    fn line_filter_rejects_progress_types() {
        for ty in [
            "progress",
            "attachment",
            "system",
            "mode",
            "queue-operation",
        ] {
            let line = format!(r#"{{"type":"{ty}","data":{{"type":"sleep_progress"}}}}"#);
            assert!(!line_mentions_content_type(line.as_bytes()), "type: {ty}");
        }
    }

    #[test]
    fn line_filter_nested_type_ignored() {
        // A progress entry with a nested "type":"text" (not a content type) — skip.
        let line = br#"{"type":"progress","data":{"type":"text"}}"#;
        assert!(!line_mentions_content_type(line));
        // But a progress line carrying a nested "type":"user" is a false
        // positive we accept (harmless — we'd just parse it).
        let fp = br#"{"type":"progress","data":{"type":"user"}}"#;
        assert!(line_mentions_content_type(fp));
    }

    #[test]
    fn scan_filter_still_parses_header_lines() {
        // progress lines past HEADER_SCAN_LINES are skipped, but the first few
        // still feed cwd/forkedFrom/isSidechain detection.
        let mut content = String::new();
        content.push_str(
            r#"{"type":"progress","cwd":"/proj","isSidechain":false,"forkedFrom":{"sessionId":"p"}}"#,
        );
        content.push('\n');
        // pile of skippable progress
        for _ in 0..100 {
            content.push_str(r#"{"type":"progress","data":{"type":"sleep"}}"#);
            content.push('\n');
        }
        content.push_str(r#"{"type":"user","message":{"role":"user","content":"hello"}}"#);
        content.push('\n');
        content.push_str(r#"{"type":"summary","summary":"Done"}"#);
        content.push('\n');

        let scan = scan(&content);
        assert_eq!(scan.project_path, "/proj");
        assert_eq!(scan.forked_from, Some("p".to_string()));
        assert_eq!(scan.first_prompt, Some("hello".to_string()));
        assert_eq!(scan.summary, Some("Done".to_string()));
        assert_eq!(scan.turn_count, 1);
    }

    #[test]
    fn scan_tag_takes_last_non_empty() {
        let scan = scan(
            r#"{"type":"tag","tag":"old","sessionId":"x"}
{"type":"tag","tag":"new","sessionId":"x"}"#,
        );
        assert_eq!(scan.tag, Some("new".to_string()));
    }

    // =========================================================================
    // Transcript message reading (preview/search input)
    // =========================================================================

    const MESSAGES_FIXTURE: &str = r#"{"type":"user","message":{"role":"user","content":"first question\nwith a second line"}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"long answer\nbody continues"}]}}
{"type":"user","message":{"role":"user","content":"/help"}}
{"type":"user","message":{"role":"user","content":"second question"}}"#;

    #[test]
    fn read_messages_full_keeps_bodies_and_skips_system_content() {
        let messages =
            read_messages_from_reader(MESSAGES_FIXTURE.as_bytes(), None, TextDetail::Full);
        // The /help slash command is dropped; real messages keep full text.
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[0].text, "first question\nwith a second line");
        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(messages[1].text, "long answer\nbody continues");
        assert_eq!(messages[2].text, "second question");
    }

    #[test]
    fn read_messages_first_line_detail_truncates_bodies() {
        let messages =
            read_messages_from_reader(MESSAGES_FIXTURE.as_bytes(), None, TextDetail::FirstLine);
        assert_eq!(messages[0].text, "first question");
        assert_eq!(messages[1].text, "long answer");
    }

    #[test]
    fn read_messages_limit_caps_collected_messages() {
        let messages =
            read_messages_from_reader(MESSAGES_FIXTURE.as_bytes(), Some(1), TextDetail::Full);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "first question\nwith a second line");
    }
}
