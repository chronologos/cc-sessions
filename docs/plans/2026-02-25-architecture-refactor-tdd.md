# Architecture Refactor (TDD) Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Refactor cc-sessions architecture to improve reliability, maintainability, and performance for findings 1-5 while preserving existing behavior.

**Architecture:** Introduce explicit health/status reporting for sync/discovery, isolate domain model from CLI orchestration, centralize message classification semantics, extract interactive navigation/search state into testable units, and reduce repeated file scans by introducing reusable metadata/search plumbing.

**Tech Stack:** Rust 2024, anyhow, clap, skim, rayon, serde_json, grep-regex, grep-searcher

---

### Task 1: Characterization Test Baseline (Safety Net)

**Files:**
- Modify: `src/main.rs`
- Modify: `src/claude_code.rs`
- Test: `src/main.rs` test module
- Test: `src/claude_code.rs` test module

**Step 1: Write the failing test**

Add characterization tests that lock in current user-visible behavior before refactors:
- "search results replace subtree view until Esc"
- "list mode excludes forks by default"
- "first prompt extraction and turn counting current behavior"

**Step 2: Run test to verify it fails**

Run: `cargo test search_results_replace_subtree_until_esc`
Expected: FAIL (missing test helper/state abstraction)

**Step 3: Write minimal implementation**

Create tiny pure helpers as needed to make tests possible without changing behavior.

**Step 4: Run test to verify it passes**

Run: `cargo test search_results_replace_subtree_until_esc`
Expected: PASS

**Step 5: Commit**

```bash
git add src/main.rs src/claude_code.rs
git commit -m "test: add characterization coverage for architecture refactor"
```

---

### Task 2: Address Finding 1 - Explicit Sync/Discovery Health Reporting

**Files:**
- Modify: `src/remote.rs`
- Modify: `src/claude_code.rs`
- Modify: `src/main.rs`
- Test: `src/remote.rs` test module
- Test: `src/main.rs` test module

**Step 1: Write the failing test**

Add tests for new reporting types:
- `SyncSummary` includes `successful`, `failed`, and failure reasons.
- `DiscoverySummary` includes source load failures and total sessions loaded.
- `--strict` mode returns error when any remote fails.

**Step 2: Run test to verify it fails**

Run: `cargo test strict_mode_fails_when_any_remote_sync_fails`
Expected: FAIL (flag/type/behavior not implemented)

**Step 3: Write minimal implementation**

- Add `--strict` CLI flag in `Args`.
- Change remote sync orchestration to return structured results (not just stderr warnings).
- Propagate source-level failures from discovery and print concise summary in CLI.
- In strict mode, `bail!` on any failed remote sync/discovery source.

**Step 4: Run test to verify it passes**

Run: `cargo test strict_mode_fails_when_any_remote_sync_fails`
Expected: PASS

**Step 5: Commit**

```bash
git add src/main.rs src/remote.rs src/claude_code.rs
git commit -m "feat: add sync/discovery health summaries and strict mode"
```

---

### Task 3: Address Finding 3 - Extract Domain Model from `main.rs`

**Files:**
- Create: `src/session.rs`
- Modify: `src/main.rs`
- Modify: `src/claude_code.rs`
- Test: `src/main.rs` test module (imports updated)

**Step 1: Write the failing test**

Add a compile-time module wiring test (or existing tests adapted) that uses `Session` from `crate::session` and verifies source display behavior remains unchanged.

**Step 2: Run test to verify it fails**

Run: `cargo test session_source_display_name_local`
Expected: FAIL (module does not exist)

**Step 3: Write minimal implementation**

- Move `Session` and `SessionSource` into `src/session.rs`.
- Re-export via `mod session;` and `use crate::session::{Session, SessionSource};`.
- Keep fields and behavior identical.

**Step 4: Run test to verify it passes**

Run: `cargo test session_source_display_name_local`
Expected: PASS

**Step 5: Commit**

```bash
git add src/main.rs src/claude_code.rs src/session.rs
git commit -m "refactor: extract session domain model into dedicated module"
```

---

### Task 4: Address Finding 4 - Centralize Message Classification Rules

**Files:**
- Create: `src/message_classification.rs`
- Modify: `src/claude_code.rs`
- Modify: `src/main.rs`
- Test: `src/message_classification.rs` (new test module)
- Test: `src/claude_code.rs` test module

**Step 1: Write the failing test**

Add table-driven tests for one shared function, e.g. `classify_user_text_for_metrics(&str) -> MessageKind`:
- counts as user turn
- excluded as slash command
- excluded as command XML
- excluded as bracketed local output

**Step 2: Run test to verify it fails**

Run: `cargo test classify_user_text_for_metrics_table`
Expected: FAIL (function/module missing)

**Step 3: Write minimal implementation**

- Implement classifier in one module.
- Replace inline checks in `read_file_head` and `count_turns` with classifier calls.
- Keep behavior backward-compatible unless test explicitly codifies intentional change.

**Step 4: Run test to verify it passes**

Run: `cargo test classify_user_text_for_metrics_table`
Expected: PASS

**Step 5: Commit**

```bash
git add src/message_classification.rs src/claude_code.rs src/main.rs
git commit -m "refactor: centralize message classification semantics"
```

---

### Task 5: Address Finding 5 - Extract Interactive State Machine

**Files:**
- Create: `src/interactive_state.rs`
- Modify: `src/main.rs`
- Test: `src/interactive_state.rs` (new test module)
- Test: `src/main.rs` test module

**Step 1: Write the failing test**

Add pure state transition tests:
- Esc in search mode clears search first.
- Esc in focused subtree clears focus next.
- Right arrow only drills into nodes with children.
- Ctrl+S with empty query is no-op.

**Step 2: Run test to verify it fails**

Run: `cargo test esc_priority_search_then_focus_then_exit`
Expected: FAIL (state reducer not implemented)

**Step 3: Write minimal implementation**

- Add `InteractiveState` + `Action` + reducer-style transition function.
- Keep skim integration in `main.rs`, but delegate state transitions and view selection.
- Preserve output formatting and keybindings.

**Step 4: Run test to verify it passes**

Run: `cargo test esc_priority_search_then_focus_then_exit`
Expected: PASS

**Step 5: Commit**

```bash
git add src/main.rs src/interactive_state.rs
git commit -m "refactor: extract interactive navigation/search state machine"
```

---

### Task 6: Address Finding 2 - Reduce Repeated I/O and Search Rescans

**Files:**
- Modify: `src/claude_code.rs`
- Create: `src/session_scan.rs` (or keep internal to `claude_code.rs` if smaller)
- Modify: `src/main.rs`
- Test: `src/claude_code.rs` test module

**Step 1: Write the failing test**

Add tests for reusable scan plumbing:
- Single parser pass can produce head metadata + turn count + optional summary.
- Search can reuse precomputed normalized text when available.
- Behavior remains identical to current extraction for representative fixtures.

**Step 2: Run test to verify it fails**

Run: `cargo test scan_once_produces_equivalent_session_metadata`
Expected: FAIL (single-pass API missing)

**Step 3: Write minimal implementation**

- Introduce `SessionScan` struct from one streaming read where feasible.
- Retain tail read or grep where required, but eliminate avoidable duplicate full-file scans.
- Add optional in-memory search text cache for active interactive session set.

**Step 4: Run test to verify it passes**

Run: `cargo test scan_once_produces_equivalent_session_metadata`
Expected: PASS

**Step 5: Commit**

```bash
git add src/claude_code.rs src/main.rs src/session_scan.rs
git commit -m "perf: reduce repeated session file scans and search rescans"
```

---

### Task 7: Refactor Polish + Regression Verification

**Files:**
- Modify: `README.md`
- Modify: `CLAUDE.md`
- Modify: `docs/plans/2026-02-25-architecture-refactor-tdd.md` (checklist updates)

**Step 1: Write the failing test**

N/A (documentation + verification task)

**Step 2: Run test to verify it fails**

N/A

**Step 3: Write minimal implementation**

- Update docs with new strict mode and health reporting output.
- Document architecture module split (`session`, `interactive_state`, classification).

**Step 4: Run test to verify it passes**

Run:
- `just test`
- `just lint`
- `just build`

Expected:
- All tests pass.
- No clippy errors.
- Build succeeds.

**Step 5: Commit**

```bash
git add README.md CLAUDE.md src docs/plans/2026-02-25-architecture-refactor-tdd.md
git commit -m "docs: capture refactored architecture and strict health behavior"
```

---

## TDD Guardrails For Every Task

- Never write production code before a failing test.
- Verify failure reason is correct (missing behavior, not typo/setup).
- Implement minimum change for green.
- Run focused test first, then full suite.
- Refactor only while tests remain green.

## Suggested Execution Order

1. Task 1 (baseline characterization)
2. Task 2 (health reporting/strict mode)
3. Task 3 (model extraction)
4. Task 4 (classification centralization)
5. Task 5 (interactive state extraction)
6. Task 6 (I/O/search performance)
7. Task 7 (docs + full verification)

Plan complete and saved to `docs/plans/2026-02-25-architecture-refactor-tdd.md`. Two execution options:

1. Subagent-Driven (this session) - I dispatch fresh subagent per task, review between tasks, fast iteration
2. Parallel Session (separate) - Open new session with executing-plans, batch execution with checkpoints

Which approach?

## Execution Status

- [x] Task 1: Characterization Test Baseline
- [x] Task 2: Explicit Sync/Discovery Health Reporting
- [x] Task 3: Extract Domain Model from `main.rs`
- [x] Task 4: Centralize Message Classification Rules
- [x] Task 5: Extract Interactive State Machine
- [x] Task 6: Reduce Repeated I/O and Search Rescans
- [x] Task 7: Refactor Polish + Regression Verification
