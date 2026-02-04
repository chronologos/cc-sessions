# Forked Sessions Implementation Plan
 
> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.
 
**Goal:** Add fork relationship awareness and interactive fork expansion in cc-sessions, with documentation and tests.
**Architecture:** Extract `forked_from` from JSONL head, build parent→children map after load, and render a hierarchical view in interactive mode with expand/collapse. Keep list mode flat with indicators and add an `--include-forks` flag for non-interactive output.
**Tech Stack:** Rust 2024, skim, serde_json, grep crates, clap
---
 
### Task 1: Document fork structure (Phase 0)
 
**Files:**
- Modify: `README.md`
- Modify: `CLAUDE.md`
 
**Step 1: Write the failing documentation test**
 
N/A (documentation-only task)
 
**Step 2: Run test to verify it fails**
 
N/A
 
**Step 3: Write minimal documentation**
 
- Add a "Forked Sessions" section describing `forkedFrom.sessionId` and parent/child relationship.
- Mention that forks are separate `.jsonl` files, and custom titles may include "(Fork)".
 
**Step 4: Verify**
 
Manually review the docs for accuracy and clarity.
 
**Step 5: Commit**
 
```bash
git add README.md CLAUDE.md
git commit -m "docs: document forked session structure"
```
 
---
 
### Task 2: Add/adjust fork extraction tests (Phase 1)
 
**Files:**
- Modify: `src/claude_code.rs`
 
**Step 1: Write the failing test**
 
```rust
#[test]
fn read_file_head_prefers_first_forked_from() {
    let temp_dir = std::env::temp_dir()
        .join(format!("cc-session-test-fork-order-{}", std::process::id()));
    fs::create_dir_all(&temp_dir).unwrap();

    let session_path = temp_dir.join("test.jsonl");
    fs::write(
        &session_path,
        r#"{"type":"user","message":{"role":"user","content":"hello"},"forkedFrom":{"sessionId":"parent-1","messageUuid":"m1"}}
{"type":"assistant","message":"hi"}
{"type":"user","message":{"role":"user","content":"later"},"forkedFrom":{"sessionId":"parent-2","messageUuid":"m2"}}"#,
    )
    .unwrap();

    let head = read_file_head(&session_path);
    assert_eq!(head.forked_from, Some("parent-1".to_string()));

    fs::remove_dir_all(&temp_dir).unwrap();
}
```
 
**Step 2: Run test to verify it fails**
 
Run: `cargo test read_file_head_prefers_first_forked_from`
Expected: FAIL (if logic picks later fork)
 
**Step 3: Write minimal implementation**
 
Ensure `read_file_head` captures the first `forkedFrom.sessionId` only.
 
**Step 4: Run test to verify it passes**
 
Run: `cargo test read_file_head_prefers_first_forked_from`
Expected: PASS
 
**Step 5: Commit**
 
```bash
git add src/claude_code.rs
git commit -m "test: cover forkedFrom extraction order"
```
 
---
 
### Task 3: Add fork include flag for list mode (Phase 1)
 
**Files:**
- Modify: `src/main.rs`
 
**Step 1: Write the failing test**
 
```rust
#[test]
fn list_mode_excludes_forks_by_default() {
    let sessions = vec![
        Session { id: "parent".into(), forked_from: None, ..test_session() },
        Session { id: "fork".into(), forked_from: Some("parent".into()), ..test_session() },
    ];

    let visible = filter_forks_for_list(&sessions, false);
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].id, "parent");
}
```
 
**Step 2: Run test to verify it fails**
 
Run: `cargo test list_mode_excludes_forks_by_default`
Expected: FAIL (function missing)
 
**Step 3: Write minimal implementation**
 
- Add `--include-forks` flag
- Add `filter_forks_for_list` helper to return either all sessions or parents only
 
**Step 4: Run test to verify it passes**
 
Run: `cargo test list_mode_excludes_forks_by_default`
Expected: PASS
 
**Step 5: Commit**
 
```bash
git add src/main.rs
git commit -m "feat: add include-forks flag for list mode"
```
 
---
 
### Task 4: Interactive expand/collapse fork tree (Phase 2)
 
**Files:**
- Modify: `src/main.rs`
 
**Step 1: Write the failing test**
 
```rust
#[test]
fn expand_collapse_controls_visible_sessions() {
    let parent = Session { id: "parent".into(), forked_from: None, ..test_session() };
    let fork = Session { id: "fork".into(), forked_from: Some("parent".into()), ..test_session() };
    let sessions = vec![&parent, &fork];

    let mut expanded = std::collections::HashSet::new();
    let children_map = build_fork_tree(&sessions);
    let visible = build_tree_view(&sessions, &children_map, &expanded);
    assert_eq!(visible.len(), 1);

    expanded.insert("parent".to_string());
    let visible = build_tree_view(&sessions, &children_map, &expanded);
    assert_eq!(visible.len(), 2);
}
```
 
**Step 2: Run test to verify it fails**
 
Run: `cargo test expand_collapse_controls_visible_sessions`
Expected: FAIL (tree view missing)
 
**Step 3: Write minimal implementation**
 
- Add an `expanded_parents` set tracked in `interactive_mode`
- Add keybinds for right/left arrow to expand/collapse
- Build a `build_tree_view` helper to return flattened rows with indentation and tree markers (`└─`, `├─`)
 
**Step 4: Run test to verify it passes**
 
Run: `cargo test expand_collapse_controls_visible_sessions`
Expected: PASS
 
**Step 5: Commit**
 
```bash
git add src/main.rs
git commit -m "feat: interactive fork tree expand/collapse"
```
