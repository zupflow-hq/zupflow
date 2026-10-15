//! Slack's message markup, as an event carries it: the people a message
//! mentions. Text is stored as Slack wrote it and rendered for a model,
//! once, when a run takes it in, by
//! [`zupflow_core::channels::thread::plain_text`] with the names of the
//! people the connection knows (each read once with `users.info`) and of
//! the channels it has stored.

/// The people a message mentions, by Slack user ID, in the order written:
/// what the message handler reads, and the people whose names are read.
pub fn mentions(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("<@") {
        let after = &rest[start + 2..];
        let Some(end) = after.find('>') else { break };
        let inner = &after[..end];
        let id = inner.split('|').next().unwrap_or_default();
        if is_id(id) {
            found.push(id.to_owned());
        }
        rest = &after[end + 1..];
    }
    found
}

/// A Slack ID: uppercase letters and digits, as Slack writes them.
fn is_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_mentions_in_order() {
        assert_eq!(
            mentions("<@U02> and <@U01|ana>, <@W03>, again <@U02> <!here> <#C1|x> <@lower>"),
            ["U02", "U01", "W03", "U02"]
        );
        assert!(mentions("no one <@").is_empty());
    }
}
