//! Is an agent's input box empty? Read from the pane's screen (`hc read`), so
//! it works for every harness without hooks: each supported kind has its own
//! anchor for the box, and a known placeholder or a blank row does not count as
//! typed. `Empty` is only claimed when the box is on the screen and provably
//! blank; a box this binary cannot place is `Unknown`, and the ticker's own
//! sends (`steps`, `brief`, `hp-agent`) treat both `Typed` and `Unknown` as
//! "not empty", so a draft is never typed over. `hp thread prompt` is the
//! exception: it is a person's explicit instruction and still types into a
//! screen it cannot read, as before.
//!
//! The screen arrives as **plain text**. `hc --json read <pane>` answers
//! `{"complete":true,"how":"screen","text":"…"}` and that `text` carries no
//! escape sequence at all, so nothing here may depend on how a cell is drawn:
//! dim, reverse video and background colour are decided by the harness and are
//! not in the read. herdr-core's own screen model does keep the attributes
//! (its formatter can print VT, which is how `tests/fixtures/prompt_box/*.ansi`
//! were captured), but `terminal.read` prints the text only; a styled read
//! would be a new core API and a new core release, not something this binary
//! can ask for.
//!
//! So the box is found by glyphs and judged by text: `placeholders` covers the
//! fixed phrase a harness draws in an empty box, [`dashes`] the border that says
//! how wide an opencode box is, and the prompt glyph (`❯`, `›`, `→`, `│ >`) the
//! row that holds the typed text. The one thing styling would have settled is
//! Claude Code's rotating `Try "…"` suggestion, which is read by shape instead.

/// What the input box holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Draft {
    Empty,
    /// Someone typed text that was not submitted.
    Typed,
    /// No input box was found on the screen (a menu or dialog, a scrolled
    /// view, a layout this binary does not know, an unsupported kind), or one
    /// whose contents it cannot read. No caller treats this as empty.
    Unknown,
}

/// Placeholders a harness draws in an empty box, by kind. A prefix: opencode
/// rotates a suggestion in after its fixed phrase.
fn placeholders(kind: &str) -> &'static [&'static str] {
    match kind {
        "codex" => &["Ask Codex to do anything"],
        "cursor" => &["Plan, search, build anything"],
        "gemini" => &["Type your message or @path/to/file"],
        "opencode" => &["Ask anything…"],
        _ => &[],
    }
}

/// Claude Code rotates a suggestion into its empty box: `Try "fix lint"`,
/// `Try "write a test for <filepath>"`. Shape only — a draft that is exactly one
/// quoted `Try "…"` and nothing else reads as the suggestion. Styling would have
/// told them apart (the suggestion is drawn dim); the text cannot.
fn is_claude_suggestion(text: &str) -> bool {
    text.starts_with("Try \"") && text.ends_with('"') && text.chars().count() > "Try \"\"".len()
}

/// Whether a box holding exactly this one line is empty: a placeholder, or one
/// of their families.
fn is_placeholder(kind: &str, text: &str) -> bool {
    (kind == "claude" && is_claude_suggestion(text)) || placeholders(kind).iter().any(|p| text.starts_with(p))
}

type Line = Vec<char>;

fn text(line: &[char]) -> String {
    line.iter().collect()
}

/// The screen as lines of characters, escape sequences dropped. `hc read`
/// prints none today; this keeps the captured fixtures readable.
fn strip(screen: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut line = Line::new();
    let mut chars = screen.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\u{1b}' => match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\n' => lines.push(std::mem::take(&mut line)),
            '\r' => {}
            c if c.is_control() => {}
            c => line.push(c),
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// The characters after the first `glyph` on the line (the anchor itself
/// dropped).
fn after(line: &[char], glyph: char) -> Option<Line> {
    let at = line.iter().position(|c| *c == glyph)?;
    Some(line[at + 1..].to_vec())
}

fn trimmed_starts(line: &[char], prefix: &str) -> bool {
    text(line).trim_start().starts_with(prefix)
}

/// A horizontal rule: a line of box-drawing `─` only.
fn is_rule(line: &[char]) -> bool {
    let t = text(line);
    let t = t.trim();
    t.chars().count() >= 10 && t.chars().all(|c| c == '─')
}

/// The top border of Claude Code's input box: a rule that may carry one label.
///
///   - The session name, an agent name or a tmux hint: `──── label ─`.
///   - Effort and fast-mode tags, right-aligned: `──── ultracode  fast mode ─`.
///   - The history position, left-aligned: `─── History 3/12 ────`.
///   - The bottom border never carries a label, so only the top is read this way.
fn is_labelled_rule(line: &[char]) -> bool {
    let t = text(line);
    let t = t.trim();
    let dashes = t.chars().filter(|c| *c == '─').count();
    dashes >= 10 && t.starts_with("──") && t.split('─').filter(|s| !s.trim().is_empty()).count() <= 1
}

/// The input box's lines, or `None` when it is not on the screen.
fn input_box(kind: &str, lines: &[Line]) -> Option<Vec<Line>> {
    let last = |pred: &dyn Fn(usize, &[char]) -> bool| lines.iter().enumerate().rev().find(|(i, l)| pred(*i, l)).map(|(i, _)| i);
    match kind {
        // `❯` right under the top border, continued until the next rule.
        "claude" => {
            let at = last(&|i, l| trimmed_starts(l, "❯") && i > 0 && is_labelled_rule(&lines[i - 1]))?;
            let end = (at + 1..lines.len()).find(|&i| is_rule(&lines[i]))?;
            let mut rows = vec![after(&lines[at], '❯')?];
            rows.extend(lines[at + 1..end].iter().cloned());
            Some(rows)
        }
        "codex" => last(&|_, l| trimmed_starts(l, "›")).and_then(|at| after(&lines[at], '›')).map(|row| vec![row]),
        "cursor" => last(&|_, l| trimmed_starts(l, "→")).and_then(|at| after(&lines[at], '→')).map(|row| vec![row]),
        // `│ > ` (or `!` shell mode, `*` yolo mode) in a bordered box.
        "gemini" => {
            let at = last(&|_, l| {
                let t = text(l);
                let t = t.trim_start();
                t.starts_with('│') && matches!(t['│'.len_utf8()..].trim_start().chars().next(), Some('>' | '!' | '*'))
            })?;
            let end = (at + 1..lines.len()).find(|&i| trimmed_starts(&lines[i], "╰"))?;
            let mut rows = Vec::new();
            for (n, line) in lines[at..end].iter().enumerate() {
                let mut row = after(line, '│')?;
                if let Some(close) = row.iter().rposition(|c| *c == '│') {
                    row.truncate(close);
                }
                if n == 0 {
                    let prompt = row.iter().position(|c| matches!(c, '>' | '!' | '*'))?;
                    row.drain(..=prompt);
                }
                rows.push(row);
            }
            Some(rows)
        }
        // The `┃` lines above the box's `╹▀▀▀` bottom, less the last one (the
        // mode and model line). The bottom border is as wide as the box, so it
        // also says where the box ends: a sidebar drawn beside it stays out.
        "opencode" => {
            let bottom = last(&|_, l| trimmed_starts(l, "╹"))?;
            let top = (0..bottom).rev().take_while(|&i| trimmed_starts(&lines[i], "┃")).last()?;
            if bottom - top < 2 {
                return None;
            }
            let width = box_width(&lines[bottom])?;
            let rows = lines[top..bottom - 1]
                .iter()
                .map(|line| {
                    let row = after(line, '┃').unwrap_or_default();
                    row.into_iter().take(width).collect()
                })
                .collect();
            Some(rows)
        }
        // The editor between the last two rules.
        "pi" => {
            let bottom = last(&|_, l| is_rule(l))?;
            let top = (0..bottom).rev().find(|&i| is_rule(&lines[i]))?;
            Some(lines[top + 1..bottom].to_vec())
        }
        _ => None,
    }
}

/// How many characters of an opencode box a row holds after its `┃`: the `▀`
/// run under the bottom border is as wide as the box. `None` when the bottom
/// border is not `╹` followed by a run worth measuring.
fn box_width(border: &[char]) -> Option<usize> {
    let at = border.iter().position(|c| *c == '╹')?;
    let run = border[at + 1..].iter().take_while(|c| **c == '▀').count();
    (run >= 10).then_some(run)
}

/// Reads the input box of an agent of `kind` from its screen, as `hc read`
/// prints it.
pub fn check(kind: &str, screen: &str) -> Draft {
    match box_text(kind, screen) {
        None => Draft::Unknown,
        Some(text) if text.is_empty() => Draft::Empty,
        Some(_) => Draft::Typed,
    }
}

/// The text typed in the input box, its lines joined by spaces (empty for an
/// empty box), or `None` when the box is not on the screen.
pub fn box_text(kind: &str, screen: &str) -> Option<String> {
    let lines = strip(screen);
    let rows = input_box(kind, &lines)?;
    let typed: Vec<String> = rows.iter().map(|row| text(row).trim().to_string()).filter(|t| !t.is_empty()).collect();
    match typed.as_slice() {
        [only] if is_placeholder(kind, only) => Some(String::new()),
        _ => Some(typed.join(" ")),
    }
}

/// True for the kinds whose input box this module can find.
pub fn knows(kind: &str) -> bool {
    matches!(kind, "claude" | "codex" | "cursor" | "gemini" | "opencode" | "pi")
}

/// The screen's plain text, styling dropped.
pub fn plain(screen: &str) -> String {
    strip(screen).iter().map(|line| text(line)).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{}/tests/fixtures/prompt_box/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    /// Every captured screen, with the answer it must give.
    fn captured() -> Vec<(&'static str, &'static str, Draft)> {
        vec![
            ("claude", "claude-empty.ansi", Draft::Empty),
            ("claude", "claude-empty-after-turn.ansi", Draft::Empty),
            ("claude", "claude-draft.ansi", Draft::Typed),
            ("claude", "claude-named-session-empty.ansi", Draft::Empty),
            ("claude", "claude-named-session-draft.ansi", Draft::Typed),
            ("codex", "codex-empty.ansi", Draft::Empty),
            ("codex", "codex-draft.ansi", Draft::Typed),
            ("cursor", "cursor-empty.ansi", Draft::Empty),
            ("cursor", "cursor-draft.ansi", Draft::Typed),
            ("gemini", "gemini-empty.ansi", Draft::Empty),
            ("gemini", "gemini-draft.ansi", Draft::Typed),
            ("opencode", "opencode-empty.ansi", Draft::Empty),
            ("opencode", "opencode-empty-session.ansi", Draft::Empty),
            ("opencode", "opencode-draft.ansi", Draft::Typed),
            ("pi", "pi-empty.ansi", Draft::Empty),
            ("pi", "pi-draft.ansi", Draft::Typed),
        ]
    }

    #[test]
    fn every_supported_kind_tells_an_empty_box_from_a_draft() {
        for (kind, name, want) in captured() {
            assert_eq!(check(kind, &fixture(name)), want, "{kind} {name}");
        }
    }

    #[test]
    fn a_screen_read_as_plain_text_is_judged_like_the_captured_one() {
        // `hc read` hands out the screen with its styling dropped: no SGR, no
        // dim, no reverse, no background. The judgement must not change when the
        // styling is gone, or it only ever worked on the capture.
        for (kind, name, _) in captured() {
            let styled = fixture(name);
            assert_eq!(check(kind, &styled), check(kind, &plain(&styled)), "{kind} {name}");
        }
    }

    #[test]
    fn an_empty_box_in_plain_text_is_empty() {
        // The same question the ticker asks of a real pane.
        for (kind, name, want) in captured() {
            assert_eq!(check(kind, &plain(&fixture(name))), want, "{kind} {name}");
        }
    }

    #[test]
    fn the_real_pi_screen_reads_as_the_ticker_sees_it() {
        // `hc read` of a live pi pane (t8d), last lines only: an idle
        // coordinator's empty editor between the two rules. The cwd line under
        // the box is not part of the judgement and is rewritten here.
        let idle = fixture("real-pi-idle.txt");
        assert_eq!(check("pi", &idle), Draft::Empty);
        assert_eq!(box_text("pi", &idle).as_deref(), Some(""));
        // The same editor with a draft in it.
        assert_eq!(check("pi", &fixture("real-pi-draft.txt")), Draft::Typed);
        // Mid-turn (t109): the top rule carries its spinner label, so the editor
        // is not on the screen this reads — unknown, never empty.
        let working = fixture("real-pi-working.txt");
        assert_eq!(check("pi", &working), Draft::Unknown);
    }

    #[test]
    fn a_plain_row_of_text_is_a_draft() {
        // What a plain screen leaves of a real box: the row's characters, with
        // the harness's dim placeholder and reverse-video cursor gone.
        let rule = "─".repeat(40);
        assert_eq!(check("pi", &format!("{rule}\nyes, and check the tests\n{rule}\n")), Draft::Typed);
        assert_eq!(check("pi", &format!("{rule}\n   \n{rule}\n")), Draft::Empty);
        // A typed line and a second one: text either way.
        assert_eq!(check("pi", &format!("{rule}\nfirst line\nsecond line\n{rule}\n")), Draft::Typed);
    }

    #[test]
    fn every_box_reports_a_draft_when_a_typed_row_is_not_a_placeholder() {
        // The conservative side: text that is not a known placeholder is text,
        // never an empty box.
        for kind in ["claude", "codex", "cursor", "gemini", "opencode", "pi"] {
            let screen = match kind {
                "claude" => "─".repeat(40) + "\n❯\u{a0}ship it\n" + &"─".repeat(40) + "\n",
                "codex" => "  › ship it\n".to_string(),
                "cursor" => "  → ship it\n".to_string(),
                "gemini" => "│ > ship it │\n╰──────────╯\n".to_string(),
                "opencode" => "┃  ship it\n┃\n╹▀▀▀▀▀▀▀▀▀▀\n".to_string(),
                _ => "─".repeat(40) + "\nship it\n" + &"─".repeat(40) + "\n",
            };
            assert_eq!(check(kind, &screen), Draft::Typed, "{kind}");
        }
    }

    #[test]
    fn a_claude_suggestion_is_a_placeholder_and_its_lookalikes_are_not() {
        let rule = "─".repeat(40);
        let boxed = |row: &str| format!("{rule}\n❯\u{a0}{row}\n{rule}\n");
        // The suggestion claude rotates into its empty box, in every shape seen.
        assert_eq!(check("claude", &boxed("Try \"fix typecheck errors\"")), Draft::Empty);
        assert_eq!(check("claude", &boxed("Try \"write a test for <filepath>\"")), Draft::Empty);
        // Shapes that are not one quoted suggestion are text someone typed.
        assert_eq!(check("claude", &boxed("Try fix the test")), Draft::Typed);
        assert_eq!(check("claude", &boxed("Try \"fix the test")), Draft::Typed);
        assert_eq!(check("claude", &boxed("try \"fix the test\"")), Draft::Typed);
        assert_eq!(check("claude", &boxed("Try \"\"")), Draft::Typed);
        // Another kind's box never reads that shape as its placeholder.
        assert_eq!(check("pi", &format!("{rule}\nTry \"fix the test\"\n{rule}\n")), Draft::Typed);
    }

    #[test]
    fn the_topic_bar_beside_an_opencode_box_is_not_read_as_a_draft() {
        // The session layout draws a sidebar to the right of the box; the box's
        // bottom border is as wide as the box and says where the box ends.
        let session = fixture("opencode-empty-session.ansi");
        assert_eq!(box_text("opencode", &session).as_deref(), Some(""));
        assert_eq!(box_text("opencode", &plain(&session)).as_deref(), Some(""));
        // A placeholder in a narrow box, a session id beside it, out of reach.
        let bottom = format!("╹{}\n", "▀".repeat(25));
        let placeholder = "┃  Ask anything… \"Fix a TODO\"      t-0001\n┃  Build auto\n";
        assert_eq!(box_text("opencode", &format!("{placeholder}{bottom}")).as_deref(), Some(""));
        // Text inside the box is read, and the sidebar beside it is not.
        let typed = "┃  fix the tests                     t-0001\n┃  Build auto\n";
        assert_eq!(box_text("opencode", &format!("{typed}{bottom}")).as_deref(), Some("fix the tests"));
    }

    #[test]
    fn a_box_with_no_border_to_measure_is_unknown() {
        // Without its `╹▀▀▀` bottom the opencode box has no edge in plain text,
        // so what sits beside it cannot be told from what is in it.
        assert_eq!(check("opencode", "┃  something\n┃\n"), Draft::Unknown);
        assert_eq!(check("opencode", "┃  something\n╹\n"), Draft::Unknown);
        // A box beside a sidebar that cannot be measured is unknown too.
        assert_eq!(check("opencode", "┃  something                       t-0001\n╹▀▀▀\n"), Draft::Unknown);
    }

    #[test]
    fn the_top_border_may_carry_one_label() {
        // Captured from Claude Code 2.1.300 started with `--name hp-demo-t-0001`.
        assert_eq!(check("claude", &fixture("claude-named-session-draft.ansi")), Draft::Typed);
        assert_eq!(check("claude", &fixture("claude-named-session-empty.ansi")), Draft::Empty);
        let d = |n: usize| "─".repeat(n);
        let cases = [
            // Shapes Claude Code draws.
            ("no label", d(70), true),
            ("session name", format!("{} hp-demo-t-0001 ─", d(52)), true),
            ("session name reaching the pane's edge", format!("{} polish-release-notes", d(54)), true),
            ("agent name", format!("{} @reviewer ─", d(58)), true),
            ("label with spaces", format!("{} View teammates: `tmux -L claude-swarm a` ─", d(27)), true),
            ("fast-mode tag and session name", format!("{} fast mode  hp-demo-t-0001 ─", d(41)), true),
            ("effort and fast-mode tags", format!("{} ultracode  fast mode /fast ─", d(42)), true),
            ("history position, left-aligned", format!("─── History 3/12 {}", d(53)), true),
            ("coloured border", format!("\u{1b}[38;5;135m{} hp-demo-t-0001 ─\u{1b}[39m", d(52)), true),
            ("indented", format!("  {} hp-demo-t-0001 ─", d(50)), true),
            ("non-Latin label", format!("{} Архитектура трёхслойной системы ──", d(40)), true),
            // Lines that only resemble a border.
            ("two labels", format!("{} a {} b {}", d(10), d(10), d(10)), false),
            ("text before the dashes", format!("note {}", d(60)), false),
            ("prose between two dashes", "─ a note about the box ─".to_string(), false),
            ("fewer than ten dashes", "───── short ─".to_string(), false),
            ("other box-drawing characters", format!("╭{}╮", d(60)), false),
            // Claude Code truncates a label to the pane's width less three, which can leave no dashes before it.
            ("label filling a narrow pane", format!(" {} ─", "a-very-long-session-name-that-fills-it"), false),
        ];
        for (name, top, want) in cases {
            assert_eq!(is_labelled_rule(&strip(&top).remove(0)), want, "{name}: {top}");
            let (empty, typed) = if want { (Draft::Empty, Draft::Typed) } else { (Draft::Unknown, Draft::Unknown) };
            assert_eq!(check("claude", &format!("{top}\n❯ \n{}\n", d(70))), empty, "{name}, empty box");
            assert_eq!(check("claude", &format!("{top}\n❯ hello\n{}\n", d(70))), typed, "{name}, typed box");
        }
    }

    #[test]
    fn a_labelled_box_is_read_like_a_plain_one() {
        let plain = "─".repeat(40);
        for top in [format!("{} Архитектура трёхслойной системы ──", "─".repeat(60)), format!("── Session name {}", "─".repeat(30))] {
            // Claude Code draws a no-break space after `❯`.
            assert_eq!(check("claude", &format!("{top}\n❯\u{a0}\n{plain}\n")), Draft::Empty, "{top}");
            assert_eq!(check("claude", &format!("{top}\n❯\u{a0}\u{1b}[7m \u{1b}[0m\n{plain}\n")), Draft::Empty, "{top}");
            assert_eq!(check("claude", &format!("{top}\n❯ \u{1b}[2mTry \"fix lint\"\u{1b}[0m\n{plain}\n")), Draft::Empty, "{top}");
            assert_eq!(check("claude", &format!("{top}\n❯\u{a0}привет, проверь тесты\n{plain}\n")), Draft::Typed, "{top}");
            assert_eq!(check("claude", &format!("{top}\n❯\u{a0}\n  вторая строка\n{plain}\n")), Draft::Typed, "{top}");
            // A transcript `❯` above the box is still not the box.
            assert_eq!(check("claude", &format!("❯ an earlier prompt\n\n{top}\n❯\u{a0}\n{plain}\n")), Draft::Empty, "{top}");
            assert_eq!(check("claude", &format!("❯ an earlier prompt\n{top}\n")), Draft::Unknown, "{top}");
        }
    }

    #[test]
    fn only_the_top_border_may_carry_a_label() {
        let d = |n: usize| "─".repeat(n);
        let labelled = format!("{} hp-demo-t-0001 ─", d(52));
        // A labelled line does not close the box: it runs on to the plain rule.
        let open = format!("{labelled}\n❯ \n{labelled}\n");
        assert_eq!(check("claude", &open), Draft::Unknown);
        let runs_on = format!("{labelled}\n❯ \n{labelled}\n{}\n", d(70));
        assert_eq!(check("claude", &runs_on), Draft::Typed);
        // Pi's editor sits between two plain rules; a labelled line is not one of them.
        let pi = format!("{}\n{labelled}\n{}\n", d(70), d(70));
        assert_eq!(check("pi", &pi), Draft::Typed);
    }

    #[test]
    fn a_screen_without_the_box_or_an_unknown_kind_is_unknown() {
        assert_eq!(check("copilot", &fixture("claude-empty.ansi")), Draft::Unknown);
        assert_eq!(check("claude", "some output\nno box here\n"), Draft::Unknown);
        assert_eq!(check("codex", &fixture("claude-empty.ansi")), Draft::Unknown);
    }

    #[test]
    fn menus_and_multi_line_drafts_count_as_typed() {
        // Codex's trust menu uses the same `›` for its selection.
        let menu = "  Do you trust the contents of this directory?\n\u{1b}[1m›\u{1b}[0m 1. Yes, continue\n  2. No, quit\n";
        assert_eq!(check("codex", menu), Draft::Typed);
        let rule = "─".repeat(40);
        let multi = format!("{rule}\n❯ \n  second line typed\n{rule}\n");
        assert_eq!(check("claude", &multi), Draft::Typed);
        let placeholder = format!("{rule}\n❯ \u{1b}[2mTry \"fix lint\"\u{1b}[0m\n{rule}\n");
        assert_eq!(check("claude", &placeholder), Draft::Empty);
        // A transcript line with `❯` that is not under a rule is not the box.
        let transcript = format!("❯ an earlier prompt\n\n{rule}\n❯ \n{rule}\n");
        assert_eq!(check("claude", &transcript), Draft::Empty);
    }

    #[test]
    fn a_cursor_over_a_placeholder_is_still_a_placeholder() {
        // Cursor draws its cursor as a reverse-video cell over the placeholder's
        // first letter, which plain text leaves as the letter itself.
        assert_eq!(check("cursor", &fixture("cursor-empty.ansi")), Draft::Empty);
        assert_eq!(
            check("cursor", "\u{1b}[2m→ \u{1b}[0m\u{1b}[7mP\u{1b}[0m\u{1b}[2mlan, search, build anything\u{1b}[0m\n"),
            Draft::Empty
        );
        // One typed character is text.
        let rule = "─".repeat(40);
        assert_eq!(check("pi", &format!("{rule}\n\u{1b}[7mx\u{1b}[0m   \n{rule}\n")), Draft::Typed);
    }
}
