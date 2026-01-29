use anyhow::{Context, Result};
use clap::Parser;
use rayon::prelude::*;
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

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
}

#[derive(Debug)]
struct Session {
    id: String,
    project: String,
    project_path: String,
    filepath: PathBuf,
    created: SystemTime,
    modified: SystemTime,
    first_message: Option<String>,
    summary: Option<String>,
}

#[derive(Deserialize)]
struct SessionsIndex {
    entries: Vec<IndexEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexEntry {
    session_id: String,
    full_path: String,
    first_prompt: Option<String>,
    summary: Option<String>,
    created: Option<String>,
    modified: Option<String>,
    project_path: Option<String>,
}

fn get_claude_projects_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not find home directory")?;
    Ok(home.join(".claude").join("projects"))
}

fn parse_iso_time(s: &str) -> Option<SystemTime> {
    // Parse ISO 8601 format: 2026-01-15T06:15:58.913Z
    // Simple parsing without pulling in chrono
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

fn find_sessions(projects_dir: &PathBuf) -> Result<Vec<Session>> {
    // Find all sessions-index.json files
    let index_files: Vec<_> = WalkDir::new(projects_dir)
        .min_depth(2)
        .max_depth(2)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() == "sessions-index.json")
        .map(|e| e.path().to_path_buf())
        .collect();

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
                    .filter_map(|entry| {
                        let filepath = PathBuf::from(&entry.full_path);
                        if !filepath.exists() {
                            return None;
                        }

                        let project_path = entry.project_path.unwrap_or_default();
                        let project = project_path
                            .split('/')
                            .last()
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

                        let first_message = entry.first_prompt.as_ref().and_then(|p| {
                            if p == "No prompt" || p.starts_with("[Request") || p.starts_with("/") {
                                None
                            } else {
                                Some(p.chars().take(50).collect())
                            }
                        });

                        Some(Session {
                            id: entry.session_id,
                            project,
                            project_path,
                            filepath,
                            created,
                            modified,
                            first_message,
                            summary: entry.summary,
                        })
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
        .collect();

    let mut sessions = sessions;
    sessions.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(sessions)
}

fn print_sessions(sessions: &[Session], count: usize) {
    println!(
        "{:<6} {:<6} {:<12} {}",
        "CREAT", "MOD", "PROJECT", "SUMMARY"
    );
    println!("{}", "─".repeat(90));

    for session in sessions.iter().take(count) {
        let created = format_time_relative(session.created);
        let modified = format_time_relative(session.modified);
        // Prefer summary, fall back to first_message
        let desc = session
            .summary
            .as_deref()
            .or(session.first_message.as_deref())
            .unwrap_or("");
        // Truncate to fit terminal
        let desc: String = desc.chars().take(60).collect();

        println!(
            "{:<6} {:<6} {:<12} {}",
            created, modified, session.project, desc
        );
    }

    println!("{}", "─".repeat(90));
    println!("Use 'cc-sessions -i' for interactive picker, -f to fork");
}

fn main() -> Result<()> {
    let args = Args::parse();
    let projects_dir = get_claude_projects_dir()?;

    if !projects_dir.exists() {
        anyhow::bail!("No Claude sessions found at {:?}", projects_dir);
    }

    let mut sessions = find_sessions(&projects_dir)?;

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
        print_sessions(&sessions, args.count);
    }

    Ok(())
}

fn interactive_mode(sessions: &[Session], fork: bool) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let input: String = sessions
        .iter()
        .map(|s| {
            let modified = format_time_relative(s.modified);
            let created = format_time_relative(s.created);
            let summary: String = s
                .summary
                .as_deref()
                .unwrap_or("")
                .chars()
                .take(50)
                .collect();
            // Fields: filepath, session_id, project_path, summary (searchable), display
            // fzf searches all fields but only displays field 5+
            format!(
                "{}\t{}\t{}\t{}\t{:<6} {:<6} {:<12} {}",
                s.filepath.display(),
                s.id,
                s.project_path,
                s.summary.as_deref().unwrap_or(""), // full summary for search
                created,
                modified,
                s.project,
                summary
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    // Preview: extract filepath from field 1, run jaq to show transcript
    let preview_cmd = r#"f=$(echo {} | cut -f1); [ -f "$f" ] && jaq -r 'if .type=="user" then .message.content[]? | select(.type=="text") | "👤 " + (.text | split("\n")[0]) elif .type=="assistant" then .message.content[]? | select(.type=="text") | "🤖 " + (.text | split("\n")[0] | if length > 80 then .[0:80] + "..." else . end) else empty end' "$f" 2>/dev/null | grep -v "^. $" | grep -v "\[Request" | head -100 || echo "No preview""#;

    let header = if fork {
        "FORK MODE - CREAT MOD   PROJECT      SUMMARY"
    } else {
        "CREAT  MOD    PROJECT      SUMMARY"
    };

    let mut fzf = Command::new("fzf")
        .args([
            "--delimiter=\t",
            "--with-nth=5..",
            "--preview",
            preview_cmd,
            "--preview-window=right:50%:wrap",
            &format!("--header={}", header),
            "--no-hscroll",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("Failed to spawn fzf - is it installed?")?;

    if let Some(mut stdin) = fzf.stdin.take() {
        stdin.write_all(input.as_bytes())?;
    }

    let output = fzf.wait_with_output()?;

    if output.status.success() {
        let selected = String::from_utf8_lossy(&output.stdout);
        let parts: Vec<&str> = selected.trim().split('\t').collect();
        if parts.len() >= 3 {
            let session_id = parts[1];
            let project_path = parts[2];

            let (action, flag) = if fork {
                ("Forking", "--fork-session")
            } else {
                ("Resuming", "")
            };
            println!("{} session {} in {}", action, session_id, project_path);

            // Change to project directory and run claude
            let cmd = if fork {
                format!(
                    "cd '{}' && claude -r '{}' {}",
                    project_path, session_id, flag
                )
            } else {
                format!("cd '{}' && claude -r '{}'", project_path, session_id)
            };
            Command::new("zsh").args(["-c", &cmd]).status()?;
        }
    }

    Ok(())
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
