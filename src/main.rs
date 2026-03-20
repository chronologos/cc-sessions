mod claude_code;
mod interactive_state;
mod message_classification;
mod remote;
mod session;

use anyhow::{Context, Result};
use clap::Parser;
use interactive_state::{Action as StateAction, Effect as StateEffect, InteractiveState};
use session::{Session, SessionSource};
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

    /// Show session ID prefixes and extra stats
    #[arg(long, help_heading = "Mode")]
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

    /// Treat any remote sync/discovery source failure as fatal
    #[arg(long, help_heading = "Remote sync")]
    strict: bool,

    // -------------------------------------------------------------------------
    // Internal (hidden from --help)
    // -------------------------------------------------------------------------
    /// Preview a session file (used internally by interactive picker)
    #[arg(long, value_name = "FILE", hide = true)]
    preview: Option<PathBuf>,
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
        let summary = remote::sync_all(&config)?;
        for result in &summary.successes {
            println!(
                "Synced '{}' in {:.1}s",
                result.remote_name,
                result.duration.as_secs_f64()
            );
        }
        for failure in &summary.failures {
            eprintln!(
                "Warning: Failed to sync '{}': {}",
                failure.remote_name, failure.reason
            );
        }
        if summary.successes.is_empty() {
            println!("No remotes configured. Add remotes to ~/.config/cc-sessions/remotes.toml");
        }
        enforce_strict_mode(args.strict, summary.failure_count(), 0)?;
        return Ok(());
    }

    let mut sync_failures = 0;

    if args.sync {
        // Force sync all remotes
        let summary = remote::sync_all(&config)?;
        for result in &summary.successes {
            eprintln!(
                "Synced '{}' in {:.1}s",
                result.remote_name,
                result.duration.as_secs_f64()
            );
        }
        sync_failures = summary.failure_count();
    } else if !args.no_sync && !config.remotes.is_empty() {
        // Auto-sync stale remotes
        let summary = remote::sync_if_stale(&config)?;
        for result in &summary.successes {
            eprintln!(
                "Auto-synced '{}' in {:.1}s",
                result.remote_name,
                result.duration.as_secs_f64()
            );
        }
        sync_failures = summary.failure_count();
    }

    // Find sessions from all sources (local + remotes)
    let discovery = claude_code::find_all_sessions_with_summary(&config, args.remote.as_deref())?;
    for failure in &discovery.failures {
        eprintln!(
            "Warning: Failed to load sessions from '{}': {}",
            failure.source_name, failure.reason
        );
    }
    enforce_strict_mode(args.strict, sync_failures, discovery.failure_count())?;
    let mut sessions = discovery.sessions;

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
        interactive_mode(&sessions, args.fork, args.debug)?;
    }

    Ok(())
}

fn enforce_strict_mode(
    strict: bool,
    sync_failures: usize,
    discovery_failures: usize,
) -> Result<()> {
    if !strict {
        return Ok(());
    }

    if sync_failures > 0 {
        anyhow::bail!("Strict mode: {} sync source(s) failed", sync_failures);
    }

    if discovery_failures > 0 {
        anyhow::bail!(
            "Strict mode: {} discovery source(s) failed",
            discovery_failures
        );
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
            let fork_indicator = if session.forked_from.is_some() {
                "↳"
            } else {
                ""
            };
            let id_short = if session.id.len() > 36 {
                &session.id[..36]
            } else {
                &session.id
            };
            let desc = format_session_desc(session, 30);
            let desc = if session.name.is_some() {
                format!("{}{}{}", colors::YELLOW, desc, colors::RESET)
            } else {
                desc
            };

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
            let desc = if session.name.is_some() {
                format!("{}{}{}", colors::YELLOW, desc, colors::RESET)
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

/// Format session description: name (★) > tag (#) > summary > first_message
fn format_session_desc(session: &Session, max_chars: usize) -> String {
    let label = match (&session.name, &session.tag) {
        (Some(name), Some(tag)) => Some(format!("★ {} #{}", name, tag)),
        (Some(name), None) => Some(format!("★ {}", name)),
        (None, Some(tag)) => Some(format!("#{}", tag)),
        (None, None) => None,
    };

    if let Some(label) = label {
        let label_len = label.chars().count();
        if label_len >= max_chars {
            return label.chars().take(max_chars).collect();
        }
        // Append summary if there's room for " - " + at least 10 chars
        if let Some(summary) = &session.summary
            && max_chars > label_len + 13
        {
            let remaining = max_chars - label_len - 3;
            return format!(
                "{} - {}",
                label,
                summary.chars().take(remaining).collect::<String>()
            );
        }
        return label;
    }

    session
        .summary
        .as_deref()
        .or(session.first_message.as_deref())
        .map(|s| s.chars().take(max_chars).collect())
        .unwrap_or_default()
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
    // Collapse whitespace and build directly into the output buffer — stop
    // collecting once we're past max_chars (summary inputs can be very long).
    let mut normalized = String::with_capacity(max_chars.min(text.len()) + 4);
    let mut words = text.split_whitespace();
    if let Some(first) = words.next() {
        normalized.push_str(first);
        for w in words {
            normalized.push(' ');
            normalized.push_str(w);
            if normalized.len() > max_chars * 4 {
                break;
            }
        }
    }

    let stripped = normalized.trim_start_matches(['#', '*']).trim_start();

    if stripped.chars().count() <= max_chars {
        return stripped.to_owned();
    }

    let truncated: String = stripped.chars().take(max_chars).collect();
    let break_point = truncated
        .rfind(' ')
        .filter(|&i| i > max_chars / 2)
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

/// Extract first text block from a message entry, borrowing from the JSON value
fn extract_message_text(entry: &serde_json::Value) -> Option<&str> {
    let content = entry.get("message")?.get("content")?;
    claude_code::first_text_block(content)
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
    let mut reader = BufReader::new(file);

    let mut output = String::new();
    let mut line = String::new();
    let mut line_count = 0;
    const MAX_LINES: usize = 100;

    while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
        if line_count >= MAX_LINES {
            break;
        }
        if !claude_code::line_mentions_content_type(line.as_bytes()) {
            line.clear();
            continue;
        }

        let entry: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                line.clear();
                continue;
            }
        };
        line.clear();

        let entry_type = entry.get("type").and_then(|v| v.as_str());

        match entry_type {
            Some("user") => {
                if let Some(text) = extract_message_text(&entry)
                    && !is_system_content(text)
                {
                    let first_line = text.lines().next().unwrap_or(text);
                    let truncated = truncate_str(first_line, 120);
                    output.push_str(&format!(
                        "{}U: {}{}\n",
                        colors::CYAN,
                        truncated,
                        colors::RESET
                    ));
                    line_count += 1;
                }
            }
            Some("assistant") => {
                if let Some(text) = extract_message_text(&entry) {
                    let first_line = text.lines().next().unwrap_or(text);
                    let truncated = truncate_str(first_line, 80);
                    output.push_str(&format!(
                        "{}A: {}{}\n",
                        colors::YELLOW,
                        truncated,
                        colors::RESET
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
    message_classification::is_system_content_for_preview(text)
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
    let mut reader = BufReader::new(file);

    // Collect all messages first (filter out progress/attachment lines before
    // the JSON parse — large sessions are dominated by those).
    let mut messages: Vec<Message> = Vec::new();
    let mut line = String::new();
    while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
        if !claude_code::line_mentions_content_type(line.as_bytes()) {
            line.clear();
            continue;
        }
        let entry: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                line.clear();
                continue;
            }
        };
        line.clear();

        let role = match entry.get("type").and_then(|v| v.as_str()) {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => continue,
        };

        if let Some(text) = extract_message_text(&entry) {
            if role == "user" && is_system_content(text) {
                continue;
            }
            messages.push(Message {
                role: role.to_owned(),
                text: text.to_owned(),
            });
        }
    }

    let pattern_lower = pattern.to_lowercase();
    let mut output = String::new();
    let mut match_count = 0;
    const MAX_MATCHES: usize = 10; // Fewer matches since we show full context

    output.push_str(&format!(
        "{}Searching for: \"{}\"{}\n\n",
        colors::GREEN,
        pattern,
        colors::RESET
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
                colors::BOLD,
                colors::RESET
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
                colors::DIM,
                colors::RESET
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
            colors::BOLD,
            match_count,
            colors::RESET
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
            colors::DIM,
            leader,
            line,
            colors::RESET
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
            color,
            leader,
            formatted_line,
            colors::RESET
        ));
    }
    output
}

/// Highlight matching text with bold/inverse (Unicode-safe)
fn highlight_match(text: &str, pattern: &str) -> String {
    if pattern.is_empty() {
        return text.to_owned();
    }

    // Fast path: ASCII-only text and pattern. Lowercasing preserves byte
    // positions, so we lower once and match_indices gives us offsets directly.
    // This is O(n) vs. the generic path's per-position re-lowering.
    if text.is_ascii() && pattern.is_ascii() {
        let text_lower = text.to_ascii_lowercase();
        let pattern_lower = pattern.to_ascii_lowercase();
        let mut result = String::with_capacity(text.len() + 16);
        let mut last = 0;
        for (i, _) in text_lower.match_indices(&pattern_lower) {
            result.push_str(&text[last..i]);
            result.push_str(colors::BOLD_INVERSE);
            result.push_str(&text[i..i + pattern.len()]);
            result.push_str(colors::RESET);
            last = i + pattern.len();
        }
        result.push_str(&text[last..]);
        return result;
    }

    // Generic path: handles case-fold expansion (ß → ss, İ → i̇). Walk the
    // original by char, lower only the pattern-sized window at each position.
    let pattern_lower = pattern.to_lowercase();
    let pattern_char_count = pattern.chars().count();
    let mut result = String::with_capacity(text.len() + 16);
    let mut last_end = 0;

    let indices: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(text.len()))
        .collect();

    let mut i = 0;
    while i + pattern_char_count < indices.len() {
        let start = indices[i];
        let end = indices[i + pattern_char_count];
        if text[start..end].to_lowercase() == pattern_lower {
            result.push_str(&text[last_end..start]);
            result.push_str(colors::BOLD_INVERSE);
            result.push_str(&text[start..end]);
            result.push_str(colors::RESET);
            last_end = end;
            i += pattern_char_count;
        } else {
            i += 1;
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
            cmd.current_dir(project_path).args(["-r", &session.id]);
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
fn build_fork_tree(sessions: &[Session]) -> std::collections::HashMap<&str, Vec<&Session>> {
    use std::collections::HashMap;
    let mut children_map: HashMap<&str, Vec<&Session>> = HashMap::new();

    for session in sessions {
        if let Some(parent_id) = session.forked_from.as_deref() {
            children_map.entry(parent_id).or_default().push(session);
        }
    }

    for children in children_map.values_mut() {
        children.sort_by(|a, b| b.modified.cmp(&a.modified));
    }

    children_map
}

/// Build header showing current navigation state
fn build_subtree_header(
    search_pattern: Option<&str>,
    search_count: Option<usize>,
    fork: bool,
    focus: Option<&str>,
    session_by_id: &std::collections::HashMap<&str, &Session>,
    debug: bool,
) -> String {
    // When searching, show esc to clear; otherwise show navigation hints
    let (nav_hint, focus_info) = if search_pattern.is_some() {
        ("esc to clear", String::new())
    } else {
        let hint = if focus.is_some() {
            "← back"
        } else {
            "→ into forks"
        };
        let info = focus
            .and_then(|id| session_by_id.get(id))
            .map(|s| format!(" [{}]", format_session_desc(s, 30)))
            .unwrap_or_default();
        (hint, info)
    };

    let status_line = match (search_pattern, search_count, fork) {
        (Some(pat), Some(count), true) => {
            format!(
                "FORK │ search: \"{}\" ({} matches) │ {}",
                pat, count, nav_hint
            )
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

    let desc = format_session_desc(session, 40);

    format!(
        "{}{}{:<4} {:<4} {} {:<6} {:<12} {}",
        prefix, id_prefix, created, modified, msgs, source, session.project, desc,
    )
}

/// Build column legend for interactive mode
fn build_column_legend(debug: bool) -> String {
    let id_col = if debug { "ID    " } else { "" };
    format!("  {}CRE  MOD  MSG SOURCE PROJECT      SUMMARY", id_col)
}

/// Compute visible sessions based on current search and subtree focus state.
/// Search mode takes priority and temporarily replaces subtree/root views.
fn visible_sessions_for_view<'a>(
    sessions: &'a [Session],
    session_by_id: &std::collections::HashMap<&str, &'a Session>,
    children_map: &std::collections::HashMap<&str, Vec<&'a Session>>,
    search_results: Option<&std::collections::HashSet<String>>,
    focus: Option<&str>,
) -> Vec<&'a Session> {
    if let Some(matched_ids) = search_results {
        return sessions
            .iter()
            .filter(|s| matched_ids.contains(&s.id))
            .collect();
    }

    if let Some(focus_id) = focus {
        let mut result = Vec::new();
        if let Some(session) = session_by_id.get(focus_id) {
            result.push(*session);
            if let Some(children) = children_map.get(focus_id) {
                result.extend(children.iter().copied());
            }
        }
        return result;
    }

    // Root view: only show sessions without a parent (or orphaned forks)
    sessions
        .iter()
        .filter(|s| {
            s.forked_from
                .as_deref()
                .map(|p| !session_by_id.contains_key(p))
                .unwrap_or(true)
        })
        .collect()
}

fn interactive_mode(sessions: &[Session], fork: bool, debug: bool) -> Result<()> {
    use crossterm::event::{KeyCode, KeyModifiers};
    use std::collections::HashMap;

    let session_by_id: HashMap<&str, &Session> =
        sessions.iter().map(|s| (s.id.as_str(), s)).collect();
    let children_map = build_fork_tree(sessions);

    // Kick off the transcript search index on a background thread so the picker
    // renders immediately. By the time the user has typed a query and hit
    // Ctrl+S the index is almost certainly ready; if not, the join blocks
    // briefly. Memory stays low for list mode and for interactive mode until
    // the index actually materializes.
    let index_targets: Vec<(String, PathBuf)> = sessions
        .iter()
        .map(|s| (s.id.clone(), s.filepath.clone()))
        .collect();
    let mut index_handle = Some(std::thread::spawn(move || {
        claude_code::build_search_index(index_targets)
    }));
    let mut search_index: Option<claude_code::SearchIndex> = None;

    let mut state = InteractiveState::default();

    loop {
        let focus = state.focus().map(String::as_str);
        let visible_sessions = visible_sessions_for_view(
            sessions,
            &session_by_id,
            &children_map,
            state.search_results(),
            focus,
        );

        let search_count = state.search_results().map(|r| r.len());
        let search_pattern = state.search_pattern().map(String::as_str);
        let header = build_subtree_header(
            search_pattern,
            search_count,
            fork,
            focus,
            &session_by_id,
            debug,
        );

        let options = SkimOptionsBuilder::default()
            .height("100%")
            .preview("") // enables preview pane
            .preview_window("right:50%:wrap")
            .header(&header)
            .prompt("filter> ")
            .reverse(false)
            .no_sort(true)
            .bind(vec![
                "ctrl-s:accept".to_string(),
                "right:accept".to_string(),
                "left:accept".to_string(),
            ])
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build skim options: {}", e))?;

        let (tx, rx): (SkimItemSender, SkimItemReceiver) = unbounded();

        let items: Vec<Arc<dyn SkimItem>> = visible_sessions
            .iter()
            .map(|session| {
                let prefix = if focus == Some(session.id.as_str()) {
                    "▷ "
                } else if children_map.contains_key(session.id.as_str()) {
                    "▶ "
                } else {
                    "  "
                };
                Arc::new(SessionItem {
                    filepath: session.filepath.clone(),
                    display: format_session_row_simple(prefix, session, debug),
                    session_id: session.id.clone(),
                    search_pattern: search_pattern.map(str::to_owned),
                }) as Arc<dyn SkimItem>
            })
            .collect();
        let _ = tx.send(items);
        drop(tx);

        let out =
            Skim::run_with(options, Some(rx)).map_err(|e| anyhow::anyhow!("skim failed: {}", e))?;

        if out.is_abort {
            match state.apply(StateAction::Esc) {
                StateEffect::Exit => return Ok(()),
                _ => continue,
            }
        }

        let key = (out.final_key.code, out.final_key.modifiers);

        if key == (KeyCode::Char('s'), KeyModifiers::CONTROL) {
            let effect = state.apply(StateAction::CtrlS {
                query: out.query.to_string(),
            });
            let StateEffect::RunSearch { pattern } = effect else {
                continue;
            };
            // Materialize the background index on first search.
            let index = search_index.get_or_insert_with(|| {
                index_handle
                    .take()
                    .and_then(|h| h.join().ok())
                    .unwrap_or_default()
            });
            // Index is built with make_ascii_lowercase(); fold the query the
            // same way so non-ASCII letters compare identically on both sides.
            let pattern_lower = pattern.to_ascii_lowercase();
            let matched_ids: std::collections::HashSet<String> = index
                .iter()
                .filter(|(_, text)| text.contains(&pattern_lower))
                .map(|(id, _)| id.clone())
                .collect();
            let _ = state.apply(StateAction::ApplySearchResults {
                pattern,
                matched_ids,
            });
            continue;
        }

        if key.0 == KeyCode::Right {
            let selected_id = out.selected_items.first().map(|m| m.output().to_string());
            let has_children = selected_id
                .as_deref()
                .map(|id| children_map.contains_key(id))
                .unwrap_or(false);
            let _ = state.apply(StateAction::Right {
                selected_id,
                has_children,
            });
            continue;
        }

        // Left: pop stack
        if key.0 == KeyCode::Left {
            let _ = state.apply(StateAction::Left);
            continue;
        }

        // Enter: select session
        let selected_id = out.selected_items.first().map(|m| m.output().to_string());
        if let StateEffect::Select { session_id } = state.apply(StateAction::Enter { selected_id })
            && let Some(session) = session_by_id.get(session_id.as_str())
        {
            resume_session(session, &session.filepath, fork)?;
            return Ok(());
        }
    }
}

/// Session item for skim display
struct SessionItem {
    filepath: PathBuf,
    display: String,
    session_id: String,
    search_pattern: Option<String>, // When set, preview shows matching lines
}

impl SkimItem for SessionItem {
    fn text(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.display)
    }

    fn output(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.session_id)
    }

    fn preview(&self, _context: PreviewContext) -> ItemPreview {
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
            tag: None,
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

        let sessions = vec![root, child1, child2];
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

        let sessions = vec![root, child, grandchild];
        let children_map = build_fork_tree(&sessions);

        assert_eq!(children_map.get("root").unwrap().len(), 1);
        assert_eq!(children_map.get("child").unwrap().len(), 1);
        assert!(!children_map.contains_key("grandchild"));
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

        let header = build_subtree_header(None, None, false, None, &session_by_id, false);
        assert!(header.contains("Select session"));
        assert!(header.contains("→ into forks"));
        assert!(header.contains("CRE")); // Legend line
    }

    #[test]
    fn build_subtree_header_fork_mode() {
        use std::collections::HashMap;
        let session_by_id: HashMap<&str, &Session> = HashMap::new();

        let header = build_subtree_header(None, None, true, None, &session_by_id, false);
        assert!(header.contains("FORK mode"));
    }

    #[test]
    fn build_subtree_header_with_search() {
        use std::collections::HashMap;
        let session_by_id: HashMap<&str, &Session> = HashMap::new();

        let header = build_subtree_header(Some("api"), Some(5), false, None, &session_by_id, false);
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

        let header =
            build_subtree_header(None, None, false, Some("focused"), &session_by_id, false);
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

    #[test]
    fn search_results_replace_subtree_until_esc() {
        use std::collections::{HashMap, HashSet};

        let root = test_session("root");
        let mut child = test_session("child");
        child.forked_from = Some("root".to_string());
        let sibling = test_session("sibling");

        let sessions = vec![root, child, sibling];
        let session_by_id: HashMap<&str, &Session> =
            sessions.iter().map(|s| (s.id.as_str(), s)).collect();
        let children_map = build_fork_tree(&sessions);

        // Focused subtree should show root + child
        let visible =
            visible_sessions_for_view(&sessions, &session_by_id, &children_map, None, Some("root"));
        let ids: Vec<&str> = visible.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["root", "child"]);

        // Search should replace subtree view
        let mut matched = HashSet::new();
        matched.insert("sibling".to_string());
        let visible = visible_sessions_for_view(
            &sessions,
            &session_by_id,
            &children_map,
            Some(&matched),
            Some("root"),
        );
        let ids: Vec<&str> = visible.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["sibling"]);

        // Clearing search restores subtree view
        let visible =
            visible_sessions_for_view(&sessions, &session_by_id, &children_map, None, Some("root"));
        let ids: Vec<&str> = visible.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["root", "child"]);
    }

    #[test]
    fn strict_mode_fails_when_any_remote_sync_fails() {
        let result = enforce_strict_mode(true, 1, 0);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Strict mode: 1 sync source(s) failed")
        );
    }

    #[test]
    fn strict_mode_fails_when_any_discovery_source_fails() {
        let result = enforce_strict_mode(true, 0, 2);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Strict mode: 2 discovery source(s) failed")
        );
    }

    #[test]
    fn strict_mode_disabled_allows_failures() {
        assert!(enforce_strict_mode(false, 3, 4).is_ok());
    }

    #[test]
    fn session_source_display_name_local() {
        let source = crate::session::SessionSource::Local;
        assert_eq!(source.display_name(), "local");
        assert!(source.is_local());
    }
}
