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

#[cfg(test)]
mod tests {
    use super::*;

    // =============================================================================
    // parse_iso_time - Critical for session sorting
    // =============================================================================

    #[test]
    fn parse_iso_time_standard_format() {
        // Real format from Claude Code sessions-index.json
        let result = parse_iso_time("2026-01-15T06:15:58.913Z");
        assert!(result.is_some());

        // Verify it's in the right ballpark (after 2025, before 2027)
        let secs = result
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let year_2025 = 55 * 365 * 86400; // ~2025
        let year_2027 = 57 * 365 * 86400; // ~2027
        assert!(secs > year_2025 && secs < year_2027);
    }

    #[test]
    fn parse_iso_time_without_milliseconds() {
        let result = parse_iso_time("2026-01-15T06:15:58Z");
        assert!(result.is_some());
    }

    #[test]
    fn parse_iso_time_ordering_preserved() {
        // Earlier time should produce smaller SystemTime
        let earlier = parse_iso_time("2026-01-15T06:00:00Z").unwrap();
        let later = parse_iso_time("2026-01-15T07:00:00Z").unwrap();
        assert!(earlier < later);

        // Different days
        let day1 = parse_iso_time("2026-01-15T12:00:00Z").unwrap();
        let day2 = parse_iso_time("2026-01-16T12:00:00Z").unwrap();
        assert!(day1 < day2);
    }

    #[test]
    fn parse_iso_time_leap_year() {
        // 2024 is a leap year - Feb 29 should parse
        let result = parse_iso_time("2024-02-29T12:00:00Z");
        assert!(result.is_some());

        // March 1 should be one day later
        let feb29 = parse_iso_time("2024-02-29T12:00:00Z").unwrap();
        let mar1 = parse_iso_time("2024-03-01T12:00:00Z").unwrap();
        let diff = mar1.duration_since(feb29).unwrap().as_secs();
        assert_eq!(diff, 86400); // exactly one day
    }

    #[test]
    fn parse_iso_time_invalid_formats() {
        assert!(parse_iso_time("not a date").is_none());
        assert!(parse_iso_time("2026-01-15").is_none()); // missing time
        assert!(parse_iso_time("06:15:58Z").is_none()); // missing date
        assert!(parse_iso_time("").is_none());
    }

    // =============================================================================
    // IndexEntry deserialization - Critical for reading session metadata
    // =============================================================================

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
        // Only required fields - optional fields missing
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

    // =============================================================================
    // Project name extraction - Used for display and filtering
    // =============================================================================

    #[test]
    fn project_name_from_path() {
        // Simulates the logic in find_sessions
        let extract =
            |path: &str| -> String { path.split('/').last().unwrap_or("unknown").to_string() };

        assert_eq!(extract("/Users/foo/my-project"), "my-project");
        assert_eq!(extract("/home/user/code/bike-power"), "bike-power");
        assert_eq!(extract("single"), "single");
        assert_eq!(extract(""), "");
    }

    // =============================================================================
    // Project filter logic - The -p flag behavior
    // =============================================================================

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

        // Nobody expects the Spanish Inquisition (but we can filter for it)
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

        // Spam, spam, spam, spam...
        assert_eq!(matches("spam"), vec!["spam", "spam-eggs", "spam-eggs-spam"]);
        // Eggs are less common
        assert_eq!(matches("eggs"), vec!["spam-eggs", "spam-eggs-spam"]);
    }

    // =============================================================================
    // Integration test with fake session data
    // =============================================================================

    #[test]
    fn find_sessions_with_fake_data() {
        // Create temp directory structure mimicking ~/.claude/projects/
        let temp_dir = std::env::temp_dir().join(format!("cc-session-test-{}", std::process::id()));
        let project_dir = temp_dir.join("-Users-sirrobin-holy-grail");
        fs::create_dir_all(&project_dir).unwrap();

        // Create fake session files
        let session1_path = project_dir.join("black-knight.jsonl");
        let session2_path = project_dir.join("killer-rabbit.jsonl");
        fs::write(
            &session1_path,
            r#"{"type":"user","message":"Tis but a scratch"}"#,
        )
        .unwrap();
        fs::write(&session2_path, r#"{"type":"user","message":"Run away!"}"#).unwrap();

        // Create sessions-index.json with two sessions
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

        // Run find_sessions
        let sessions = find_sessions(&temp_dir).unwrap();

        // Verify results
        assert_eq!(sessions.len(), 2);

        // Should be sorted by modified time (newest first)
        assert_eq!(sessions[0].id, "killer-rabbit-encounter");
        assert_eq!(sessions[1].id, "black-knight-battle");

        // Verify project name extraction
        assert_eq!(sessions[0].project, "holy-grail");

        // Verify summary is preserved
        assert_eq!(
            sessions[0].summary,
            Some("Deploying Holy Hand Grenade of Antioch".to_string())
        );

        // Cleanup
        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn find_sessions_filters_missing_files() {
        let temp_dir =
            std::env::temp_dir().join(format!("cc-session-test-parrot-{}", std::process::id()));
        let project_dir = temp_dir.join("-Users-shopkeeper-ministry-of-silly-walks");
        fs::create_dir_all(&project_dir).unwrap();

        // The parrot exists (it's just resting)
        let resting_parrot = project_dir.join("norwegian-blue.jsonl");
        fs::write(
            &resting_parrot,
            r#"{"type":"user","message":"Beautiful plumage!"}"#,
        )
        .unwrap();

        // But the ex-parrot has ceased to be
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

        // Only the resting parrot should be found (the ex-parrot is no more)
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
        // The Knights Who Say Ni have corrupted the sacred index
        let temp_dir =
            std::env::temp_dir().join(format!("cc-session-test-ni-{}", std::process::id()));
        let knights_dir = temp_dir.join("-Users-knight-shrubbery");
        let good_dir = temp_dir.join("-Users-arthur-camelot");
        fs::create_dir_all(&knights_dir).unwrap();
        fs::create_dir_all(&good_dir).unwrap();

        // Corrupted index - not valid JSON (the knights demand a shrubbery, not JSON)
        fs::write(
            knights_dir.join("sessions-index.json"),
            "NI! NI! NI! We demand a shrubbery!",
        )
        .unwrap();

        // Good index in another project
        let quest_path = good_dir.join("quest.jsonl");
        fs::write(&quest_path, r#"{"type":"user","message":"What is your quest?"}"#).unwrap();
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

        // Should gracefully skip corrupted index and return sessions from good index
        let sessions = find_sessions(&temp_dir).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "seek-holy-grail");

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn find_sessions_handles_empty_index() {
        // The Bridgekeeper's index has no entries - none shall pass
        let temp_dir =
            std::env::temp_dir().join(format!("cc-session-test-bridge-{}", std::process::id()));
        let bridge_dir = temp_dir.join("-Users-bridgekeeper-gorge");
        fs::create_dir_all(&bridge_dir).unwrap();

        // Valid JSON but empty entries array
        fs::write(
            bridge_dir.join("sessions-index.json"),
            r#"{"entries": []}"#,
        )
        .unwrap();

        let sessions = find_sessions(&temp_dir).unwrap();
        assert_eq!(sessions.len(), 0);

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn find_sessions_merges_multiple_projects() {
        // Sessions from Camelot, the French Castle, and Swamp Castle
        let temp_dir =
            std::env::temp_dir().join(format!("cc-session-test-castles-{}", std::process::id()));

        let camelot = temp_dir.join("-Users-arthur-camelot");
        let french = temp_dir.join("-Users-french-castle");
        let swamp = temp_dir.join("-Users-dennis-swamp-castle");
        fs::create_dir_all(&camelot).unwrap();
        fs::create_dir_all(&french).unwrap();
        fs::create_dir_all(&swamp).unwrap();

        // Camelot session (oldest)
        let camelot_session = camelot.join("round-table.jsonl");
        fs::write(&camelot_session, "{}").unwrap();
        fs::write(
            camelot.join("sessions-index.json"),
            format!(
                r#"{{"entries": [{{
                    "sessionId": "round-table-discussion",
                    "fullPath": "{}",
                    "projectPath": "/Users/arthur/camelot",
                    "summary": "On second thought, let's not go there",
                    "modified": "1975-04-01T10:00:00Z"
                }}]}}"#,
                camelot_session.display()
            ),
        )
        .unwrap();

        // French Castle session (middle)
        let french_session = french.join("taunting.jsonl");
        fs::write(&french_session, "{}").unwrap();
        fs::write(
            french.join("sessions-index.json"),
            format!(
                r#"{{"entries": [{{
                    "sessionId": "taunt-session",
                    "fullPath": "{}",
                    "projectPath": "/Users/french/castle",
                    "summary": "I fart in your general direction",
                    "modified": "1975-04-02T10:00:00Z"
                }}]}}"#,
                french_session.display()
            ),
        )
        .unwrap();

        // Swamp Castle session (newest)
        let swamp_session = swamp.join("huge-tracts.jsonl");
        fs::write(&swamp_session, "{}").unwrap();
        fs::write(
            swamp.join("sessions-index.json"),
            format!(
                r#"{{"entries": [{{
                    "sessionId": "inheritance-planning",
                    "fullPath": "{}",
                    "projectPath": "/Users/dennis/swamp-castle",
                    "summary": "But she's got huge... tracts of land",
                    "modified": "1975-04-03T10:00:00Z"
                }}]}}"#,
                swamp_session.display()
            ),
        )
        .unwrap();

        let sessions = find_sessions(&temp_dir).unwrap();

        // Should have all 3 sessions, sorted by modified (newest first)
        assert_eq!(sessions.len(), 3);
        assert_eq!(sessions[0].id, "inheritance-planning");
        assert_eq!(sessions[0].project, "swamp-castle");
        assert_eq!(sessions[1].id, "taunt-session");
        assert_eq!(sessions[1].project, "castle");
        assert_eq!(sessions[2].id, "round-table-discussion");
        assert_eq!(sessions[2].project, "camelot");

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn find_sessions_handles_missing_optional_fields() {
        // Tim the Enchanter's minimal session - just the required fields
        let temp_dir =
            std::env::temp_dir().join(format!("cc-session-test-tim-{}", std::process::id()));
        let tim_dir = temp_dir.join("-Users-tim-enchanter");
        fs::create_dir_all(&tim_dir).unwrap();

        let session_path = tim_dir.join("fireball.jsonl");
        fs::write(&session_path, "{}").unwrap();

        // Minimal index - no summary, no created/modified, no projectPath
        let index_json = format!(
            r#"{{
                "entries": [{{
                    "sessionId": "big-pointy-teeth",
                    "fullPath": "{}"
                }}]
            }}"#,
            session_path.display()
        );
        fs::write(tim_dir.join("sessions-index.json"), index_json).unwrap();

        let sessions = find_sessions(&temp_dir).unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "big-pointy-teeth");
        assert_eq!(sessions[0].project, "unknown"); // No projectPath falls back to "unknown"
        assert!(sessions[0].summary.is_none());

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn find_sessions_all_files_missing() {
        // All the brave knights' sessions have been eaten by the Legendary Black Beast of Aaaargh
        let temp_dir =
            std::env::temp_dir().join(format!("cc-session-test-beast-{}", std::process::id()));
        let cave_dir = temp_dir.join("-Users-knights-cave");
        fs::create_dir_all(&cave_dir).unwrap();

        // Index references sessions that don't exist
        let index_json = r#"{
            "entries": [
                {
                    "sessionId": "sir-robin",
                    "fullPath": "/eaten/by/beast/robin.jsonl",
                    "summary": "Bravely ran away"
                },
                {
                    "sessionId": "sir-lancelot",
                    "fullPath": "/eaten/by/beast/lancelot.jsonl",
                    "summary": "Got a bit carried away"
                }
            ]
        }"#;
        fs::write(cave_dir.join("sessions-index.json"), index_json).unwrap();

        let sessions = find_sessions(&temp_dir).unwrap();

        // All sessions are gone - eaten by the beast
        assert_eq!(sessions.len(), 0);

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn find_sessions_empty_projects_dir() {
        // The Castle of Aaaargh - completely empty
        let temp_dir =
            std::env::temp_dir().join(format!("cc-session-test-aaaargh-{}", std::process::id()));
        fs::create_dir_all(&temp_dir).unwrap();

        // No project directories at all
        let sessions = find_sessions(&temp_dir).unwrap();
        assert_eq!(sessions.len(), 0);

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn find_sessions_filters_system_prompts() {
        // Brother Maynard's sessions with various first_prompt values
        let temp_dir =
            std::env::temp_dir().join(format!("cc-session-test-maynard-{}", std::process::id()));
        let maynard_dir = temp_dir.join("-Users-maynard-monastery");
        fs::create_dir_all(&maynard_dir).unwrap();

        let session_path = maynard_dir.join("holy-book.jsonl");
        fs::write(&session_path, "{}").unwrap();

        // Test various first_prompt values that should be filtered
        let index_json = format!(
            r#"{{
                "entries": [{{
                    "sessionId": "consult-book",
                    "fullPath": "{}",
                    "projectPath": "/Users/maynard/monastery",
                    "firstPrompt": "/help",
                    "summary": null
                }}]
            }}"#,
            session_path.display()
        );
        fs::write(maynard_dir.join("sessions-index.json"), index_json).unwrap();

        let sessions = find_sessions(&temp_dir).unwrap();

        assert_eq!(sessions.len(), 1);
        // first_message should be None because it started with "/"
        assert!(sessions[0].first_message.is_none());

        fs::remove_dir_all(&temp_dir).unwrap();
    }
}
