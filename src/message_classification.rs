/// Classification for user-message text when computing session metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Empty,
    SlashCommand,
    CommandTag,
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

    if text.starts_with('<') {
        return MessageKind::CommandTag;
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
/// This intentionally preserves existing behavior:
/// - Excludes slash commands
/// - Excludes XML/system tags
/// - Excludes only [Request...] bracketed system content (not all bracketed text)
pub fn is_first_prompt_candidate(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }

    !text.starts_with('/') && !text.starts_with('<') && !text.starts_with("[Request")
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
                MessageKind::CommandTag,
            ),
            ("[local command output]", MessageKind::BracketedOutput),
            ("", MessageKind::Empty),
        ];

        for (text, expected) in cases {
            assert_eq!(classify_user_text_for_metrics(text), expected);
        }
    }
}
