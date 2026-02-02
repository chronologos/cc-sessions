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
#[command(name = "cc-session", about = "List Claude Code sessions")]
struct Args {
    /// Number of sessions to show (for list mode)
    #[arg(long, default_value = "15")]
    count: usize,

    /// List mode (non-interactive) - show sessions as a table
    #[arg(long)]
    list: bool,

    /// Fork session instead of resuming (creates new session ID)
    #[arg(long)]
    fork: bool,

    /// Filter by project name (substring match, case-insensitive)
    #[arg(long)]
    project: Option<String>,

    /// Minimum number of conversation turns (filters out one-shot sessions)
    #[arg(long)]
    min_turns: Option<usize>,

    /// Preview a session file (internal use by interactive mode)
    #[arg(long, value_name = "FILE")]
    preview: Option<PathBuf>,

    /// Debug mode - show session IDs and stats
    #[arg(long)]
    debug: bool,

    /// Filter to sessions from a specific remote (or "local")
    #[arg(long, value_name = "NAME")]
    remote: Option<String>,

    /// Force sync all remotes before listing
    #[arg(long)]
    sync: bool,

    /// Skip auto-sync (use cached data only)
    #[arg(long)]
    no_sync: bool,

    /// Sync remotes and exit (for cron/scripting)
    #[arg(long)]
    sync_only: bool,
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
    pub name: Option<String>,  // customTitle from /rename - indicates important session
    pub turn_count: usize,     // Number of user messages (conversation turns)
    pub source: SessionSource, // Where this session came from
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
        print_sessions(&sessions, args.count, args.debug);
    } else {
        interactive_mode(&sessions, args.fork, &config)?;
    }

    Ok(())
}

// =============================================================================
// Display Functions
// =============================================================================

fn print_sessions(sessions: &[Session], count: usize, debug: bool) {
    if debug {
        println!(
            "{:<6} {:<6} {:<8} {:<16} {:<40} SUMMARY",
            "CREAT", "MOD", "SOURCE", "PROJECT", "ID"
        );
        println!("{}", "─".repeat(120));

        for session in sessions.iter().take(count) {
            let created = format_time_relative(session.created);
            let modified = format_time_relative(session.modified);
            let source = session.source.display_name();
            let id_short = if session.id.len() > 36 {
                &session.id[..36]
            } else {
                &session.id
            };
            let desc = format_session_desc(session, 30);

            println!(
                "{:<6} {:<6} {:<8} {:<16} {:<40} {}",
                created, modified, source, session.project, id_short, desc
            );
        }

        println!("{}", "─".repeat(120));
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

            println!(
                "{:<6} {:<6} {:<8} {:<16} {}",
                created, modified, source, session.project, desc
            );
        }

        println!("{}", "─".repeat(100));
        println!("Use 'cc-sessions' for interactive picker, --fork to fork");
    }
}

fn format_time_relative(time: SystemTime) -> String {
    let now = SystemTime::now();
    let duration = now.duration_since(time).unwrap_or_default();
    let secs = duration.as_secs();

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

/// Highlight matching text with bold/inverse
fn highlight_match(text: &str, pattern: &str) -> String {
    let text_lower = text.to_lowercase();
    let pattern_lower = pattern.to_lowercase();

    let mut result = String::new();
    let mut last_end = 0;

    for (start, _) in text_lower.match_indices(&pattern_lower) {
        // Add text before match
        result.push_str(&text[last_end..start]);
        // Add highlighted match (using original case from text)
        result.push_str(colors::BOLD_INVERSE);
        result.push_str(&text[start..start + pattern.len()]);
        result.push_str(colors::RESET);
        last_end = start + pattern.len();
    }

    // Add remaining text
    result.push_str(&text[last_end..]);
    result
}

// =============================================================================
// Session Resume
// =============================================================================

/// Resume or fork a session, handling both local and remote sessions.
fn resume_session(session: &Session, filepath: &std::path::Path, fork: bool) -> Result<()> {
    use std::process::Command;

    let action = if fork { "Forking" } else { "Resuming" };
    let fork_flag = if fork { " --fork-session" } else { "" };
    let project_path = &session.project_path;

    // Validate project path
    if project_path.is_empty() {
        eprintln!("Error: Session {} has no project path recorded", session.id);
        eprintln!("Session file: {}", filepath.display());
        anyhow::bail!("Cannot resume: no project path");
    }

    // Build the claude command (same for local and remote)
    let claude_cmd = format!(
        "cd '{}' && claude -r '{}'{}",
        project_path, session.id, fork_flag
    );

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

            Command::new("zsh").args(["-c", &claude_cmd]).status()?
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

fn interactive_mode(
    sessions: &[Session],
    fork: bool,
    config: &remote::Config,
) -> Result<()> {
    use skim::prelude::*;
    use std::collections::HashMap;

    // Start with all sessions; may be filtered by transcript search
    let mut current_sessions: Vec<&Session> = sessions.iter().collect();
    let mut search_pattern: Option<String> = None;

    loop {
        let header = match (&search_pattern, fork) {
            (Some(pat), true) => format!(
                "FORK │ search: \"{}\" │ ctrl+s: new search │ esc: clear",
                pat
            ),
            (Some(pat), false) => format!("search: \"{}\" │ ctrl+s: new search │ esc: clear", pat),
            (None, true) => "FORK mode │ ctrl+s: transcript search".to_string(),
            (None, false) => "Select session │ ctrl+s: transcript search".to_string(),
        };

        let options = SkimOptionsBuilder::default()
            .height(Some("100%"))
            .preview(Some(""))
            .preview_window(Some("right:50%:wrap"))
            .header(Some(&header))
            .prompt(Some("filter> "))
            .bind(vec!["ctrl-s:accept"]) // ctrl+s triggers transcript search
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build skim options: {}", e))?;

        let (tx, rx): (SkimItemSender, SkimItemReceiver) = unbounded();

        // Build lookup table: display text -> session data
        // This is more reliable than downcasting, which can fail with skim's internal wrapping
        let mut session_lookup: HashMap<String, (&Session, PathBuf)> = HashMap::new();

        for session in &current_sessions {
            let display = format!(
                "{:<6} {:<6} {:<8} {:<12} {}",
                format_time_relative(session.created),
                format_time_relative(session.modified),
                session.source.display_name(),
                session.project,
                format_session_desc(session, 45),
            );
            session_lookup.insert(display.clone(), (session, session.filepath.clone()));

            let item = SessionItem {
                filepath: session.filepath.clone(),
                display,
                search_pattern: search_pattern.clone(),
            };
            let _ = tx.send(Arc::new(item));
        }
        drop(tx);

        let output = Skim::run_with(&options, Some(rx));

        match output {
            Some(out) if out.is_abort => {
                // Esc pressed - if searching, clear search; otherwise exit
                if search_pattern.is_some() {
                    search_pattern = None;
                    current_sessions = sessions.iter().collect();
                    continue;
                }
                return Ok(());
            }
            Some(out) => {
                // Check if ctrl+s was pressed (query will be in out.query)
                let query = out.query.trim();

                // ctrl+s triggers search if there's a query
                if out.final_key == Key::Ctrl('s') {
                    if query.is_empty() {
                        continue; // No query, just re-show
                    }

                    // Search transcripts across all sources
                    match claude_code::search_all_sessions(config, query, None) {
                        Ok(matched) => {
                            // Filter to matched session IDs
                            let matched_ids: std::collections::HashSet<_> =
                                matched.iter().map(|s| &s.id).collect();
                            current_sessions = sessions
                                .iter()
                                .filter(|s| matched_ids.contains(&s.id))
                                .collect();
                            search_pattern = Some(query.to_string());
                        }
                        Err(e) => {
                            eprintln!("Search error: {}", e);
                        }
                    }
                    continue;
                }

                // Normal selection (Enter)
                if let Some(item) = out.selected_items.first() {
                    // Use text() to get display string, then look up in our table
                    // This is more reliable than downcasting which can fail with skim
                    let display_text = item.text().to_string();
                    let (session, filepath) = session_lookup
                        .get(&display_text)
                        .context("Session not found in lookup table")?;

                    resume_session(session, filepath, fork)?;
                    return Ok(());
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
    search_pattern: Option<String>, // When set, preview shows matching lines
}

impl SkimItem for SessionItem {
    fn text(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.display)
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
}
