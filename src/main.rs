mod claude_code;
mod remote;

use anyhow::{Context, Result};
use clap::Parser;
use skim::prelude::*;
use std::borrow::Cow;
use std::path::PathBuf;
use std::time::SystemTime;

// =============================================================================
// CLI Interface
// =============================================================================

#[derive(Parser)]
#[command(
    name = "cc-sessions",
    about = "List and resume Claude Code sessions across projects and machines"
)]
struct Args {
    // -------------------------------------------------------------------------
    // Mode
    // -------------------------------------------------------------------------

    /// List mode: print sessions as a table (no picker, no preview). Use without --list for interactive picker
    #[arg(long, help_heading = "Mode")]
    list: bool,

    /// Number of sessions to show [default: 15]. List only (ignored in interactive mode)
    #[arg(long, default_value = "15", help_heading = "Mode")]
    count: usize,

    // -------------------------------------------------------------------------
    // Interactive-only (ignored with --list)
    // -------------------------------------------------------------------------

    /// Fork session instead of resuming (creates new session ID). Interactive only; ignored with --list
    #[arg(long, help_heading = "Interactive only")]
    fork: bool,

    /// Show session ID prefixes and extra stats. Works in both modes
    #[arg(long, help_heading = "Interactive only")]
    debug: bool,

    // -------------------------------------------------------------------------
    // List-only
    // -------------------------------------------------------------------------

    /// Include forked sessions in the table. List only (interactive mode shows forks via → navigation)
    #[arg(long, help_heading = "List only")]
    include_forks: bool,

    // -------------------------------------------------------------------------
    // Filtering (both modes)
    // -------------------------------------------------------------------------

    /// Filter by project name (substring match, case-insensitive)
    #[arg(long, help_heading = "Filtering")]
    project: Option<String>,

    /// Minimum number of conversation turns (filters out one-shot sessions)
    #[arg(long, help_heading = "Filtering")]
    min_turns: Option<usize>,

    /// Filter to sessions from a specific remote (e.g. devbox) or "local"
    #[arg(long, value_name = "NAME", help_heading = "Filtering")]
    remote: Option<String>,

    // -------------------------------------------------------------------------
    // Remote sync
    // -------------------------------------------------------------------------

    /// Force sync all remotes before listing
    #[arg(long, help_heading = "Remote sync")]
    sync: bool,

    /// Skip auto-sync (use cached remote data only)
    #[arg(long, help_heading = "Remote sync")]
    no_sync: bool,

    /// Sync all remotes and exit; no listing or picker (e.g. for cron). Other flags ignored
    #[arg(long, help_heading = "Remote sync")]
    sync_only: bool,

    // -------------------------------------------------------------------------
    // Internal (hidden from --help)
    // -------------------------------------------------------------------------

    /// Preview a session file (used internally by interactive picker)
    #[arg(long, value_name = "FILE", hide = true)]
    preview: Option<PathBuf>,
}

// =============================================================================
// Session Model (abstraction layer)
// =============================================================================

/// Where a session originated from
#[derive(Debug, Clone)]
pub enum SessionSource {
    /// Local session from ~/.claude/projects
    Local,
    /// Remote session synced via SSH
    Remote {
        /// Config key (e.g., "devbox")
        name: String,
        /// SSH alias or raw hostname/IP
        host: String,
        /// Only needed for raw hosts without SSH config
        user: Option<String>,
    },
}

impl SessionSource {
    /// Display name for the source (e.g., "local", "devbox")
    pub fn display_name(&self) -> &str {
        match self {
            SessionSource::Local => "local",
            SessionSource::Remote { name, .. } => name,
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, SessionSource::Local)
    }
}

#[derive(Debug)]
pub struct Session {
    pub id: String,
    pub project: String,
    pub project_path: String,
    pub filepath: PathBuf,
    pub created: SystemTime,
    pub modified: SystemTime,
    pub first_message: Option<String>,
    pub summary: Option<String>,
    pub name: Option<String>,       // customTitle from /rename - indicates important session
    pub turn_count: usize,          // Number of user messages (conversation turns)
    pub source: SessionSource,      // Where this session came from
    pub forked_from: Option<String>, // Parent session ID if this is a fork
}

// =============================================================================
// Main Entry Point
// =============================================================================

fn main() -> Result<()> {
    let args = Args::parse();

    // Preview mode: output formatted transcript for a session file
    if let Some(ref filepath) = args.preview {
        print_session_preview(filepath)?;
        return Ok(());
    }

    // Load remote config
    let config = remote::load_config()?;

    // Handle sync operations
    if args.sync_only {
        // Sync all remotes and exit
        let results = remote::sync_all(&config)?;
        for result in &results {
            println!(
                "Synced '{}' in {:.1}s",
                result.remote_name,
                result.duration.as_secs_f64()
            );
        }
        if results.is_empty() {
            println!("No remotes configured. Add remotes to ~/.config/cc-sessions/remotes.toml");
        }
        return Ok(());
    }

    if args.sync {
        // Force sync all remotes
        let results = remote::sync_all(&config)?;
        for result in &results {
            eprintln!(
                "Synced '{}' in {:.1}s",
                result.remote_name,
                result.duration.as_secs_f64()
            );
        }
    } else if !args.no_sync && !config.remotes.is_empty() {
        // Auto-sync stale remotes
        let results = remote::sync_if_stale(&config)?;
        for result in &results {
            eprintln!(
                "Auto-synced '{}' in {:.1}s",
                result.remote_name,
                result.duration.as_secs_f64()
            );
        }
    }

    // Find sessions from all sources (local + remotes)
    let mut sessions = claude_code::find_all_sessions(&config, args.remote.as_deref())?;

    // Filter by project name if specified
    if let Some(ref filter) = args.project {
        let filter_lower = filter.to_lowercase();
        sessions.retain(|s| s.project.to_lowercase().contains(&filter_lower));
    }

    // Filter by minimum turns (excludes one-shot sessions)
    if let Some(min) = args.min_turns {
        sessions.retain(|s| s.turn_count >= min);
    }

    if sessions.is_empty() {
        if args.project.is_some() {
            anyhow::bail!("No sessions found matching project filter");
        }
        if let Some(ref remote_name) = args.remote {
            anyhow::bail!("No sessions found for remote '{}'", remote_name);
        }
        anyhow::bail!("No sessions found");
    }

    if args.list {
        let list_sessions = filter_forks_for_list(&sessions, args.include_forks);
        print_sessions(&list_sessions, args.count, args.debug);
    } else {
        interactive_mode(&sessions, args.fork, args.debug, &config)?;
    }

    Ok(())
}

// =============================================================================
// Display Functions
// =============================================================================

fn print_sessions(sessions: &[&Session], count: usize, debug: bool) {
    if debug {
        println!(
            "{:<6} {:<6} {:<4} {:<8} {:<16} {:<40} SUMMARY",
            "CREAT", "MOD", "FORK", "SOURCE", "PROJECT", "ID"
        );
        println!("{}", "─".repeat(130));

        for session in sessions.iter().take(count) {
            let created = format_time_relative(session.created);
            let modified = format_time_relative(session.modified);
            let source = session.source.display_name();
            let fork_indicator = if session.forked_from.is_some() { "↳" } else { "" };
            let id_short = if session.id.len() > 36 {
                &session.id[..36]
            } else {
                &session.id
            };
            let desc = format_session_desc(session, 30);

            println!(
                "{:<6} {:<6} {:<4} {:<8} {:<16} {:<40} {}",
                created, modified, fork_indicator, source, session.project, id_short, desc
            );
        }

        println!("{}", "─".repeat(130));
        println!("Total: {} sessions", sessions.len());
    } else {
        println!(
            "{:<6} {:<6} {:<8} {:<16} SUMMARY",
            "CREAT", "MOD", "SOURCE", "PROJECT"
        );
        println!("{}", "─".repeat(100));

        for session in sessions.iter().take(count) {
            let created = format_time_relative(session.created);
            let modified = format_time_relative(session.modified);
            let source = session.source.display_name();
            let desc = format_session_desc(session, 50);
            let desc = if session.forked_from.is_some() {
                format!("↳ {}", desc)
            } else {
                desc
            };

            println!(
                "{:<6} {:<6} {:<8} {:<16} {}",
                created, modified, source, session.project, desc
            );
        }

        println!("{}", "─".repeat(100));
        println!("Run without --list for interactive picker; use --fork to fork when resuming");
    }
}

fn format_time_relative(time: SystemTime) -> String {
    let now = SystemTime::now();

    // Handle future timestamps (clock skew, filesystem issues)
    let secs = match now.duration_since(time) {
        Ok(d) => d.as_secs(),
        Err(_) => return "?".to_string(), // Future timestamp
    };

    if secs < 60 {
        "now".to_string()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else if secs < 604800 {
        format!("{}d", secs / 86400)
    } else {
        format!("{}w", secs / 604800)
    }
}

/// Format session description: show name (★) if present, otherwise summary/first_message
fn format_session_desc(session: &Session, max_chars: usize) -> String {
    // Named sessions show ★ prefix with name, then summary if space allows
    if let Some(ref name) = session.name {
        let prefix = format!("★ {}", name);
        let prefix_len = prefix.chars().count();

        if prefix_len >= max_chars {
            return prefix.chars().take(max_chars).collect();
        }

        // Append summary if there's enough room
        match &session.summary {
            Some(summary) if max_chars > prefix_len + 13 => {
                // " - " + at least 10 chars
                let remaining = max_chars - prefix_len - 3;
                let summary_truncated: String = summary.chars().take(remaining).collect();
                format!("{} - {}", prefix, summary_truncated)
            }
            _ => prefix,
        }
    } else {
        // No name - use summary or first_message
        session
            .summary
            .as_deref()
            .or(session.first_message.as_deref())
            .map(|s| s.chars().take(max_chars).collect())
            .unwrap_or_default()
    }
}

fn filter_forks_for_list(sessions: &[Session], include_forks: bool) -> Vec<&Session> {
    if include_forks {
        return sessions.iter().collect();
    }

    sessions
        .iter()
        .filter(|s| s.forked_from.is_none())
        .collect()
}

/// Normalize text for display: collapse whitespace, strip markdown, truncate gracefully
pub fn normalize_summary(text: &str, max_chars: usize) -> String {
    // Collapse all whitespace (including newlines) to single spaces
    let normalized: String = text.split_whitespace().collect::<Vec<_>>().join(" ");

    // Strip common markdown prefixes
    let stripped = normalized.trim_start_matches(['#', '*']).trim_start();

    // Fast path for short strings
    if stripped.chars().count() <= max_chars {
        return stripped.to_string();
    }

    // Truncate at char boundary, break at word if possible
    let truncated: String = stripped.chars().take(max_chars).collect();

    // Try to break at last word boundary (if past halfway point)
    let break_point = truncated
        .rmatch_indices(' ')
        .next()
        .filter(|(i, _)| *i > max_chars / 2)
        .map(|(i, _)| i)
        .unwrap_or(truncated.len());

    format!("{}...", &truncated[..break_point])
}

// =============================================================================
// ANSI Colors (shared across preview functions)
// =============================================================================

mod colors {
    pub const CYAN: &str = "\x1b[36m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const GREEN: &str = "\x1b[32m";
    pub const DIM: &str = "\x1b[2m";
    pub const BOLD: &str = "\x1b[1m";
    pub const BOLD_INVERSE: &str = "\x1b[1;7m";
    pub const RESET: &str = "\x1b[0m";
}

// =============================================================================
// Preview Mode (internal, replaces jaq dependency)
// =============================================================================

/// Print formatted transcript preview for a session file.
/// Used internally by skim's preview command.
fn print_session_preview(filepath: &PathBuf) -> Result<()> {
    let content = generate_preview_content(filepath)?;
    print!("{}", content);
    Ok(())
}

/// Extract text content from a message entry
fn extract_message_text(entry: &serde_json::Value) -> Option<String> {
    let content = entry.get("message")?.get("content")?;

    // Content can be a string or array of content blocks
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

/// Truncate string to max chars, adding ... if truncated
fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(max).collect::<String>())
    }
}

/// Generate preview content as a string (for skim's preview pane)
fn generate_preview_content(filepath: &PathBuf) -> Result<String> {
    use std::fs::File;
    use std::io::{BufRead, BufReader};

    let file = File::open(filepath).context("Could not open session file")?;
    let reader = BufReader::new(file);

    let mut output = String::new();
    let mut line_count = 0;
    const MAX_LINES: usize = 100;

    for line in reader.lines().map_while(Result::ok) {
        if line_count >= MAX_LINES {
            break;
        }

        let entry: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let entry_type = entry.get("type").and_then(|v| v.as_str());

        match entry_type {
            Some("user") => {
                if let Some(text) = extract_message_text(&entry) {
                    if !is_system_content(&text) {
                        let first_line = text.lines().next().unwrap_or(&text);
                        let truncated = truncate_str(first_line, 120);
                        output.push_str(&format!(
                            "{}U: {}{}\n",
                            colors::CYAN, truncated, colors::RESET
                        ));
                        line_count += 1;
                    }
                }
            }
            Some("assistant") => {
                if let Some(text) = extract_message_text(&entry) {
                    let first_line = text.lines().next().unwrap_or(&text);
                    let truncated = truncate_str(first_line, 80);
                    output.push_str(&format!(
                        "{}A: {}{}\n",
                        colors::YELLOW, truncated, colors::RESET
                    ));
                    line_count += 1;
                }
            }
            _ => {}
        }
    }

    if output.is_empty() {
        output.push_str("(empty session)");
    }

    Ok(output)
}

/// Check if content is system/XML content that should be skipped in previews
fn is_system_content(text: &str) -> bool {
    text.starts_with('[') || text.starts_with('<') || text.starts_with('/')
}

/// A message from the transcript
struct Message {
    role: String, // "user" or "assistant"
    text: String,
}

/// Generate preview showing matching messages with full conversation context
fn generate_search_preview(filepath: &PathBuf, pattern: &str) -> Result<String> {
    use std::fs::File;
    use std::io::{BufRead, BufReader};

    let file = File::open(filepath).context("Could not open session file")?;
    let reader = BufReader::new(file);

    // Collect all messages first
    let mut messages: Vec<Message> = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        let entry: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let entry_type = entry.get("type").and_then(|v| v.as_str());
        let role = match entry_type {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => continue,
        };

        if let Some(text) = extract_message_text(&entry) {
            // Skip system prompts and XML content for user messages
            if role == "user" && is_system_content(&text) {
                continue;
            }
            messages.push(Message {
                role: role.to_string(),
                text,
            });
        }
    }

    let pattern_lower = pattern.to_lowercase();
    let mut output = String::new();
    let mut match_count = 0;
    const MAX_MATCHES: usize = 10; // Fewer matches since we show full context

    output.push_str(&format!(
        "{}Searching for: \"{}\"{}\n\n",
        colors::GREEN, pattern, colors::RESET
    ));

    // Find messages containing the pattern
    let matching_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.text.to_lowercase().contains(&pattern_lower))
        .map(|(i, _)| i)
        .collect();

    // Show each match with surrounding context
    let mut shown_indices: std::collections::HashSet<usize> = std::collections::HashSet::new();

    for &match_idx in &matching_indices {
        if match_count >= MAX_MATCHES {
            output.push_str(&format!(
                "\n{}... more matches truncated{}\n",
                colors::BOLD, colors::RESET
            ));
            break;
        }

        // Skip if we already showed this message as context
        if shown_indices.contains(&match_idx) {
            continue;
        }

        // Separator between match groups
        if match_count > 0 {
            output.push_str(&format!(
                "\n{}════════════════════════════════{}\n\n",
                colors::DIM, colors::RESET
            ));
        }

        // Show previous message (context)
        if match_idx > 0 && !shown_indices.contains(&(match_idx - 1)) {
            let prev = &messages[match_idx - 1];
            output.push_str(&format_context_message(prev));
            output.push('\n');
            shown_indices.insert(match_idx - 1);
        }

        // Show matching message (highlighted)
        let msg = &messages[match_idx];
        output.push_str(&format_matching_message(msg, pattern));
        shown_indices.insert(match_idx);
        match_count += 1;

        // Show next message (context)
        if match_idx + 1 < messages.len() && !shown_indices.contains(&(match_idx + 1)) {
            output.push('\n');
            let next = &messages[match_idx + 1];
            output.push_str(&format_context_message(next));
            shown_indices.insert(match_idx + 1);
        }
    }

    if match_count == 0 {
        output.push_str("(no matches in transcript)");
    } else {
        output.push_str(&format!(
            "\n\n{}{} matching messages{}",
            colors::BOLD, match_count, colors::RESET
        ));
    }

    Ok(output)
}

/// Format a context message (dimmed, truncated if too long)
fn format_context_message(msg: &Message) -> String {
    let prefix = if msg.role == "user" { "U" } else { "A" };
    const MAX_CONTEXT_LINES: usize = 10;
    let lines: Vec<&str> = msg.text.lines().collect();

    let mut output = String::new();
    for (i, line) in lines.iter().take(MAX_CONTEXT_LINES).enumerate() {
        let leader = if i == 0 {
            format!("{}: ", prefix)
        } else {
            "   ".to_string()
        };
        output.push_str(&format!(
            "{}{}{}{}\n",
            colors::DIM, leader, line, colors::RESET
        ));
    }
    if lines.len() > MAX_CONTEXT_LINES {
        output.push_str(&format!(
            "{}   ... ({} more lines){}\n",
            colors::DIM,
            lines.len() - MAX_CONTEXT_LINES,
            colors::RESET
        ));
    }
    output
}

/// Format a matching message (colored, with highlights)
fn format_matching_message(msg: &Message, pattern: &str) -> String {
    let (prefix, color) = if msg.role == "user" {
        ("U", colors::CYAN)
    } else {
        ("A", colors::YELLOW)
    };

    let pattern_lower = pattern.to_lowercase();
    let mut output = String::new();

    for (i, line) in msg.text.lines().enumerate() {
        let formatted_line = if line.to_lowercase().contains(&pattern_lower) {
            highlight_match(line, pattern)
        } else {
            line.to_string()
        };

        let leader = if i == 0 {
            format!("{}: ", prefix)
        } else {
            "   ".to_string()
        };
        output.push_str(&format!(
            "{}{}{}{}\n",
            color, leader, formatted_line, colors::RESET
        ));
    }
    output
}

/// Highlight matching text with bold/inverse (Unicode-safe)
///
/// Uses character-based matching to handle cases where lowercasing
/// changes byte length (e.g., ß → ss, İ → i̇).
fn highlight_match(text: &str, pattern: &str) -> String {
    if pattern.is_empty() {
        return text.to_string();
    }

    let pattern_lower = pattern.to_lowercase();
    let pattern_char_count = pattern.chars().count();
    let mut result = String::new();
    let mut last_end = 0;
    let mut i = 0;

    while i < text.len() {
        let remaining = &text[i..];
        let remaining_lower = remaining.to_lowercase();

        if remaining_lower.starts_with(&pattern_lower) {
            // Found match - count characters to get correct byte length in original
            let match_byte_len = remaining
                .char_indices()
                .nth(pattern_char_count)
                .map(|(idx, _)| idx)
                .unwrap_or(remaining.len());

            result.push_str(&text[last_end..i]);
            result.push_str(colors::BOLD_INVERSE);
            result.push_str(&text[i..i + match_byte_len]);
            result.push_str(colors::RESET);

            last_end = i + match_byte_len;
            i = last_end;
        } else {
            // Advance to next character boundary
            i += remaining.chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        }
    }

    result.push_str(&text[last_end..]);
    result
}

// =============================================================================
// Session Resume
// =============================================================================

/// Escape a string for safe inclusion in single-quoted shell argument.
/// Handles single quotes by ending the quote, adding escaped quote, reopening.
/// Only used for remote SSH commands where shell invocation is unavoidable.
fn shell_escape(s: &str) -> String {
    s.replace("'", "'\\''")
}

/// Resume or fork a session, handling both local and remote sessions.
fn resume_session(session: &Session, filepath: &std::path::Path, fork: bool) -> Result<()> {
    use std::process::Command;

    let action = if fork { "Forking" } else { "Resuming" };
    let project_path = &session.project_path;

    // Validate project path
    if project_path.is_empty() {
        eprintln!("Error: Session {} has no project path recorded", session.id);
        eprintln!("Session file: {}", filepath.display());
        anyhow::bail!("Cannot resume: no project path");
    }

    let status = match &session.source {
        SessionSource::Local => {
            // Verify directory exists locally
            if !std::path::Path::new(project_path).exists() {
                eprintln!(
                    "Error: Project directory no longer exists: {}",
                    project_path
                );
                eprintln!("Session file: {}", filepath.display());
                anyhow::bail!("Cannot resume: directory '{}' not found", project_path);
            }

            println!(
                "{} session {} in {}",
                action, session.id, session.project_path
            );

            // Invoke claude directly — no shell, no escaping needed
            let mut cmd = Command::new("claude");
            cmd.current_dir(project_path)
                .args(["-r", &session.id]);
            if fork {
                cmd.arg("--fork-session");
            }
            cmd.status()?
        }
        SessionSource::Remote { name, host, user } => {
            let ssh_target = match user {
                Some(u) => format!("{}@{}", u, host),
                None => host.clone(),
            };

            println!(
                "{} remote session {} on {} in {}",
                action, session.id, name, session.project_path
            );

            // Remote requires shell string — escape for safe single-quoting
            let fork_flag = if fork { " --fork-session" } else { "" };
            let claude_cmd = format!(
                "cd '{}' && claude -r '{}'{}",
                shell_escape(project_path),
                shell_escape(&session.id),
                fork_flag
            );

            // -t allocates a pseudo-TTY (required for claude's interactive mode)
            Command::new("ssh")
                .args(["-t", &ssh_target, &claude_cmd])
                .status()?
        }
    };

    if !status.success() {
        let code = status.code().unwrap_or(-1);
        eprintln!("Command exited with code {}", code);
        eprintln!("Session file: {}", filepath.display());
    }

    Ok(())
}

// =============================================================================
// Interactive Mode (skim - no external dependencies)
// =============================================================================

/// Build a map of parent session ID → child sessions (forks)
fn build_fork_tree<'a>(
    sessions: &[&'a Session],
) -> std::collections::HashMap<String, Vec<&'a Session>> {
    use std::collections::HashMap;
    let mut children_map: HashMap<String, Vec<&Session>> = HashMap::new();

    for session in sessions {
        if let Some(ref parent_id) = session.forked_from {
            children_map
                .entry(parent_id.clone())
                .or_default()
                .push(session);
        }
    }

    // Sort children by modified time (most recent first)
    for children in children_map.values_mut() {
        children.sort_by(|a, b| b.modified.cmp(&a.modified));
    }

    children_map
}

/// Build header showing current navigation state
fn build_subtree_header(
    search_pattern: &Option<String>,
    search_count: Option<usize>,
    fork: bool,
    focus: Option<&String>,
    session_by_id: &std::collections::HashMap<&str, &Session>,
    debug: bool,
) -> String {
    // When searching, show esc to clear; otherwise show navigation hints
    let (nav_hint, focus_info) = if search_pattern.is_some() {
        ("esc to clear".to_string(), String::new())
    } else {
        let hint = if focus.is_some() {
            "← back"
        } else {
            "→ into forks"
        };
        let info = focus
            .and_then(|id| session_by_id.get(id.as_str()))
            .map(|s| {
                let desc = format_session_desc(s, 30);
                format!(" [{}]", desc)
            })
            .unwrap_or_default();
        (hint.to_string(), info)
    };

    let status_line = match (search_pattern, search_count, fork) {
        (Some(pat), Some(count), true) => {
            format!("FORK │ search: \"{}\" ({} matches) │ {}", pat, count, nav_hint)
        }
        (Some(pat), Some(count), false) => {
            format!("search: \"{}\" ({} matches) │ {}", pat, count, nav_hint)
        }
        (Some(pat), None, true) => format!("FORK │ search: \"{}\" │ {}", pat, nav_hint),
        (Some(pat), None, false) => format!("search: \"{}\" │ {}", pat, nav_hint),
        (None, _, true) => format!("FORK mode │ {}{}", nav_hint, focus_info),
        (None, _, false) => format!("Select session │ {}{}", nav_hint, focus_info),
    };

    let legend = build_column_legend(debug);
    format!("{}\n{}", status_line, legend)
}

/// Simple session row format (no tree glyphs)
fn format_session_row_simple(prefix: &str, session: &Session, debug: bool) -> String {
    let created = format_time_relative(session.created);
    let modified = format_time_relative(session.modified);
    let source = session.source.display_name();
    let id_prefix = if debug {
        format!("{:<6}", &session.id[..5.min(session.id.len())])
    } else {
        String::new()
    };
    let msgs = format!("{:>3}", session.turn_count);

    format!(
        "{}{}{:<4} {:<4} {} {:<6} {:<12} {}",
        prefix,
        id_prefix,
        created,
        modified,
        msgs,
        source,
        session.project,
        format_session_desc(session, 40),
    )
}

/// Build column legend for interactive mode
fn build_column_legend(debug: bool) -> String {
    let id_col = if debug { "ID    " } else { "" };
    format!(
        "  {}CRE  MOD  MSG SOURCE PROJECT      SUMMARY",
        id_col
    )
}

fn interactive_mode(
    sessions: &[Session],
    fork: bool,
    debug: bool,
    config: &remote::Config,
) -> Result<()> {
    use skim::prelude::*;
    use std::collections::HashMap;

    // Build session lookup and children map once
    let session_by_id: HashMap<&str, &Session> =
        sessions.iter().map(|s| (s.id.as_str(), s)).collect();
    let children_map = build_fork_tree(&sessions.iter().collect::<Vec<_>>());

    // Navigation state - stack tracks drill-down history (empty = root view)
    let mut search_pattern: Option<String> = None;
    let mut search_results: Option<std::collections::HashSet<String>> = None;
    let mut focus_stack: Vec<String> = Vec::new();

    loop {
        // Build visible sessions based on search results or focus
        // Search results take priority - they replace the view temporarily
        let focus = focus_stack.last();
        let visible_sessions: Vec<&Session> = if let Some(ref matched_ids) = search_results {
            // Search mode: show only sessions that matched the search
            sessions
                .iter()
                .filter(|s| matched_ids.contains(&s.id))
                .collect()
        } else if let Some(focus_id) = focus {
            // Subtree mode: show focused session + direct children only
            let mut result = Vec::new();
            if let Some(session) = session_by_id.get(focus_id.as_str()) {
                result.push(*session);
                if let Some(children) = children_map.get(focus_id) {
                    result.extend(children.iter());
                }
            }
            result
        } else {
            // Root view: only show sessions without a parent (or orphaned forks)
            sessions
                .iter()
                .filter(|s| {
                    s.forked_from
                        .as_ref()
                        .map(|p| !session_by_id.contains_key(p.as_str()))
                        .unwrap_or(true)
                })
                .collect()
        };

        // Build display info
        let mut session_info: HashMap<String, (bool, Option<&str>)> = HashMap::new();
        for session in &visible_sessions {
            let has_children = children_map.contains_key(&session.id);
            let parent_id = session.forked_from.as_deref();
            session_info.insert(session.id.clone(), (has_children, parent_id));
        }

        let search_count = search_results.as_ref().map(|r| r.len());
        let header =
            build_subtree_header(&search_pattern, search_count, fork, focus, &session_by_id, debug);

        let options = SkimOptionsBuilder::default()
            .height(Some("100%"))
            .preview(Some(""))
            .preview_window(Some("right:50%:wrap"))
            .header(Some(&header))
            .prompt(Some("filter> "))
            .reverse(false)
            .nosort(true)
            .bind(vec![
                "ctrl-s:accept", // transcript search
                "right:accept",  // drill into subtree
                "left:accept",   // go up to parent
            ])
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build skim options: {}", e))?;

        let (tx, rx): (SkimItemSender, SkimItemReceiver) = unbounded();

        for session in &visible_sessions {
            let (has_children, _) = session_info.get(&session.id).unwrap_or(&(false, None));
            let is_focus = focus.map(|f| f == &session.id).unwrap_or(false);
            let prefix = if is_focus {
                "▷ " // Hollow triangle for focused parent
            } else if *has_children {
                "▶ " // Filled triangle for items with children
            } else {
                "  " // No indicator for leaf nodes
            };
            let display = format_session_row_simple(prefix, session, debug);

            let item = SessionItem {
                filepath: session.filepath.clone(),
                display,
                match_text: format_session_desc(session, 100),
                session_id: session.id.clone(),
                search_pattern: search_pattern.clone(),
            };
            let _ = tx.send(Arc::new(item));
        }
        drop(tx);

        let output = Skim::run_with(&options, Some(rx));

        match output {
            Some(out) if out.is_abort => {
                // Esc: if searching, clear search; if in subtree, go to root; otherwise exit
                if search_results.is_some() {
                    search_results = None;
                    search_pattern = None;
                    continue;
                }
                if !focus_stack.is_empty() {
                    focus_stack.clear();
                    continue;
                }
                return Ok(());
            }
            Some(out) => {
                let query = out.query.trim();

                // ctrl+s triggers transcript search - replaces view with matching sessions
                if out.final_key == Key::Ctrl('s') {
                    if query.is_empty() {
                        continue;
                    }
                    match claude_code::search_all_sessions(config, query, None) {
                        Ok(matched) => {
                            let matched_ids: std::collections::HashSet<String> =
                                matched.iter().map(|s| s.id.clone()).collect();
                            search_results = Some(matched_ids);
                            search_pattern = Some(query.to_string());
                        }
                        Err(e) => {
                            eprintln!("Search error: {}", e);
                        }
                    }
                    continue;
                }

                // Right: drill into subtree if session has children
                if out.final_key == Key::Right {
                    if let Some(item) = out.selected_items.first() {
                        let selected_id = item.output().to_string();
                        // Don't push if already viewing this session's subtree
                        let already_focused = focus_stack.last().map(|f| f == &selected_id).unwrap_or(false);
                        if !already_focused {
                            if let Some((has_children, _)) = session_info.get(&selected_id) {
                                if *has_children {
                                    focus_stack.push(selected_id);
                                }
                            }
                        }
                    }
                    continue;
                }

                // Left: pop navigation stack (go back)
                if out.final_key == Key::Left {
                    focus_stack.pop();
                    continue;
                }

                // Enter: select session
                if let Some(item) = out.selected_items.first() {
                    let selected_id = item.output().to_string();
                    if let Some(session) = session_by_id.get(selected_id.as_str()) {
                        resume_session(session, &session.filepath, fork)?;
                        return Ok(());
                    }
                }
            }
            None => return Ok(()),
        }
    }
}

/// Session item for skim display
struct SessionItem {
    filepath: PathBuf,
    display: String,
    match_text: String,
    session_id: String,
    search_pattern: Option<String>, // When set, preview shows matching lines
}

impl SkimItem for SessionItem {
    fn text(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.match_text)
    }

    fn display<'a>(&'a self, _context: DisplayContext<'a>) -> AnsiString<'a> {
        AnsiString::from(self.display.as_str())
    }

    fn output(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.session_id)
    }

    fn preview(&self, _context: PreviewContext) -> ItemPreview {
        // Generate preview content directly (no subprocess needed)
        let result = match &self.search_pattern {
            Some(pattern) => generate_search_preview(&self.filepath, pattern),
            None => generate_preview_content(&self.filepath),
        };
        match result {
            Ok(content) => ItemPreview::AnsiText(content),
            Err(_) => ItemPreview::Text("(failed to load preview)".to_string()),
        }
    }
}

// =============================================================================
// Tests (general functionality)
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Project filter logic - The -p flag behavior
    // =========================================================================

    #[test]
    fn project_filter_case_insensitive() {
        let projects = [
            "holy-grail",
            "Ministry-Of-Silly-Walks",
            "SPANISH-INQUISITION",
        ];

        let matches = |filter: &str| -> Vec<&str> {
            let filter_lower = filter.to_lowercase();
            projects
                .iter()
                .filter(|p| p.to_lowercase().contains(&filter_lower))
                .copied()
                .collect()
        };

        assert_eq!(matches("spanish"), ["SPANISH-INQUISITION"]);
        assert_eq!(matches("SILLY"), ["Ministry-Of-Silly-Walks"]);
        assert_eq!(matches("grail"), ["holy-grail"]);
    }

    #[test]
    fn project_filter_substring() {
        let projects = ["spam", "spam-eggs", "spam-eggs-spam"];

        let matches = |filter: &str| -> Vec<&str> {
            let filter_lower = filter.to_lowercase();
            projects
                .iter()
                .filter(|p| p.to_lowercase().contains(&filter_lower))
                .copied()
                .collect()
        };

        assert_eq!(matches("spam"), ["spam", "spam-eggs", "spam-eggs-spam"]);
        assert_eq!(matches("eggs"), ["spam-eggs", "spam-eggs-spam"]);
    }

    // =========================================================================
    // Text normalization
    // =========================================================================

    #[test]
    fn normalize_summary_collapses_whitespace() {
        assert_eq!(
            normalize_summary("hello   world\n\ntest", 50),
            "hello world test"
        );
    }

    #[test]
    fn normalize_summary_strips_markdown() {
        assert_eq!(normalize_summary("# Heading", 50), "Heading");
        assert_eq!(normalize_summary("## Sub heading", 50), "Sub heading");
        assert_eq!(normalize_summary("* bullet point", 50), "bullet point");
    }

    #[test]
    fn normalize_summary_truncates_at_word() {
        // Should truncate at word boundary when possible
        let result = normalize_summary("hello world this is a test", 15);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 18); // 15 + "..."
    }

    #[test]
    fn normalize_summary_preserves_short_text() {
        assert_eq!(normalize_summary("short", 50), "short");
    }

    // =========================================================================
    // Time formatting
    // =========================================================================

    #[test]
    fn format_time_relative_now() {
        let now = SystemTime::now();
        assert_eq!(format_time_relative(now), "now");
    }

    #[test]
    fn format_time_relative_minutes() {
        use std::time::Duration;
        let time = SystemTime::now() - Duration::from_secs(120);
        assert_eq!(format_time_relative(time), "2m");
    }

    #[test]
    fn format_time_relative_hours() {
        use std::time::Duration;
        let time = SystemTime::now() - Duration::from_secs(3600 * 3);
        assert_eq!(format_time_relative(time), "3h");
    }

    #[test]
    fn format_time_relative_days() {
        use std::time::Duration;
        let time = SystemTime::now() - Duration::from_secs(86400 * 2);
        assert_eq!(format_time_relative(time), "2d");
    }

    #[test]
    fn format_time_relative_weeks() {
        use std::time::Duration;
        let time = SystemTime::now() - Duration::from_secs(604800 * 3);
        assert_eq!(format_time_relative(time), "3w");
    }

    #[test]
    fn format_time_relative_future() {
        use std::time::Duration;
        let time = SystemTime::now() + Duration::from_secs(3600);
        assert_eq!(format_time_relative(time), "?");
    }

    // =========================================================================
    // Fork list and tree view
    // =========================================================================

    fn test_session(id: &str) -> Session {
        Session {
            id: id.to_string(),
            project: "test-project".to_string(),
            project_path: "/tmp/test-project".to_string(),
            filepath: PathBuf::from(format!("/tmp/{}.jsonl", id)),
            created: SystemTime::now(),
            modified: SystemTime::now(),
            first_message: None,
            summary: Some("test summary".to_string()),
            name: None,
            turn_count: 1,
            source: SessionSource::Local,
            forked_from: None,
        }
    }

    #[test]
    fn list_mode_excludes_forks_by_default() {
        let parent = test_session("parent");
        let mut fork = test_session("fork");
        fork.forked_from = Some("parent".to_string());

        let sessions = vec![parent, fork];
        let visible = filter_forks_for_list(&sessions, false);

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, "parent");
    }

    // =========================================================================
    // Fork tree and subtree collection
    // =========================================================================

    #[test]
    fn build_fork_tree_maps_parent_to_children() {
        let root = test_session("root");
        let mut child1 = test_session("child1");
        child1.forked_from = Some("root".to_string());
        let mut child2 = test_session("child2");
        child2.forked_from = Some("root".to_string());

        let sessions: Vec<&Session> = vec![&root, &child1, &child2];
        let children_map = build_fork_tree(&sessions);

        assert!(children_map.contains_key("root"));
        assert_eq!(children_map.get("root").unwrap().len(), 2);
        assert!(!children_map.contains_key("child1"));
        assert!(!children_map.contains_key("child2"));
    }

    #[test]
    fn build_fork_tree_handles_nested_forks() {
        // root -> child -> grandchild
        let root = test_session("root");
        let mut child = test_session("child");
        child.forked_from = Some("root".to_string());
        let mut grandchild = test_session("grandchild");
        grandchild.forked_from = Some("child".to_string());

        let sessions: Vec<&Session> = vec![&root, &child, &grandchild];
        let children_map = build_fork_tree(&sessions);

        assert_eq!(children_map.get("root").unwrap().len(), 1);
        assert_eq!(children_map.get("child").unwrap().len(), 1);
        assert!(!children_map.contains_key("grandchild"));
    }

    // =========================================================================
    // Subtree collection (test-only helper for future use)
    // =========================================================================

    /// Collect a session and all its descendants into a vec (test helper)
    fn collect_subtree<'a>(
        session: &'a Session,
        children_map: &std::collections::HashMap<String, Vec<&'a Session>>,
        result: &mut Vec<&'a Session>,
    ) {
        result.push(session);
        if let Some(children) = children_map.get(&session.id) {
            for child in children {
                collect_subtree(child, children_map, result);
            }
        }
    }

    #[test]
    fn collect_subtree_includes_all_descendants() {
        // root -> child1, child2
        // child1 -> grandchild
        let root = test_session("root");
        let mut child1 = test_session("child1");
        child1.forked_from = Some("root".to_string());
        let mut child2 = test_session("child2");
        child2.forked_from = Some("root".to_string());
        let mut grandchild = test_session("grandchild");
        grandchild.forked_from = Some("child1".to_string());

        let sessions: Vec<&Session> = vec![&root, &child1, &child2, &grandchild];
        let children_map = build_fork_tree(&sessions);

        let mut result = Vec::new();
        collect_subtree(&root, &children_map, &mut result);

        assert_eq!(result.len(), 4);
        let ids: Vec<&str> = result.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"root"));
        assert!(ids.contains(&"child1"));
        assert!(ids.contains(&"child2"));
        assert!(ids.contains(&"grandchild"));
    }

    #[test]
    fn collect_subtree_from_middle_excludes_siblings() {
        // root -> child1, child2
        // child1 -> grandchild
        let root = test_session("root");
        let mut child1 = test_session("child1");
        child1.forked_from = Some("root".to_string());
        let mut child2 = test_session("child2");
        child2.forked_from = Some("root".to_string());
        let mut grandchild = test_session("grandchild");
        grandchild.forked_from = Some("child1".to_string());

        let sessions: Vec<&Session> = vec![&root, &child1, &child2, &grandchild];
        let children_map = build_fork_tree(&sessions);

        let mut result = Vec::new();
        collect_subtree(&child1, &children_map, &mut result);

        assert_eq!(result.len(), 2);
        let ids: Vec<&str> = result.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"child1"));
        assert!(ids.contains(&"grandchild"));
        assert!(!ids.contains(&"root"));
        assert!(!ids.contains(&"child2"));
    }

    // =========================================================================
    // Column legend and header formatting
    // =========================================================================

    #[test]
    fn build_column_legend_without_debug() {
        let legend = build_column_legend(false);
        assert_eq!(legend, "  CRE  MOD  MSG SOURCE PROJECT      SUMMARY");
        assert!(!legend.contains("ID"));
    }

    #[test]
    fn build_column_legend_with_debug() {
        let legend = build_column_legend(true);
        assert!(legend.contains("ID"));
        assert!(legend.contains("CRE"));
        assert!(legend.contains("MSG"));
    }

    #[test]
    fn build_subtree_header_root_view() {
        use std::collections::HashMap;
        let session_by_id: HashMap<&str, &Session> = HashMap::new();

        let header = build_subtree_header(&None, None, false, None, &session_by_id, false);
        assert!(header.contains("Select session"));
        assert!(header.contains("→ into forks"));
        assert!(header.contains("CRE")); // Legend line
    }

    #[test]
    fn build_subtree_header_fork_mode() {
        use std::collections::HashMap;
        let session_by_id: HashMap<&str, &Session> = HashMap::new();

        let header = build_subtree_header(&None, None, true, None, &session_by_id, false);
        assert!(header.contains("FORK mode"));
    }

    #[test]
    fn build_subtree_header_with_search() {
        use std::collections::HashMap;
        let session_by_id: HashMap<&str, &Session> = HashMap::new();

        // Search with match count
        let header =
            build_subtree_header(&Some("api".to_string()), Some(5), false, None, &session_by_id, false);
        assert!(header.contains("search: \"api\""));
        assert!(header.contains("(5 matches)"));
        assert!(header.contains("esc to clear"));
    }

    #[test]
    fn build_subtree_header_focused_shows_back() {
        use std::collections::HashMap;
        let session = test_session("focused");
        let mut session_by_id: HashMap<&str, &Session> = HashMap::new();
        session_by_id.insert("focused", &session);

        let focus = "focused".to_string();
        let header = build_subtree_header(&None, None, false, Some(&focus), &session_by_id, false);
        assert!(header.contains("← back"));
        assert!(!header.contains("→ into forks"));
    }

    // =========================================================================
    // Session row formatting
    // =========================================================================

    #[test]
    fn format_session_row_simple_basic() {
        let session = test_session("test-id");
        let row = format_session_row_simple("  ", &session, false);

        // Should contain project name and source
        assert!(row.contains("test-proj"));
        assert!(row.contains("local"));
        // Should NOT start with ID prefix when debug=false (starts with "  " prefix)
        assert!(row.starts_with("  "));
        // ID "test-id" first 5 chars is "test-" which should NOT appear at start
        assert!(!row.starts_with("  test-"));
    }

    #[test]
    fn format_session_row_simple_with_debug() {
        let session = test_session("abcdef-1234");
        let row = format_session_row_simple("▶ ", &session, true);

        // Should contain first 5 chars of ID
        assert!(row.contains("abcde"));
        // Should contain the prefix
        assert!(row.starts_with("▶ "));
    }

    #[test]
    fn format_session_row_simple_shows_turn_count() {
        let mut session = test_session("test");
        session.turn_count = 42;
        let row = format_session_row_simple("  ", &session, false);

        // Turn count should be right-aligned in 3 chars
        assert!(row.contains(" 42 "));
    }

    // =========================================================================
    // Shell escaping (security)
    // =========================================================================

    #[test]
    fn shell_escape_no_quotes() {
        assert_eq!(shell_escape("hello"), "hello");
        assert_eq!(shell_escape("/path/to/project"), "/path/to/project");
    }

    #[test]
    fn shell_escape_single_quotes() {
        // Single quote becomes: end quote, escaped quote, start quote
        assert_eq!(shell_escape("it's"), "it'\\''s");
        assert_eq!(shell_escape("'quoted'"), "'\\''quoted'\\''");
    }

    #[test]
    fn shell_escape_multiple_quotes() {
        assert_eq!(shell_escape("a'b'c"), "a'\\''b'\\''c");
    }

    #[test]
    fn shell_escape_preserves_other_chars() {
        // Double quotes, spaces, etc. are fine inside single quotes
        assert_eq!(shell_escape("hello world"), "hello world");
        assert_eq!(shell_escape("\"quoted\""), "\"quoted\"");
        assert_eq!(shell_escape("$HOME"), "$HOME");
    }

    // =========================================================================
    // Highlight matching (Unicode-safe)
    // =========================================================================

    #[test]
    fn highlight_match_basic() {
        let result = highlight_match("hello world", "world");
        assert!(result.contains(colors::BOLD_INVERSE));
        assert!(result.contains("world"));
        assert!(result.contains(colors::RESET));
    }

    #[test]
    fn highlight_match_case_insensitive() {
        let result = highlight_match("Hello World", "world");
        // Should highlight "World" (preserving original case)
        assert!(result.contains("World"));
        assert!(result.contains(colors::BOLD_INVERSE));
    }

    #[test]
    fn highlight_match_empty_pattern() {
        assert_eq!(highlight_match("hello", ""), "hello");
    }

    #[test]
    fn highlight_match_no_match() {
        let result = highlight_match("hello", "xyz");
        assert!(!result.contains(colors::BOLD_INVERSE));
        assert_eq!(result, "hello");
    }

    #[test]
    fn highlight_match_multibyte_chars() {
        // Test with emoji and Unicode - should not panic
        let result = highlight_match("hello 🌍 world", "world");
        assert!(result.contains(colors::BOLD_INVERSE));
    }

    #[test]
    fn highlight_match_unicode_case_fold() {
        // ß lowercases to "ss" - pattern "ss" should still work
        // The text has ß, searching for "ss" should not find it (different chars)
        // But searching for "ß" in text with "ß" should work
        let result = highlight_match("Straße", "ße");
        assert!(result.contains(colors::BOLD_INVERSE));
    }
}
