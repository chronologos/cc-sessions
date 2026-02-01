# cc-sessions

Agent-focused documentation. See README.md for user-facing docs.

## Build & Test

```bash
just build    # Build release binary
just test     # Run tests
just install  # Build and install to ~/.local/bin (includes macOS signing)
just lint     # Run clippy
```

## Architecture

### Code Organization

```
src/
  main.rs         # CLI, display, skim - general/timeless code
  claude_code.rs  # Session loading, JSONL parsing - format-specific
```

**Boundary principle:** If Claude Code changes its storage format, changes should be isolated to `claude_code.rs`. The `Session` struct in `main.rs` acts as an abstraction layer.

| Module | Responsibility | Changes when... |
|--------|---------------|-----------------|
| `main.rs` | CLI args, display formatting, skim integration | UI/UX requirements change |
| `claude_code.rs` | JSONL reading, metadata extraction, path discovery | Claude Code format changes |

### Session Storage Structure

```
~/.claude/projects/
  -Users-you-project-a/
    abc12345-1234-1234-1234-123456789abc.jsonl   # Session transcript (UUID filename)
    def45678-5678-5678-5678-567890123def.jsonl
  -Users-you-project-b/
    ghi78901-9012-9012-9012-901234567890.jsonl
```

Sessions are identified by UUID filenames. Non-UUID files (like `agent-*`) are filtered out.

### Session JSONL Message Types

```mermaid
flowchart LR
    subgraph "Session .jsonl file"
        direction TB
        U1[user] --> A1[assistant]
        A1 --> U2[user]
        U2 --> A2[assistant]
    end

    subgraph "Message Content"
        direction TB
        UM["user.message.content<br/>─────────────────<br/>string | ContentBlock[]"]
        AM["assistant.message.content[]<br/>─────────────────<br/>• type: text<br/>• type: thinking<br/>• type: tool_use"]
    end

    subgraph "Other Entry Types"
        SUM[summary]
        CT[custom-title]
        SYS[system]
    end

    U1 -.-> UM
    A1 -.-> AM
```

## Implementation Details

### Session Discovery

All metadata is extracted directly from `.jsonl` files (no index dependency):

1. **Walk** `~/.claude/projects/*/` for `.jsonl` files
2. **Validate** filename is a UUID (8-4-4-4-12 hex format)
3. **Extract** metadata from file head + tail:
   - **HEAD** (first ~50 lines): `cwd` field → project path, first `user` message
   - **TAIL** (last ~16KB): `summary` type entry, `custom-title` entry
4. **Timestamps** from filesystem (created, modified)
5. **Filter out** empty sessions (no cwd, no user message, no summary)

Uses `rayon` for parallel processing across files.

### UUID Validation

Session filenames must match UUID format: `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`

This filters out:
- `agent-*` files (subagent sessions spawned by Task tool)
- Other non-session files

### Metadata Extraction

| Field | Source | Location |
|-------|--------|----------|
| `project_path` | `cwd` field | File head |
| `first_message` | First `user` entry | File head |
| `summary` | `summary` type entry | File tail (16KB) |
| `name` (customTitle) | `custom-title` entry | Grep entire file |
| `created` | Filesystem | `metadata.created()` |
| `modified` | Filesystem | `metadata.modified()` |

**Why grep for customTitle?** The `/rename` command can be invoked at any point in a session, so `custom-title` entries can appear anywhere in the file, not just the tail.

### Session Names (customTitle)
- Set via `/rename` command in Claude Code
- Stored as `{"type":"custom-title","customTitle":"...","sessionId":"..."}`
- Displayed with `★` prefix: `★ name - summary`
- Indicates user-marked important sessions

### Interactive Mode

Uses embedded [skim](https://github.com/lotabout/skim) crate (no external fzf dependency):

1. Build `SkimOptions` with preview command pointing to self
2. Send `SessionItem`s through crossbeam channel
3. Preview calls `cc-sessions --preview <filepath>` to format transcript
4. On selection, spawns `zsh -c "cd <project> && claude -r <session-id>"`

The `--preview` flag is internal: it reads a `.jsonl` file and outputs color-coded transcript lines.

## Dependencies

- **Build**: Rust 1.85+ (edition 2024)
- **Runtime**: None (skim is embedded, preview is self-contained)

### Key crates

| Crate | Purpose |
|-------|---------|
| `skim` | Embedded fuzzy finder (replaces fzf) |
| `rayon` | Parallel file processing |
| `grep-regex`, `grep-searcher` | Fast pattern matching for customTitle |
| `serde_json` | JSONL parsing |
| `clap` | CLI argument parsing |
