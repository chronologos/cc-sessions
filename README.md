# cc-sessions

A fast CLI tool to list and resume [Claude Code](https://claude.ai/code) sessions across all projects.

## Installation

### Pre-built binary (macOS ARM64)

```bash
curl -L https://github.com/chronologos/cc-sessions/releases/latest/download/cc-sessions-macos-arm64 -o ~/bin/cc-sessions
chmod +x ~/bin/cc-sessions
# Ad-hoc sign to avoid Gatekeeper
xattr -cr ~/bin/cc-sessions && codesign -s - ~/bin/cc-sessions
```

### Build from source

Requires Rust 1.85+ (edition 2024).

```bash
cargo build --release
cp target/release/cc-session ~/bin/cc-sessions
# macOS: ad-hoc sign
xattr -cr ~/bin/cc-sessions && codesign -s - ~/bin/cc-sessions
```

## Usage

```bash
cc-sessions              # List 15 most recent sessions
cc-sessions -c 30        # List 30 sessions
cc-sessions -i           # Interactive picker with transcript preview
cc-sessions -f           # Fork a session (creates new session ID)
cc-sessions -i -f        # Interactive mode + fork
cc-sessions -p dotfiles  # Filter by project name (case-insensitive)
cc-sessions --debug      # Show session IDs and stats
```

### List mode

Shows sessions with relative timestamps and AI-generated summaries:

```
CREAT  MOD    PROJECT          SUMMARY
──────────────────────────────────────────────────────────────────────────────────────────
1h     1h     dotfiles         Shell alias structure refactoring
2d     3h     bike-power       Bike Power App: Build 10, Landscape Layout
4h     3h     cc-session       ★ my-session - Claude Code session improvements
```

**Named sessions**: Sessions renamed with `/rename` in Claude Code show a `★` prefix, indicating importance.

### Interactive mode (`-i`)

- **Fuzzy search** through project names and summaries
- **Preview pane** shows conversation transcript with color-coded user (cyan) / assistant (yellow) prefixes
- **Enter** to resume session in the original project directory
- Use `-f` to fork instead of resume (creates new session ID)

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
- **Resume** (`-r`): Continues the existing session
- **Fork** (`-f`): Creates a new session with the conversation history

## License

MIT
