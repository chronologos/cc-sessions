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
cc-sessions           # List 15 most recent sessions
cc-sessions -c 30     # List 30 sessions
cc-sessions -i        # Interactive fzf picker with transcript preview
cc-sessions -f        # Fork a session (creates new session ID)
cc-sessions -i -f     # Interactive mode + fork
```

### List mode

Shows sessions with relative timestamps and AI-generated summaries:

```
CREAT  MOD    PROJECT      SUMMARY
──────────────────────────────────────────────────────────────────────────────────────────
1h     1h     dotfiles     Shell alias structure refactoring
2d     3h     bike-power   Bike Power App: Build 10, Landscape Layout
4h     3h     server       Add support for extracting note media
```

### Interactive mode (`-i`)

![fzf picker](https://github.com/user-attachments/assets/placeholder.png)

- **Fuzzy search** through project names, summaries, and transcript metadata
- **Preview pane** shows conversation transcript with 👤 user / 🤖 assistant prefixes
- **Enter** to resume session in the original project directory
- Use `-f` to fork instead of resume (creates new session ID)

## Requirements

- **Runtime**: `fzf` and `jaq` (for interactive mode preview)
  ```bash
  brew install fzf jaq
  ```

## How it works

Claude Code stores session data in `~/.claude/projects/`. This tool:

1. Reads `sessions-index.json` files from each project directory
2. Extracts metadata: session ID, project path, summary, timestamps
3. Filters out stale entries where the session file no longer exists
4. Uses `rayon` for parallel processing across projects

When you select a session:
- **Resume** (`-r`): Continues the existing session
- **Fork** (`-f`): Creates a new session with the conversation history

## License

MIT
