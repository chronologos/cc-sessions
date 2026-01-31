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
cc-sessions -i           # Interactive fzf picker with transcript preview
cc-sessions -f           # Fork a session (creates new session ID)
cc-sessions -i -f        # Interactive mode + fork
cc-sessions -p dotfiles  # Filter by project name (case-insensitive)
cc-sessions --debug      # Show session source (indexed/orphan) and stats
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

- **Fuzzy search** through project names, summaries, and transcript metadata
- **Preview pane** shows conversation transcript with color-coded user (cyan) / assistant (yellow) prefixes
- **Hybrid search**: `ctrl-s` for full-text transcript search, `ctrl-n` for normal filter
- **Enter** to resume session in the original project directory
- Use `-f` to fork instead of resume (creates new session ID)

## Features

- **Orphan detection**: Finds sessions not yet indexed by Claude Code (shown as "orphan" in `--debug` mode)
- **Session names**: Shows `★ name` for sessions renamed with `/rename`
- **Stale timestamp fix**: Uses file mtime when newer than index timestamp
- **Parallel processing**: Uses rayon for fast scanning across many projects

## Requirements

- **Runtime**: `fzf` and `jaq` (for interactive mode preview)
  ```bash
  brew install fzf jaq
  ```

## How it works

Claude Code stores session data in `~/.claude/projects/`. This tool:

1. Reads `sessions-index.json` files from each project directory
2. Discovers orphan `.jsonl` files not yet in the index
3. Extracts metadata: session ID, project path, summary, timestamps, custom name
4. Filters out stale entries and empty sessions

When you select a session:
- **Resume** (`-r`): Continues the existing session
- **Fork** (`-f`): Creates a new session with the conversation history

## License

MIT
