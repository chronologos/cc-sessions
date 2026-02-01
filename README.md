# cc-sessions

A fast CLI tool to list and resume [Claude Code](https://claude.ai/code) sessions across all projects.

![Interactive mode with transcript preview](preview.png)

## Installation

### Pre-built binary (macOS ARM64)

```bash
curl -L https://github.com/chronologos/cc-sessions/releases/latest/download/cc-sessions-macos-arm64 -o ~/bin/cc-sessions
chmod +x ~/bin/cc-sessions
# Ad-hoc sign to avoid Gatekeeper
xattr -cr ~/bin/cc-sessions && codesign -s - ~/bin/cc-sessions
```

### Build from source

Requires Rust 1.85+ (edition 2024) and [just](https://github.com/casey/just).

```bash
just install  # Build and install to ~/.local/bin (includes macOS signing)
```

## Usage

```bash
cc-sessions                      # Interactive picker (default)
cc-sessions --fork               # Fork mode - creates new session ID instead of resuming
cc-sessions --project dotfiles   # Filter by project name (case-insensitive)
cc-sessions --list               # List mode (non-interactive table)
cc-sessions --list --count 30    # List 30 sessions
cc-sessions --list --debug       # List with session IDs and stats
```

### Interactive mode (default)

- **Fuzzy search** through project names and summaries
- **Preview pane** shows conversation transcript with color-coded user (cyan) / assistant (yellow) prefixes
- **ctrl+s** for full-text transcript search (greps all messages, shows matches with context)
- **Enter** to resume session in the original project directory
- **esc** clears search filter, or exits if no filter active
- Use `--fork` to fork instead of resume (creates new session ID)

### List mode (`--list`)

Shows sessions as a table with relative timestamps and AI-generated summaries:

```
CREAT  MOD    PROJECT          SUMMARY
──────────────────────────────────────────────────────────────────────────────────────────
1h     1h     dotfiles         Shell alias structure refactoring
2d     3h     bike-power       Bike Power App: Build 10, Landscape Layout
4h     3h     cc-session       ★ my-session - Claude Code session improvements
```

Sessions renamed with `/rename` in Claude Code show a `★` prefix.

## Features

- **Zero runtime dependencies**: Interactive mode uses embedded [skim](https://github.com/lotabout/skim) (no fzf/jaq needed)
- **Session names**: Shows `★ name` for sessions renamed with `/rename`
- **Direct file scanning**: Reads session metadata directly from `.jsonl` files
- **Parallel processing**: Uses rayon for fast scanning across many projects

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
