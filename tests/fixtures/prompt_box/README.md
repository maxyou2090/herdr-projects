# Input-box fixtures

Two kinds of sample, and the difference matters: the judgement may only use what
`hc read` actually prints.

- `*.ansi` — screens captured from a real harness through herdr-core's VT
  formatter (styles included). They are the record of what each harness draws,
  and `strip` drops the escapes, so they are also the record of what a real read
  of that screen looks like.
- `real-pi-*.txt` — `hc read` of a live pi pane, verbatim (last lines only). This
  is the real thing: no escape sequence, no dim, no reverse, no background. The
  cwd line under the box is rewritten so the capture carries no local path; it
  is outside the box and takes no part in the judgement.

The test `a_screen_read_as_plain_text_is_judged_like_the_captured_one` pins the
first kind against the second: `check(kind, ansi)` must equal
`check(kind, plain(ansi))`, or the judgement is reading styling that a real pane
never shows.
