use std::collections::HashSet;

#[derive(Debug, Default)]
pub struct InteractiveState {
    search_pattern: Option<String>,
    search_results: Option<HashSet<String>>,
    focus_stack: Vec<String>,
}

#[derive(Debug)]
pub enum Action {
    Esc,
    CtrlS { query: String },
    ApplySearchResults { pattern: String, matched_ids: HashSet<String> },
    Right {
        selected_id: Option<String>,
        has_children: bool,
    },
    Left,
    Enter { selected_id: Option<String> },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    Continue,
    Exit,
    RunSearch { pattern: String },
    Select { session_id: String },
}

impl InteractiveState {
    pub fn search_pattern(&self) -> Option<&String> {
        self.search_pattern.as_ref()
    }

    pub fn search_results(&self) -> Option<&HashSet<String>> {
        self.search_results.as_ref()
    }

    pub fn focus(&self) -> Option<&String> {
        self.focus_stack.last()
    }

    pub fn push_focus_for_test(&mut self, id: &str) {
        self.focus_stack.push(id.to_string());
    }

    pub fn apply(&mut self, action: Action) -> Effect {
        match action {
            Action::Esc => {
                if self.search_results.is_some() {
                    self.search_results = None;
                    self.search_pattern = None;
                    return Effect::Continue;
                }

                if !self.focus_stack.is_empty() {
                    self.focus_stack.clear();
                    return Effect::Continue;
                }

                Effect::Exit
            }
            Action::CtrlS { query } => {
                let query = query.trim();
                if query.is_empty() {
                    return Effect::Continue;
                }
                Effect::RunSearch {
                    pattern: query.to_string(),
                }
            }
            Action::ApplySearchResults {
                pattern,
                matched_ids,
            } => {
                self.search_pattern = Some(pattern);
                self.search_results = Some(matched_ids);
                Effect::Continue
            }
            Action::Right {
                selected_id,
                has_children,
            } => {
                let Some(selected_id) = selected_id else {
                    return Effect::Continue;
                };

                let already_focused = self.focus_stack.last().map(|f| f == &selected_id).unwrap_or(false);
                if has_children && !already_focused {
                    self.focus_stack.push(selected_id);
                }
                Effect::Continue
            }
            Action::Left => {
                self.focus_stack.pop();
                Effect::Continue
            }
            Action::Enter { selected_id } => {
                let Some(session_id) = selected_id else {
                    return Effect::Continue;
                };
                Effect::Select { session_id }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esc_priority_search_then_focus_then_exit() {
        let mut state = InteractiveState::default();
        state.push_focus_for_test("root");

        let mut matched = HashSet::new();
        matched.insert("a".to_string());
        assert_eq!(
            state.apply(Action::ApplySearchResults {
                pattern: "api".to_string(),
                matched_ids: matched,
            }),
            Effect::Continue
        );

        assert_eq!(state.apply(Action::Esc), Effect::Continue);
        assert!(state.search_results().is_none());
        assert!(state.search_pattern().is_none());
        assert!(state.focus().is_some());

        assert_eq!(state.apply(Action::Esc), Effect::Continue);
        assert!(state.focus().is_none());

        assert_eq!(state.apply(Action::Esc), Effect::Exit);
    }

    #[test]
    fn right_arrow_only_drills_when_has_children() {
        let mut state = InteractiveState::default();

        assert_eq!(
            state.apply(Action::Right {
                selected_id: Some("leaf".to_string()),
                has_children: false,
            }),
            Effect::Continue
        );
        assert!(state.focus().is_none());

        assert_eq!(
            state.apply(Action::Right {
                selected_id: Some("parent".to_string()),
                has_children: true,
            }),
            Effect::Continue
        );
        assert_eq!(state.focus().map(String::as_str), Some("parent"));
    }

    #[test]
    fn ctrl_s_empty_query_is_noop() {
        let mut state = InteractiveState::default();
        assert_eq!(
            state.apply(Action::CtrlS {
                query: "   ".to_string()
            }),
            Effect::Continue
        );
        assert!(state.search_pattern().is_none());
        assert!(state.search_results().is_none());
    }
}
