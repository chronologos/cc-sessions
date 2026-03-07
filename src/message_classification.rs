/// Tag prefixes the CLI writes into user-message content that are never
/// real user input (slash command expansion, bash mode, IDE context, hooks).
/// Matched as prefixes so `<command-name>foo</command-name>` etc. are caught.
const SYSTEM_TAG_PREFIXES: &[&str] = &[
    "<command-",       // <command-name>, <command-message>, <command-args>
    "<local-command-", // <local-command-stdout>, <local-command-caveat>
    "<bash-",          // <bash-input>, <bash-stdout>, <bash-stderr>
    "<ide_",           // <ide_selection>, <ide_opened_file>, <ide_diagnostics>
    "<session-start-hook>",
    "<system-reminder>",
    "<tick>",
    "<goal>",
    "<teammate-message>",
    "<task-notification>",
    "<mcp-", // <mcp-resource>, <mcp-resource-update>, <mcp-polling-update>
    "<ultraplan-mode>",
];

pub fn starts_with_system_tag(text: &str) -> bool {
    SYSTEM_TAG_PREFIXES.iter().any(|p| text.starts_with(p))
}

/// Whether a user-message text payload should be hidden in transcript previews.
///
/// Narrower than `classify_user_text_for_metrics` — only hides content that is
/// definitively system-generated, never content a user might plausibly type.
pub fn is_system_content_for_preview(text: &str) -> bool {
    starts_with_system_tag(text) || text.starts_with("[Request") || text.starts_with('/')
}

/// Classification for user-message text when computing session metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Empty,
    SlashCommand,
    SystemTag,
    BracketedOutput,
    UserContent,
}

/// Classify a user text payload for turn-count metrics.
pub fn classify_user_text_for_metrics(text: &str) -> MessageKind {
    if text.is_empty() {
        return MessageKind::Empty;
    }

    if text.starts_with('/') {
        return MessageKind::SlashCommand;
    }

    if starts_with_system_tag(text) {
        return MessageKind::SystemTag;
    }

    if text.starts_with('[') {
        return MessageKind::BracketedOutput;
    }

    MessageKind::UserContent
}

/// Whether a user text should count as a conversation turn.
pub fn counts_as_turn(text: &str) -> bool {
    classify_user_text_for_metrics(text) == MessageKind::UserContent
}

/// Whether a user text should be used as first prompt summary candidate.
///
/// - Excludes slash commands
/// - Excludes known system-generated tag prefixes (but NOT arbitrary `<text>`)
/// - Excludes `[Request interrupted...]` system messages (but NOT arbitrary brackets)
pub fn is_first_prompt_candidate(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with('/')
        && !starts_with_system_tag(text)
        && !text.starts_with("[Request")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_user_text_for_metrics_table() {
        let cases = [
            ("normal user text", MessageKind::UserContent),
            ("/help", MessageKind::SlashCommand),
            (
                "<command-message>init</command-message>",
                MessageKind::SystemTag,
            ),
            (
                "<local-command-stdout>out</local-command-stdout>",
                MessageKind::SystemTag,
            ),
            ("<bash-input>ls</bash-input>", MessageKind::SystemTag),
            ("<tick>", MessageKind::SystemTag),
            ("[local command output]", MessageKind::BracketedOutput),
            ("", MessageKind::Empty),
            // Real user input starting with < should NOT be classified as system
            ("<Button> component is broken", MessageKind::UserContent),
            ("<div class='x'>", MessageKind::UserContent),
            ("< 5 seconds", MessageKind::UserContent),
        ];

        for (text, expected) in cases {
            assert_eq!(
                classify_user_text_for_metrics(text),
                expected,
                "input: {text:?}"
            );
        }
    }

    #[test]
    fn is_first_prompt_candidate_accepts_angle_bracket_user_text() {
        assert!(is_first_prompt_candidate("<Button> is broken"));
        assert!(is_first_prompt_candidate("<?xml version='1.0'?>"));
        assert!(!is_first_prompt_candidate(
            "<command-name>/init</command-name>"
        ));
        assert!(!is_first_prompt_candidate("<tick>"));
        assert!(!is_first_prompt_candidate("[Request interrupted by user]"));
        assert!(is_first_prompt_candidate("[not a request interrupt]"));
    }

    #[test]
    fn is_system_content_for_preview_narrow_filter() {
        // System-generated content: hide
        assert!(is_system_content_for_preview(
            "<command-message>init</command-message>"
        ));
        assert!(is_system_content_for_preview("<tick>"));
        assert!(is_system_content_for_preview(
            "[Request interrupted by user]"
        ));
        assert!(is_system_content_for_preview("/help"));

        // Plausible user input: show
        assert!(!is_system_content_for_preview("<Button> is broken"));
        assert!(!is_system_content_for_preview("<div class='x'>"));
        assert!(!is_system_content_for_preview("[not a request interrupt]"));
    }
}
