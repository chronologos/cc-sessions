# cc-session

A fast CLI tool to list and resume Claude Code sessions across all projects.

## Build & Install

```bash
cargo build --release
cp target/release/cc-session ~/bin/cc-sessions
# macOS: ad-hoc sign to avoid Gatekeeper killing unsigned binaries
xattr -cr ~/bin/cc-sessions && codesign -s - ~/bin/cc-sessions
```

## Usage

```bash
cc-sessions           # List 15 most recent sessions
cc-sessions -c 30     # List 30 sessions
cc-sessions -i        # Interactive fzf picker with transcript preview
```

## Architecture

### Session Discovery
- Reads `sessions-index.json` files from `~/.claude/projects/*/`
- Each index contains session metadata: id, projectPath, firstPrompt, created, modified
- Filters out sessions where the `.jsonl` file no longer exists
- Uses `rayon` for parallel processing of index files

### Data Source
Claude Code maintains `sessions-index.json` in each project directory with:
- `projectPath`: Actual filesystem path (e.g., `/Users/iantay/Documents/repos/bike-power`)
- `firstPrompt`: First user message (already extracted)
- `created`/`modified`: ISO 8601 timestamps

Note: The index may contain stale entries for deleted sessions. We filter these by checking if `fullPath` exists.

### Interactive Mode
- Pipes session list to fzf with tab-delimited fields
- Preview runs `jaq` to extract conversation transcript from `.jsonl` files
- On selection, spawns `zsh -c "cd <project> && claude -r <session-id>"`

## Dependencies

- **Runtime**: `fzf` and `jaq` (for interactive mode preview)
- **Build**: Rust 1.85+ (edition 2024), rayon for parallelism

## Output Format

List mode shows relative times:
```
CREATED  MODIFIED PROJECT      FIRST MESSAGE
─────────────────────────────────────────────────────────────────────────────────────
2h       now      dotfiles     Execute the Claude Code Docs helper script...
1d       4h       server       add support for extracting note media
```

Interactive mode shows transcript preview with 👤 user / 🤖 assistant prefixes.
