mod claude_code;

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

    /// Preview a session file (internal use by interactive mode)
    #[arg(long, value_name = "FILE")]
    preview: Option<PathBuf>,

    /// Debug mode - show session IDs and stats
    #[arg(long)]
    debug: bool,
}

// =============================================================================
// Session Model (abstraction layer)
// =============================================================================

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
    pub name: Option<String>, // customTitle from /rename - indicates important session
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

    let projects_dir = claude_code::get_claude_projects_dir()?;

    if !projects_dir.exists() {
        anyhow::bail!("No Claude sessions found at {:?}", projects_dir);
    }

    let mut sessions = claude_code::find_sessions(&projects_dir)?;

    // Filter by project name if specified
    if let Some(ref filter) = args.project {
        let filter_lower = filter.to_lowercase();
        sessions.retain(|s| s.project.to_lowercase().contains(&filter_lower));
    }

    if sessions.is_empty() {
        if args.project.is_some() {
            anyhow::bail!("No sessions found matching project filter");
        }
        anyhow::bail!("No sessions found");
    }

    if args.list {
        print_sessions(&sessions, args.count, args.debug);
    } else {
        interactive_mode(&sessions, args.fork)?;
    }

    Ok(())
}

// =============================================================================
// Display Functions
// =============================================================================

fn print_sessions(sessions: &[Session], count: usize, debug: bool) {
    if debug {
        println!(
            "{:<6} {:<6} {:<16} {:<40} {}",
            "CREAT", "MOD", "PROJECT", "ID", "SUMMARY"
        );
        println!("{}", "─".repeat(110));

        for session in sessions.iter().take(count) {
            let created = format_time_relative(session.created);
            let modified = format_time_relative(session.modified);
            let id_short = if session.id.len() > 36 {
                &session.id[..36]
            } else {
                &session.id
            };
            let desc = format_session_desc(session, 35);

            println!(
                "{:<6} {:<6} {:<16} {:<40} {}",
                created, modified, session.project, id_short, desc
            );
        }

        println!("{}", "─".repeat(110));
        println!("Total: {} sessions", sessions.len());
    } else {
        println!(
            "{:<6} {:<6} {:<16} {}",
            "CREAT", "MOD", "PROJECT", "SUMMARY"
        );
        println!("{}", "─".repeat(90));

        for session in sessions.iter().take(count) {
            let created = format_time_relative(session.created);
            let modified = format_time_relative(session.modified);
            let desc = format_session_desc(session, 55);

            println!(
                "{:<6} {:<6} {:<16} {}",
                created, modified, session.project, desc
            );
        }

        println!("{}", "─".repeat(90));
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
    if let Some(ref name) = session.name {
        // Named sessions show ★ prefix with name, then summary if space allows
        let prefix = format!("★ {}", name);
        if prefix.chars().count() >= max_chars {
            return prefix.chars().take(max_chars).collect();
        }
        if let Some(ref summary) = session.summary {
            let remaining = max_chars - prefix.chars().count() - 3; // " - " separator
            if remaining > 10 {
                let summary_truncated: String = summary.chars().take(remaining).collect();
                return format!("{} - {}", prefix, summary_truncated);
            }
        }
        return prefix;
    }

    // No name - use summary or first_message
    session
        .summary
        .as_deref()
        .or(session.first_message.as_deref())
        .map(|s| s.chars().take(max_chars).collect())
        .unwrap_or_default()
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
// Preview Mode (internal, replaces jaq dependency)
// =============================================================================

/// Print formatted transcript preview for a session file.
/// Used internally by skim's preview command.
fn print_session_preview(filepath: &PathBuf) -> Result<()> {
    use std::fs::File;
    use std::io::{BufRead, BufReader};

    let file = File::open(filepath).context("Could not open session file")?;
    let reader = BufReader::new(file);

    // ANSI colors: cyan for user, yellow for assistant
    const CYAN: &str = "\x1b[36m";
    const YELLOW: &str = "\x1b[33m";
    const RESET: &str = "\x1b[0m";

    let mut line_count = 0;
    const MAX_LINES: usize = 100;

    for line in reader.lines() {
        if line_count >= MAX_LINES {
            break;
        }

        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        let entry: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let entry_type = entry.get("type").and_then(|v| v.as_str());

        match entry_type {
            Some("user") => {
                if let Some(text) = extract_message_text(&entry) {
                    // Skip system prompts and XML content
                    if !text.starts_with('[') && !text.starts_with('<') && !text.starts_with('/') {
                        let first_line = text.lines().next().unwrap_or(&text);
                        let truncated = truncate_str(first_line, 120);
                        println!("{}U: {}{}", CYAN, truncated, RESET);
                        line_count += 1;
                    }
                }
            }
            Some("assistant") => {
                if let Some(text) = extract_message_text(&entry) {
                    let first_line = text.lines().next().unwrap_or(&text);
                    let truncated = truncate_str(first_line, 80);
                    println!("{}A: {}{}", YELLOW, truncated, RESET);
                    line_count += 1;
                }
            }
            _ => {}
        }
    }

    if line_count == 0 {
        println!("(empty session)");
    }

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

    const CYAN: &str = "\x1b[36m";
    const YELLOW: &str = "\x1b[33m";
    const RESET: &str = "\x1b[0m";

    let mut output = String::new();
    let mut line_count = 0;
    const MAX_LINES: usize = 100;

    for line in reader.lines() {
        if line_count >= MAX_LINES {
            break;
        }

        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        let entry: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let entry_type = entry.get("type").and_then(|v| v.as_str());

        match entry_type {
            Some("user") => {
                if let Some(text) = extract_message_text(&entry) {
                    if !text.starts_with('[') && !text.starts_with('<') && !text.starts_with('/') {
                        let first_line = text.lines().next().unwrap_or(&text);
                        let truncated = truncate_str(first_line, 120);
                        output.push_str(&format!("{}U: {}{}\n", CYAN, truncated, RESET));
                        line_count += 1;
                    }
                }
            }
            Some("assistant") => {
                if let Some(text) = extract_message_text(&entry) {
                    let first_line = text.lines().next().unwrap_or(&text);
                    let truncated = truncate_str(first_line, 80);
                    output.push_str(&format!("{}A: {}{}\n", YELLOW, truncated, RESET));
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
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

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
            if role == "user"
                && (text.starts_with('[') || text.starts_with('<') || text.starts_with('/'))
            {
                continue;
            }
            messages.push(Message {
                role: role.to_string(),
                text,
            });
        }
    }

    const CYAN: &str = "\x1b[36m";
    const YELLOW: &str = "\x1b[33m";
    const GREEN: &str = "\x1b[32m";
    const DIM: &str = "\x1b[2m";
    const BOLD: &str = "\x1b[1m";
    const RESET: &str = "\x1b[0m";

    let pattern_lower = pattern.to_lowercase();
    let mut output = String::new();
    let mut match_count = 0;
    const MAX_MATCHES: usize = 10; // Fewer matches since we show full context

    output.push_str(&format!(
        "{}Searching for: \"{}\"{}\n\n",
        GREEN, pattern, RESET
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
            output.push_str(&format!("\n{}... more matches truncated{}\n", BOLD, RESET));
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
                DIM, RESET
            ));
        }

        // Show previous message (context)
        if match_idx > 0 && !shown_indices.contains(&(match_idx - 1)) {
            let prev = &messages[match_idx - 1];
            output.push_str(&format_context_message(prev, DIM, RESET));
            output.push_str("\n");
            shown_indices.insert(match_idx - 1);
        }

        // Show matching message (highlighted)
        let msg = &messages[match_idx];
        output.push_str(&format_matching_message(msg, pattern, CYAN, YELLOW, RESET));
        shown_indices.insert(match_idx);
        match_count += 1;

        // Show next message (context)
        if match_idx + 1 < messages.len() && !shown_indices.contains(&(match_idx + 1)) {
            output.push_str("\n");
            let next = &messages[match_idx + 1];
            output.push_str(&format_context_message(next, DIM, RESET));
            shown_indices.insert(match_idx + 1);
        }
    }

    if match_count == 0 {
        output.push_str("(no matches in transcript)");
    } else {
        output.push_str(&format!(
            "\n\n{}{} matching messages{}",
            BOLD, match_count, RESET
        ));
    }

    Ok(output)
}

/// Format a context message (dimmed, truncated if too long)
fn format_context_message(msg: &Message, dim: &str, reset: &str) -> String {
    let prefix = if msg.role == "user" { "U" } else { "A" };
    let max_lines = 10;
    let lines: Vec<&str> = msg.text.lines().collect();

    let mut output = String::new();
    for (i, line) in lines.iter().take(max_lines).enumerate() {
        if i == 0 {
            output.push_str(&format!("{}{}: {}{}\n", dim, prefix, line, reset));
        } else {
            output.push_str(&format!("{}   {}{}\n", dim, line, reset));
        }
    }
    if lines.len() > max_lines {
        output.push_str(&format!(
            "{}   ... ({} more lines){}\n",
            dim,
            lines.len() - max_lines,
            reset
        ));
    }
    output
}

/// Format a matching message (colored, with highlights)
fn format_matching_message(
    msg: &Message,
    pattern: &str,
    cyan: &str,
    yellow: &str,
    reset: &str,
) -> String {
    let (prefix, color) = if msg.role == "user" {
        ("U", cyan)
    } else {
        ("A", yellow)
    };

    let mut output = String::new();
    for (i, line) in msg.text.lines().enumerate() {
        let line_lower = line.to_lowercase();
        let pattern_lower = pattern.to_lowercase();

        let formatted_line = if line_lower.contains(&pattern_lower) {
            highlight_match(line, pattern)
        } else {
            line.to_string()
        };

        if i == 0 {
            output.push_str(&format!(
                "{}{}: {}{}\n",
                color, prefix, formatted_line, reset
            ));
        } else {
            output.push_str(&format!("{}   {}{}\n", color, formatted_line, reset));
        }
    }
    output
}

/// Highlight matching text with bold/inverse
fn highlight_match(text: &str, pattern: &str) -> String {
    const BOLD: &str = "\x1b[1;7m"; // Bold + inverse
    const RESET: &str = "\x1b[0m";

    let text_lower = text.to_lowercase();
    let pattern_lower = pattern.to_lowercase();

    let mut result = String::new();
    let mut last_end = 0;

    for (start, _) in text_lower.match_indices(&pattern_lower) {
        // Add text before match
        result.push_str(&text[last_end..start]);
        // Add highlighted match (using original case from text)
        result.push_str(BOLD);
        result.push_str(&text[start..start + pattern.len()]);
        result.push_str(RESET);
        last_end = start + pattern.len();
    }

    // Add remaining text
    result.push_str(&text[last_end..]);
    result
}

// =============================================================================
// Interactive Mode (skim - no external dependencies)
// =============================================================================

fn interactive_mode(sessions: &[Session], fork: bool) -> Result<()> {
    use skim::prelude::*;
    use std::process::Command;

    let projects_dir = claude_code::get_claude_projects_dir()?;

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

        for session in &current_sessions {
            let item = SessionItem {
                filepath: session.filepath.clone(),
                session_id: session.id.clone(),
                project_path: session.project_path.clone(),
                display: format!(
                    "{:<6} {:<6} {:<12} {}",
                    format_time_relative(session.created),
                    format_time_relative(session.modified),
                    session.project,
                    format_session_desc(session, 50),
                ),
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

                    // Search transcripts for the query
                    match claude_code::search_sessions(&projects_dir, query) {
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
                    let session_item = item
                        .as_any()
                        .downcast_ref::<SessionItem>()
                        .context("Failed to get selected session")?;

                    let action = if fork { "Forking" } else { "Resuming" };
                    println!(
                        "{} session {} in {}",
                        action, session_item.session_id, session_item.project_path
                    );

                    let fork_flag = if fork { " --fork-session" } else { "" };
                    let cmd = format!(
                        "cd '{}' && claude -r '{}'{}",
                        session_item.project_path, session_item.session_id, fork_flag
                    );
                    Command::new("zsh").args(["-c", &cmd]).status()?;
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
    session_id: String,
    project_path: String,
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
        let projects = vec![
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

        assert_eq!(matches("spanish"), vec!["SPANISH-INQUISITION"]);
        assert_eq!(matches("SILLY"), vec!["Ministry-Of-Silly-Walks"]);
        assert_eq!(matches("grail"), vec!["holy-grail"]);
    }

    #[test]
    fn project_filter_substring() {
        let projects = vec!["spam", "spam-eggs", "spam-eggs-spam"];

        let matches = |filter: &str| -> Vec<&str> {
            let filter_lower = filter.to_lowercase();
            projects
                .iter()
                .filter(|p| p.to_lowercase().contains(&filter_lower))
                .copied()
                .collect()
        };

        assert_eq!(matches("spam"), vec!["spam", "spam-eggs", "spam-eggs-spam"]);
        assert_eq!(matches("eggs"), vec!["spam-eggs", "spam-eggs-spam"]);
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
