mod claude_code;
mod interactive_state;
mod message_classification;
mod remote;
mod session;

use anyhow::Result;
use clap::Parser;
use claude_code::{Message, Role, TextDetail};
use interactive_state::{Action as StateAction, Effect as StateEffect, InteractiveState};
use session::{Session, SessionSource};
use skim::prelude::*;
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

// =============================================================================
// CLI Interface
// =============================================================================

#[derive(Parser)]
#[command(
    name = "cc-sessions",
    version,
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
fn print_session_preview(filepath: &Path) -> Result<()> {
    let content = generate_preview_content(filepath)?;
    print!("{}", content);
    Ok(())
}

/// Render the scrollback preview: one line per message, first text line only.
/// Pure — operates entirely on already-read messages.
fn render_preview(messages: &[Message]) -> String {
    use std::fmt::Write as _;

    let mut output = String::new();
    for msg in messages {
        let (glyph, color) = match msg.role {
            Role::User => ('U', colors::CYAN),
            Role::Assistant => ('A', colors::YELLOW),
        };
        let first_line = msg.text.lines().next().unwrap_or(&msg.text);
        let _ = writeln!(output, "{color}{glyph}: {first_line}{}", colors::RESET);
    }

    if output.is_empty() {
        output.push_str("(empty session)");
    }

    output
}

/// Generate preview content as a string (for skim's preview pane). Skim is
/// configured with `:wrap`, so we emit untruncated lines and let the pane
/// handle overflow — no arbitrary width caps. Reads only first lines (the
/// preview shows one line per message) so large message bodies are never
/// cloned on skim's per-keystroke preview path.
fn generate_preview_content(filepath: &Path) -> Result<String> {
    const MAX_MESSAGES: usize = 100;
    let messages = claude_code::read_messages(filepath, Some(MAX_MESSAGES), TextDetail::FirstLine)?;
    Ok(render_preview(&messages))
}

/// Generate preview showing matching messages with full conversation context.
/// Thin I/O shell over [`claude_code::read_messages`] + [`render_search_preview`].
fn generate_search_preview(filepath: &Path, pattern: &str) -> Result<String> {
    // Search must find matches anywhere, so read the whole transcript (no cap).
    let messages = claude_code::read_messages(filepath, None, TextDetail::Full)?;
    Ok(render_search_preview(&messages, pattern))
}

/// Render search results: each matching message with one message of surrounding
/// context on either side, the matched substring highlighted. Pure — operates
/// entirely on already-read messages.
fn render_search_preview(messages: &[Message], pattern: &str) -> String {
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

    output
}

/// Format a context message (dimmed, truncated if too long)
fn format_context_message(msg: &Message) -> String {
    let prefix = if msg.role == Role::User { "U" } else { "A" };
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
    let (prefix, color) = match msg.role {
        Role::User => ("U", colors::CYAN),
        Role::Assistant => ("A", colors::YELLOW),
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

/// A fully-resolved resume invocation, as data. Constructed by the pure
/// [`build_resume_command`] so command/argument assembly — including remote
/// shell escaping, which is security-sensitive — is unit-testable without
/// spawning a process.
#[derive(Debug, PartialEq, Eq)]
enum ResumeCommand {
    /// Run `claude -r <id> [--fork-session]` with cwd `dir`. No shell involved.
    Local { dir: String, args: Vec<String> },
    /// Run `ssh -t <target> <remote_cmd>`; `remote_cmd` is a shell string.
    Remote { target: String, remote_cmd: String },
}

/// Build the resume/fork invocation for a session. Pure — no I/O.
fn build_resume_command(session: &Session, fork: bool) -> ResumeCommand {
    match &session.source {
        SessionSource::Local => {
            // Invoke claude directly — no shell, no escaping needed.
            let mut args = vec!["-r".to_string(), session.id.clone()];
            if fork {
                args.push("--fork-session".to_string());
            }
            ResumeCommand::Local {
                dir: session.project_path.clone(),
                args,
            }
        }
        SessionSource::Remote { host, user, .. } => {
            let target = remote::format_ssh_target(host, user.as_deref());

            // Remote requires a shell string — escape for safe single-quoting.
            let fork_flag = if fork { " --fork-session" } else { "" };
            let remote_cmd = format!(
                "cd '{}' && claude -r '{}'{}",
                shell_escape(&session.project_path),
                shell_escape(&session.id),
                fork_flag
            );
            ResumeCommand::Remote { target, remote_cmd }
        }
    }
}

/// Resume or fork a session, handling both local and remote sessions.
fn resume_session(session: &Session, filepath: &std::path::Path, fork: bool) -> Result<()> {
    use std::process::Command;

    let action = if fork { "Forking" } else { "Resuming" };

    // Validate project path
    if session.project_path.is_empty() {
        eprintln!("Error: Session {} has no project path recorded", session.id);
        eprintln!("Session file: {}", filepath.display());
        anyhow::bail!("Cannot resume: no project path");
    }

    let status = match build_resume_command(session, fork) {
        ResumeCommand::Local { dir, args } => {
            // Verify directory exists locally
            if !std::path::Path::new(&dir).exists() {
                eprintln!("Error: Project directory no longer exists: {}", dir);
                eprintln!("Session file: {}", filepath.display());
                anyhow::bail!("Cannot resume: directory '{}' not found", dir);
            }

            println!("{} session {} in {}", action, session.id, dir);

            Command::new("claude")
                .current_dir(&dir)
                .args(&args)
                .status()?
        }
        ResumeCommand::Remote { target, remote_cmd } => {
            println!(
                "{} remote session {} on {} in {}",
                action,
                session.id,
                session.source.display_name(),
                session.project_path
            );

            // -t allocates a pseudo-TTY (required for claude's interactive mode)
            Command::new("ssh")
                .args(["-t", &target, &remote_cmd])
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
        children.sort_by_key(|s| std::cmp::Reverse(s.modified));
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
    let focus_info = focus
        .filter(|_| search_pattern.is_none())
        .and_then(|id| session_by_id.get(id))
        .map(|s| format!(" │ in [{}]", format_session_desc(s, 30)))
        .unwrap_or_default();

    let mode = if fork { "FORK mode" } else { "Select session" };
    let status_line = match (search_pattern, search_count) {
        (Some(pat), Some(count)) => format!("{} │ search: \"{}\" ({} matches)", mode, pat, count),
        (Some(pat), None) => format!("{} │ search: \"{}\"", mode, pat),
        (None, _) => format!("{}{}", mode, focus_info),
    };

    let legend = build_column_legend(debug);
    let keys = build_key_hints(search_pattern.is_some(), focus.is_some(), fork);
    format!("{}\n{}\n{}", status_line, legend, keys)
}

/// Shortcut line shown under the column legend. Only lists keys that do
/// something in the current view (←/→ are inert while searching).
fn build_key_hints(searching: bool, focused: bool, fork: bool) -> String {
    let enter = if fork { "enter fork" } else { "enter resume" };
    let keys: &[&str] = match (searching, focused) {
        (true, _) => &[enter, "alt+p preview", "esc clear search"],
        // Kept short: the header shares the list pane (~half the terminal).
        (false, true) => &[
            enter,
            "→/← forks",
            "ctrl+s search",
            "alt+p preview",
            "esc root",
        ],
        (false, false) => &[
            enter,
            "→ forks",
            "ctrl+s search",
            "alt+p preview",
            "esc quit",
        ],
    };
    format!("  {}", keys.join(" · "))
}

/// Width (in columns) consumed by the fixed fields before SUMMARY:
/// prefix (2) + CRE (4+1) + MOD (4+1) + MSG (3+1) + SOURCE (6+1) + PROJECT (12+1).
const FIXED_COLS: usize = 36;

/// Simple session row format (no tree glyphs). `desc_width` is the budget for
/// the trailing summary column — caller computes it from the available pane
/// width so we only truncate when we actually run out of space.
fn format_session_row_simple(
    prefix: &str,
    session: &Session,
    debug: bool,
    desc_width: usize,
) -> String {
    let created = format_time_relative(session.created);
    let modified = format_time_relative(session.modified);
    let source = session.source.display_name();
    let id_prefix = if debug {
        format!("{:<6}", &session.id[..5.min(session.id.len())])
    } else {
        String::new()
    };
    let msgs = format!("{:>3}", session.turn_count);

    // PROJECT column is fixed at 12 chars so FIXED_COLS arithmetic holds.
    // Long project names are middle-elided (keeps both prefix and suffix
    // readable — `claude-cli-internal` → `claud…ternal`).
    let project = elide_middle(&session.project, 12);

    let desc = format_session_desc(session, desc_width);

    format!(
        "{}{}{:<4} {:<4} {} {:<6} {:<12} {}",
        prefix, id_prefix, created, modified, msgs, source, project, desc,
    )
}

/// Middle-elide a string to at most `max` chars. Keeps roughly equal head and
/// tail, inserts `…` between them. Returns a `Cow` to avoid allocating when
/// the input already fits.
fn elide_middle(s: &str, max: usize) -> Cow<'_, str> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return Cow::Borrowed(s);
    }
    let head = (max - 1) / 2;
    let tail = max - 1 - head;
    let mut out = String::with_capacity(max);
    out.extend(&chars[..head]);
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    Cow::Owned(out)
}

/// Available width for the SUMMARY column given the list pane width.
/// Floors at a small minimum so very narrow terminals still show something.
fn desc_budget(pane_width: u16, debug: bool) -> usize {
    let fixed = FIXED_COLS + if debug { 6 } else { 0 };
    (pane_width as usize).saturating_sub(fixed).max(20)
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
        // Rows are built for the full terminal width (preview hidden) and
        // clipped to the live list-pane width in `SessionItem::display`, so
        // toggling the preview or resizing reflows without restarting skim.
        let (term_w, _) = crossterm::terminal::size().unwrap_or((160, 40));
        let desc_width = desc_budget(term_w, debug);

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
            // Rows are pre-fitted to the pane; hscroll would shift a row
            // sideways to chase a match in its clipped tail.
            .no_hscroll(true)
            .bind(vec![
                // Tagged accepts: skim 5 overwrites `final_key` with synthetic
                // events (change/focus/...), so the tag is the only reliable
                // way to tell which key ended the run.
                "ctrl-s:accept(ctrl-s)".to_string(),
                "right:accept(right)".to_string(),
                "left:accept(left)".to_string(),
                // Handled inside skim (not via accept) so query and cursor survive
                "alt-p:toggle-preview".to_string(),
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
                    display: format_session_row_simple(prefix, session, debug, desc_width),
                    session_id: session.id.clone(),
                    named: session.name.is_some(),
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

        let accept_tag = match &out.final_event {
            Event::Action(Action::Accept(tag)) => tag.as_deref(),
            _ => None,
        };

        if accept_tag == Some("ctrl-s") {
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

        if accept_tag == Some("right") {
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
        if accept_tag == Some("left") {
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
    named: bool,                    // Has a custom title — render bold+yellow
    search_pattern: Option<String>, // When set, preview shows matching lines
}

impl SkimItem for SessionItem {
    fn text(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.display)
    }

    fn display<'a>(&'a self, mut context: DisplayContext) -> ratatui::text::Line<'a> {
        use ratatui::style::{Color, Modifier};
        if self.named {
            context.base_style = context
                .base_style
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD);
        }
        let width = context.container_width;
        if self.display.chars().count() <= width {
            return context.to_line(Cow::Borrowed(&self.display));
        }
        let (clipped, matches) = clip_to_width(&self.display, &context.matches, width);
        context.matches = matches;
        context.to_line(Cow::Borrowed(clipped))
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

/// Clip `text` to `width` chars and drop/trim match highlights that fall in
/// the clipped tail (skim's `to_line` would otherwise emit NUL spans for
/// out-of-range char indices and panic on out-of-range byte ranges).
fn clip_to_width<'a>(text: &'a str, matches: &Matches, width: usize) -> (&'a str, Matches) {
    let end = text
        .char_indices()
        .nth(width)
        .map_or(text.len(), |(i, _)| i);
    let clipped = &text[..end];
    let matches = match matches {
        Matches::CharIndices(idx) => {
            Matches::CharIndices(idx.iter().copied().filter(|&i| i < width).collect())
        }
        Matches::CharRange(start, stop) if *start < width => {
            Matches::CharRange(*start, (*stop).min(width))
        }
        Matches::ByteRange(start, stop) if *start < end => {
            Matches::ByteRange(*start, (*stop).min(end))
        }
        _ => Matches::None,
    };
    (clipped, matches)
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
        let lines: Vec<&str> = header.lines().collect();
        assert!(lines[0].contains("Select session"));
        assert!(lines[1].contains("CRE")); // Legend line
        // Shortcuts sit below the column titles
        assert!(lines[2].contains("→ forks"));
        assert!(lines[2].contains("esc quit"));
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
        assert!(header.contains("esc clear search"));
        // ←/→ are inert while searching, so don't advertise them
        assert!(!header.contains("→ forks"));
    }

    #[test]
    fn build_subtree_header_focused_shows_back() {
        use std::collections::HashMap;
        let session = test_session("focused");
        let mut session_by_id: HashMap<&str, &Session> = HashMap::new();
        session_by_id.insert("focused", &session);

        let header =
            build_subtree_header(None, None, false, Some("focused"), &session_by_id, false);
        assert!(header.contains("→/← forks"));
        assert!(header.contains("esc root"));
        assert!(header.contains("in [test summary]"));
    }

    #[test]
    fn key_hints_fork_mode_says_fork() {
        assert!(build_key_hints(false, false, true).contains("enter fork"));
        assert!(build_key_hints(false, false, false).contains("enter resume"));
    }

    // =========================================================================
    // Row clipping (preview toggle reflow)
    // =========================================================================

    #[test]
    fn clip_to_width_drops_highlights_past_the_edge() {
        let (text, m) = clip_to_width("héllo world", &Matches::CharIndices(vec![1, 4, 8]), 5);
        assert_eq!(text, "héllo");
        assert!(matches!(m, Matches::CharIndices(ref v) if v == &vec![1, 4]));
    }

    #[test]
    fn clip_to_width_trims_ranges() {
        let (_, m) = clip_to_width("abcdefgh", &Matches::CharRange(3, 7), 5);
        assert!(matches!(m, Matches::CharRange(3, 5)));
        let (_, m) = clip_to_width("abcdefgh", &Matches::CharRange(6, 7), 5);
        assert!(matches!(m, Matches::None));
        // Byte range clamps to a char boundary ("é" is 2 bytes)
        let (text, m) = clip_to_width("héllo world", &Matches::ByteRange(1, 9), 3);
        assert_eq!(text, "hél");
        assert!(matches!(m, Matches::ByteRange(1, 4)));
    }

    // =========================================================================
    // Session row formatting
    // =========================================================================

    #[test]
    fn format_session_row_simple_basic() {
        let session = test_session("test-id");
        let row = format_session_row_simple("  ", &session, false, 40);

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
        let row = format_session_row_simple("▶ ", &session, true, 40);

        // Should contain first 5 chars of ID
        assert!(row.contains("abcde"));
        // Should contain the prefix
        assert!(row.starts_with("▶ "));
    }

    #[test]
    fn elide_middle_passthrough_when_fits() {
        assert_eq!(elide_middle("short", 12), "short");
        assert_eq!(elide_middle("exactly-12ch", 12), "exactly-12ch");
    }

    #[test]
    fn elide_middle_shortens_long_names() {
        let out = elide_middle("claude-cli-internal", 12);
        assert_eq!(out.chars().count(), 12);
        assert!(out.contains('…'));
        // Keeps head and tail readable
        assert!(out.starts_with("claud"));
        assert!(out.ends_with("ternal"));
    }

    #[test]
    fn desc_budget_scales_with_pane_width() {
        // 200-col pane → 200 − 36 fixed = 164
        assert_eq!(desc_budget(200, false), 164);
        // Debug adds 6 for the ID prefix
        assert_eq!(desc_budget(200, true), 158);
        // Narrow pane floors at 20
        assert_eq!(desc_budget(40, false), 20);
    }

    #[test]
    fn format_session_row_simple_shows_turn_count() {
        let mut session = test_session("test");
        session.turn_count = 42;
        let row = format_session_row_simple("  ", &session, false, 40);

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

    // =========================================================================
    // Preview rendering (pure — no disk, operates on &[Message])
    // =========================================================================

    fn msg(role: Role, text: &str) -> Message {
        Message {
            role,
            text: text.to_string(),
        }
    }

    #[test]
    fn render_preview_empty_is_placeholder() {
        assert_eq!(render_preview(&[]), "(empty session)");
    }

    #[test]
    fn render_preview_uses_glyphs_and_first_line_only() {
        let messages = [
            msg(Role::User, "first line\nsecond line"),
            msg(Role::Assistant, "reply"),
        ];
        let out = render_preview(&messages);
        assert!(out.contains("U: first line"));
        assert!(out.contains("A: reply"));
        // Only the first line of a multi-line message is shown.
        assert!(!out.contains("second line"));
    }

    #[test]
    fn render_search_preview_highlights_match_with_context() {
        let messages = [
            msg(Role::User, "tell me about the api"),
            msg(Role::Assistant, "the API is healthy"),
            msg(Role::User, "thanks"),
        ];
        let out = render_search_preview(&messages, "healthy");
        assert!(out.contains(colors::BOLD_INVERSE)); // highlighted match
        assert!(out.contains("matching messages"));
        // Surrounding context is included.
        assert!(out.contains("tell me about the api"));
    }

    #[test]
    fn render_search_preview_no_match() {
        let messages = [msg(Role::User, "hello"), msg(Role::Assistant, "hi")];
        let out = render_search_preview(&messages, "nonexistent");
        assert!(out.contains("(no matches in transcript)"));
    }

    // =========================================================================
    // Resume command construction (pure — security-sensitive escaping)
    // =========================================================================

    fn remote_session(id: &str, user: Option<&str>) -> Session {
        Session {
            source: SessionSource::Remote {
                name: "devbox".to_string(),
                host: "devbox.internal".to_string(),
                user: user.map(str::to_string),
            },
            ..test_session(id)
        }
    }

    #[test]
    fn build_resume_command_local_no_fork() {
        let session = test_session("abc");
        assert_eq!(
            build_resume_command(&session, false),
            ResumeCommand::Local {
                dir: "/tmp/test-project".to_string(),
                args: vec!["-r".to_string(), "abc".to_string()],
            }
        );
    }

    #[test]
    fn build_resume_command_local_fork_appends_flag() {
        let session = test_session("abc");
        let ResumeCommand::Local { args, .. } = build_resume_command(&session, true) else {
            panic!("expected local command");
        };
        assert_eq!(args, ["-r", "abc", "--fork-session"]);
    }

    #[test]
    fn build_resume_command_remote_target_and_fork() {
        let session = remote_session("xyz", Some("ec2-user"));
        let ResumeCommand::Remote { target, remote_cmd } = build_resume_command(&session, true)
        else {
            panic!("expected remote command");
        };
        assert_eq!(target, "ec2-user@devbox.internal");
        assert_eq!(
            remote_cmd,
            "cd '/tmp/test-project' && claude -r 'xyz' --fork-session"
        );
    }

    #[test]
    fn build_resume_command_remote_no_user_uses_bare_host() {
        let session = remote_session("xyz", None);
        let ResumeCommand::Remote { target, .. } = build_resume_command(&session, false) else {
            panic!("expected remote command");
        };
        assert_eq!(target, "devbox.internal");
    }

    #[test]
    fn build_resume_command_remote_escapes_single_quotes() {
        // A project path containing a single quote must be safely escaped so it
        // can't break out of the single-quoted shell argument.
        let mut session = remote_session("s'id", None);
        session.project_path = "/tmp/it's mine".to_string();
        let ResumeCommand::Remote { remote_cmd, .. } = build_resume_command(&session, false) else {
            panic!("expected remote command");
        };
        assert_eq!(
            remote_cmd,
            r#"cd '/tmp/it'\''s mine' && claude -r 's'\''id'"#
        );
    }
}
