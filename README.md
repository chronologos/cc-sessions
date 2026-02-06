# cc-sessions

Claude Code's `/resume` only shows sessions for your current project, on your current machine. If you work across multiple codebases, finding that session from 3 days ago means remembering which repo you were in.

**cc-sessions fixes this:**

- **Search across machines and projects** — Find any session from anywhere, not just your current directory or machine
- **Preview before resuming** — See the conversation transcript to jog your memory before jumping in
- **Full-text search** — Press `ctrl+s` to grep conversation content ("where did I discuss that API?")

![Interactive mode with transcript preview](preview.png)

## Installation

### Pre-built binaries

```bash
# macOS (Apple Silicon, or Intel via Rosetta)
curl -L https://github.com/chronologos/cc-sessions/releases/latest/download/cc-sessions-macos-arm64 -o ~/.local/bin/cc-sessions
chmod +x ~/.local/bin/cc-sessions
xattr -cr ~/.local/bin/cc-sessions && codesign -s - -f ~/.local/bin/cc-sessions

# Linux x86_64
curl -L https://github.com/chronologos/cc-sessions/releases/latest/download/cc-sessions-linux-x86_64 -o ~/.local/bin/cc-sessions
chmod +x ~/.local/bin/cc-sessions

# Linux ARM64
curl -L https://github.com/chronologos/cc-sessions/releases/latest/download/cc-sessions-linux-arm64 -o ~/.local/bin/cc-sessions
chmod +x ~/.local/bin/cc-sessions
```

### Build from source

Requires Rust 1.85+ (edition 2024) and [just](https://github.com/casey/just).

```bash
just install  # Build and install to ~/.local/bin (macOS signing handled automatically)
```

## Usage

```bash
cc-sessions                      # Interactive picker (default)
cc-sessions --fork               # Fork mode - creates new session ID instead of resuming
cc-sessions --project dotfiles   # Filter by project name (case-insensitive)
cc-sessions --debug              # Show session ID prefixes (works in interactive mode too)
cc-sessions --list               # List mode (non-interactive table)
cc-sessions --list --count 30    # List 30 sessions
cc-sessions --list --debug       # List with session IDs and stats
cc-sessions --list --include-forks  # List mode including forked sessions
```

### Interactive mode (default)

*Preview, transcript search, and fork navigation are **interactive-only**; they are not available in list mode.*

- **Fuzzy search** through project names and summaries
- **Preview pane** shows conversation transcript with color-coded user (cyan) / assistant (yellow) prefixes
- **ctrl+s** for full-text transcript search — replaces view with matching sessions, esc clears
- **Enter** to resume session in the original project directory
- **esc** clears search first, then goes to root view, then exits
- **▶** indicates sessions with forks — press **→** to drill into direct children
- **▷** indicates the focused parent when viewing a subtree
- **←** goes back to the previous view
- Use `--fork` to fork instead of resume (creates new session ID)
- Use `--debug` to show session ID prefixes (useful for debugging)

Column layout: `CRE MOD MSG SOURCE PROJECT SUMMARY` (timestamps, message count, source, project name, summary)

### List mode (`--list`)

Plain table output: no preview, no transcript search, no fork drill-down. Use `--count` and `--include-forks` to control how many sessions and whether forked sessions are shown.

```
CRE  MOD  MSG SOURCE PROJECT      SUMMARY
───────────────────────────────────────────────────────────────────────────────
1h   1h    12 local  dotfiles     Shell alias structure refactoring
2d   3h    45 local  bike-power   Bike Power App: Build 10, Landscape
4h   3h     8 local  cc-session   ★ my-session - Claude Code session...
```

Sessions renamed with `/rename` in Claude Code show a `★` prefix.

### Remote sessions and sync

If you use remote sessions (configured in `~/.config/cc-sessions/remotes.toml`):

- **`--sync`** — Force sync all remotes before listing (otherwise remotes are auto-synced when stale).
- **`--no-sync`** — Skip auto-sync; use cached remote data only.
- **`--sync-only`** — Sync all remotes and exit (no listing or picker). Useful for cron or scripts.

### Forked sessions

Claude Code forks create a separate `.jsonl` file that references the parent via
`forkedFrom.sessionId`. cc-sessions detects this relationship and can display
forks nested under their parent sessions in interactive mode.

## How it works

Claude Code stores session data in `~/.claude/projects/`. This tool:

1. Scans for `.jsonl` files with valid UUID filenames
2. Extracts metadata directly from file contents (cwd, first message, summary, custom title)
3. Uses filesystem timestamps for accurate sorting
4. Filters out empty sessions and non-session files

When you select a session:

- **Resume** (default): Continues the existing session
- **Fork** (`--fork`): Creates a new session with the conversation history

## License

MIT
