//! Quoting for the one place this binary builds a command line as text: the
//! `agent start` line a pane runs, and the same line as `context` prints it.
//! Everywhere else a command is argv, not a string.

/// Single-quote escaping: safe for any value in an `sh` command string. Plain
/// words are left bare so printed commands stay readable and stable.
pub fn quote(value: &str) -> String {
    if is_plain(value) {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

/// Only characters that no shell treats specially.
fn is_plain(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':' | '@' | '+' | ','))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_values_stay_bare_and_everything_else_is_single_quoted() {
        assert_eq!(quote("/usr/local/bin/hp"), "/usr/local/bin/hp");
        assert_eq!(quote("--root"), "'--root'");
        assert_eq!(quote("/tmp/my dir"), "'/tmp/my dir'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote("$(touch /tmp/pwned)"), "'$(touch /tmp/pwned)'");
        assert_eq!(quote(""), "''");
    }
}
