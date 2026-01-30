mod claude_code;

use anyhow::{Context, Result};
use clap::Parser;
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

// =============================================================================
// CLI Interface
// =============================================================================

#[derive(Parser)]
#[command(name = "cc-session", about = "List Claude Code sessions")]
struct Args {
    /// Number of sessions to show
    #[arg(short, long, default_value = "15")]
    count: usize,

    /// Interactive mode with fzf
    #[arg(short, long)]
    interactive: bool,

    /// Fork session instead of resuming (creates new session ID)
    #[arg(short, long)]
    fork: bool,

    /// Filter by project name (substring match, case-insensitive)
    #[arg(short, long)]
    project: Option<String>,

    /// Search transcript contents (used internally by interactive mode)
    #[arg(short, long)]
    search: Option<String>,

    /// Debug mode - show where sessions come from
    #[arg(long)]
    debug: bool,
}

// =============================================================================
// Session Model (abstraction layer)
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SessionSource {
    Indexed,
    Orphan,
}

#[derive(Debug)]
pub struct Session {
    pub id: String,
    pub source: SessionSource,
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
    let projects_dir = claude_code::get_claude_projects_dir()?;

    if !projects_dir.exists() {
        anyhow::bail!("No Claude sessions found at {:?}", projects_dir);
    }

    // Search mode: find sessions matching pattern and output for fzf
    if let Some(ref pattern) = args.search {
        if pattern.is_empty() {
            // Empty pattern: return all sessions in fzf format
            let sessions = claude_code::find_sessions(&projects_dir)?;
            print_sessions_fzf(&sessions);
        } else {
            let sessions = claude_code::search_sessions(&projects_dir, pattern)?;
            print_sessions_fzf(&sessions);
        }
        return Ok(());
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

    if args.interactive || args.fork {
        interactive_mode(&sessions, args.fork)?;
    } else {
        print_sessions(&sessions, args.count, args.debug);
    }

    Ok(())
}

// =============================================================================
// Display Functions
// =============================================================================

fn print_sessions(sessions: &[Session], count: usize, debug: bool) {
    if debug {
        println!(
            "{:<6} {:<6} {:<8} {:<16} {}",
            "CREAT", "MOD", "SOURCE", "PROJECT", "SUMMARY"
        );
        println!("{}", "─".repeat(100));

        for session in sessions.iter().take(count) {
            let created = format_time_relative(session.created);
            let modified = format_time_relative(session.modified);
            let source = match session.source {
                SessionSource::Indexed => "index",
                SessionSource::Orphan => "orphan",
            };
            let desc = format_session_desc(session, 45);

            println!(
                "{:<6} {:<6} {:<8} {:<16} {}",
                created, modified, source, session.project, desc
            );
        }

        // Show stats
        let indexed = sessions
            .iter()
            .filter(|s| s.source == SessionSource::Indexed)
            .count();
        let orphans = sessions
            .iter()
            .filter(|s| s.source == SessionSource::Orphan)
            .count();
        println!("{}", "─".repeat(100));
        println!(
            "Total: {} (indexed: {}, orphans: {})",
            sessions.len(),
            indexed,
            orphans
        );
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
        println!("Use 'cc-sessions -i' for interactive picker, -f to fork");
    }
}

/// Print sessions in fzf-compatible tab-delimited format
fn print_sessions_fzf(sessions: &[Session]) {
    for s in sessions.iter().take(50) {
        println!("{}", format_session_fzf(s));
    }
}

/// Format a single session for fzf (tab-delimited)
fn format_session_fzf(s: &Session) -> String {
    let modified = format_time_relative(s.modified);
    let created = format_time_relative(s.created);
    let desc = format_session_desc(s, 50);
    format!(
        "{}\t{}\t{}\t{}\t{:<6} {:<6} {:<12} {}",
        s.filepath.display(),
        s.id,
        s.project_path,
        s.summary.as_deref().unwrap_or(""),
        created,
        modified,
        s.project,
        desc
    )
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
// Interactive Mode (fzf integration)
// =============================================================================

fn interactive_mode(sessions: &[Session], fork: bool) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    // Get path to current executable for search reload
    let exe_path = std::env::current_exe().context("Could not get executable path")?;

    let input: String = sessions
        .iter()
        .map(|s| format_session_fzf(s))
        .collect::<Vec<_>>()
        .join("\n");

    // Write session list to temp file for reload back to normal mode
    let temp_file = std::env::temp_dir().join(format!("cc-sessions-{}.txt", std::process::id()));
    fs::write(&temp_file, &input)?;

    // Preview: in search mode show rg matches, otherwise show transcript
    let preview_cmd = r#"f=$(echo {} | cut -f1); q="$FZF_QUERY"; [ -f "$f" ] && { if [ -n "$q" ] && [ "$FZF_PROMPT" = "search> " ]; then rg --color=always -C1 "$q" "$f" 2>/dev/null | head -80 || echo "No matches"; else jaq -r 'if .type=="user" then .message.content[]? | select(.type=="text") | "👤 " + (.text | split("\n")[0]) elif .type=="assistant" then .message.content[]? | select(.type=="text") | "🤖 " + (.text | split("\n")[0] | if length > 80 then .[0:80] + "..." else . end) else empty end' "$f" 2>/dev/null | grep -v "^. $" | grep -v "\[Request" | head -100; fi; } || echo "No preview""#;

    let header = if fork {
        "FORK │ ctrl-s: search transcripts │ ctrl-n: normal"
    } else {
        "ctrl-s: search transcripts │ ctrl-n: normal │ CREAT MOD PROJECT"
    };

    // Keybindings: ctrl-s enables search mode, ctrl-n returns to filter mode
    let bind_search = format!(
        "ctrl-s:disable-search+change-prompt(search> )+reload({} --search {{q}})",
        exe_path.display()
    );
    let bind_normal = format!(
        "ctrl-n:enable-search+change-prompt(filter> )+reload(cat {})+clear-query",
        temp_file.display()
    );
    let bind_change = format!("change:reload:{} --search {{q}}", exe_path.display());

    let mut fzf = Command::new("fzf")
        .args([
            "--delimiter=\t",
            "--with-nth=5..",
            "--preview",
            preview_cmd,
            "--preview-window=right:50%:wrap",
            &format!("--header={}", header),
            "--prompt=filter> ",
            "--no-hscroll",
            "--ansi",
            "--bind",
            &bind_search,
            "--bind",
            &bind_normal,
            "--bind",
            &bind_change,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("Failed to spawn fzf - is it installed?")?;

    if let Some(mut stdin) = fzf.stdin.take() {
        stdin.write_all(input.as_bytes())?;
    }

    let output = fzf.wait_with_output()?;

    // Clean up temp file
    let _ = fs::remove_file(&temp_file);

    if output.status.success() {
        let selected = String::from_utf8_lossy(&output.stdout);
        let parts: Vec<&str> = selected.trim().split('\t').collect();
        if parts.len() >= 3 {
            let session_id = parts[1];
            let project_path = parts[2];

            let action = if fork { "Forking" } else { "Resuming" };
            println!("{} session {} in {}", action, session_id, project_path);

            // Change to project directory and run claude
            let fork_flag = if fork { " --fork-session" } else { "" };
            let cmd = format!(
                "cd '{}' && claude -r '{}'{}",
                project_path, session_id, fork_flag
            );
            Command::new("zsh").args(["-c", &cmd]).status()?;
        }
    }

    Ok(())
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
