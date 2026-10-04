//! A thread stuck on a failed retry: its agent gave up mid-turn, the pane
//! sits idle with the failure still on its screen, and no report is coming.
//! Nothing else would ever reach the coordinator about it, so the ticker
//! reads idle panes for these lines and reports what it sees.

/// Lowercase fragments of the lines harnesses print when they stop retrying.
/// Matched against plain screen text, so only harnesses' own final words fit;
/// extend the list as other harnesses are seen to give up.
const PHRASES: &[&str] = &[
    // pi
    "retry failed after",
];

/// The first screen line carrying a failed retry, with escape sequences
/// dropped, or `None`. The line itself is the report's signature: the same
/// failure reports once, a different one reports again.
pub fn retry_failure(screen: &str) -> Option<String> {
    crate::trust_screen::plain_text(screen)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .find(|line| PHRASES.iter().any(|phrase| line.to_lowercase().contains(phrase)))
        .map(|line| line.chars().take(200).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sees_a_failed_retry_through_styling() {
        let screen = "some output\n\u{1b}[1mError:\u{1b}[0m Retry failed after 3 attempts: terminated\n\n❯ prompt";
        assert_eq!(retry_failure(screen).as_deref(), Some("Error: Retry failed after 3 attempts: terminated"));
    }

    #[test]
    fn takes_the_first_failure_line_and_caps_its_length() {
        let long = "Error: Retry failed after 5 attempts: ".repeat(10);
        let screen = format!("noise\n{long}\nError: Retry failed after 7 attempts: later\n");
        let seen = retry_failure(&screen).unwrap();
        assert!(seen.len() <= 200, "{seen}");
        assert!(seen.starts_with("Error: Retry failed after 5 attempts:"), "{seen}");
    }

    #[test]
    fn ordinary_errors_and_clean_screens_are_not_failures() {
        assert_eq!(retry_failure("Error: file not found\n"), None);
        assert_eq!(retry_failure(""), None);
        assert_eq!(retry_failure("retried once, then it worked\n"), None);
    }
}
