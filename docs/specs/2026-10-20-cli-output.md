# Spec: output for people and for agents

Date: 2026-10-20. Status: implemented.

## Problem

Every line skillcask prints is plain text: errors, warnings and the states in `status` look the
same, `doctor` prints `problem:` and `fixed:` lines in no particular order, and errors carry the
next step inside the sentence (`…; use --global`). Coding agents run skillcask too and pay for
every padded column and every `ok` row in tokens. `auth set` reads the token through git's prompt,
which echoes nothing, so a user cannot tell whether the paste arrived.

## Decisions

- One output mode per run, decided at start:
  - **agent**: `--agent`, `SKILLCASK_AGENT=1`, or detected from `AI_AGENT` (non-empty) or
    `CLAUDECODE=1`. `SKILLCASK_AGENT=0` turns detection off. No other agents are guessed.
  - **person**: everything else.
- Colour only for a person, per stream, when the stream is a terminal: red `error`, yellow
  `warning`, cyan `hint` and `note`, green verbs (`installed`, `added`…) and states (`ok`). `NO_COLOR`
  (non-empty) turns it off, `CLICOLOR_FORCE` (non-empty, not `0`) on. On Windows only inside
  Windows Terminal (`WT_SESSION`), since the legacy console prints the escape codes. Plain ANSI
  codes, no new dependency.
- Errors keep their message and may carry a hint, printed as its own line:
  `error: the home directory is the global scope` / `  hint: run skillcask --global install`.
- `status` for a person: the table with coloured states and a summary line. For an agent: rows
  with a problem only, fields separated by tabs, then one summary line.
- `doctor` for a person: the checks that passed (`✓ links: 12 ok`), then problems (`✗`), repairs
  (`✓`) and warnings (`!`) grouped, then a summary. For an agent: problems, repairs and warnings
  only, one line per item without symbols.
- `outdated` and other tables: aligned columns for a person, tab-separated for an agent.
- `auth set` confirms what it received: `stored a 53-character token for <host> with git's
  credential helper (osxkeychain)`. Only the length, never any character of the token.

- Text that comes from a registry (names, versions, descriptions, URLs) can carry terminal escape
  sequences, for example OSC 52 writing the clipboard. `search` replaces every control character in
  it, and errors, warnings and notes replace all of them except their own line breaks.

## Non-goals

- JSON output.
- Masking the token prompt with `*`: it needs the terminal in raw mode, which means a dependency
  with `unsafe` code or writing it ourselves against `#![forbid(unsafe_code)]`.

## Testing

- Unit: mode and colour detection from a fake environment for each variable above.
- Integration: the test harness clears the agent and colour variables; `status` and `doctor` in
  agent mode print no `ok` rows and no padding; `CLICOLOR_FORCE=1` colours `error:`; `NO_COLOR`
  beats it; an error with a hint prints the hint line; `auth set` reports the length and not the
  token.
