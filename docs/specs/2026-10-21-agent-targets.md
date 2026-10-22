# Spec: agent presets and default targets

Date: 2026-10-21. Status: implemented, for 0.6.0.

## Problem

`[agents] targets` takes directory paths only. To support another agent a user has to know
where it reads skills, and that differs between project and home (`.github/skills` vs
`~/.copilot/skills`). Every `skills.toml` repeats the list, because `init` writes the built-in
default into it; a user who wants the same agents in every project edits each one, and
`config.toml` cannot help since it reads only `[policy]`.

Most agents no longer need a link at all. Of the 15 agents checked on 2026-10-21, 12 read the
shared `.agents/skills` (project) and `~/.agents/skills` (home), which is the skillcask store.
The default target `.junie/skills` is redundant in a project for that reason.

## Goals

1. A target can be an agent name; skillcask knows the directories for the project and the
   global scope, and links nothing where the agent reads the store.
2. `config.toml` sets the targets for every project and the global scope; a `skills.toml`
   overrides them.
3. A target dropped from the list leaves no stale links behind.

## Non-goals

- Detecting agents from the files in a project (`.cursor/`, `.kiro/`). The store covers almost
  every agent, so a wrong guess costs more than it saves.
- Converting skills for agents without `SKILL.md` support (rule files, prompts). Package content
  stays data.
- Agent-specific skill variants (`.roo/skills-{mode}`).
- Merging the target lists of several layers.

## Presets

A target without `/` is an agent name; an unknown name is exit 1 listing the known ones. A path
needs a `/` (`./skills`, `tools/skills`), so a typo never creates a directory. "Store" means the
agent reads `.agents/skills` or `~/.agents/skills` and gets no link.

| Name | Project | Global (under `~`) | Source |
|---|---|---|---|
| `claude` | `.claude/skills` | `.claude/skills` | code.claude.com/docs/en/skills; `.agents` support is an open request (anthropics/claude-code#31005) |
| `kiro` | `.kiro/skills` | `.kiro/skills` | kiro.dev/docs/skills; no `.agents` support documented |
| `junie` | store | `.junie/skills` | junie.jetbrains.com/docs/agent-skills.html lists `.agents/skills` for projects only |
| `codex` | store | store | learn.chatgpt.com/docs/build-skills |
| `copilot` | store | store | docs.github.com (Copilot CLI, coding agent), code.visualstudio.com (VS Code) |
| `cursor` | store | store | cursor.com/docs/context/skills |
| `gemini` | store | store | geminicli.com/docs/cli/skills; project skills need a trusted folder |
| `opencode` | store | store | opencode.ai/docs/skills |
| `amp` | store | store | ampcode.com/docs/customize/skills |
| `devin` | store | store | docs.devin.ai (Windsurf is now Devin Desktop) |
| `roo` | store | store | Roo Code docs, features/skills |
| `cline` | store | store | cline/cline source (`skill-directories.ts`); its docs omit `.agents` |
| `goose` | store | store | goose-docs.ai, using-skills |
| `factory` | store | store | docs.factory.com/cli/configuration/skills |
| `vibe` | store | store | github.com/mistralai/mistral-vibe README |

Names and paths mix: `targets = ["claude", "copilot", "./tools/skills"]`. Two entries that
resolve to the same directory link once. The table lives in one place in the code and in the
README; a change in an agent is a one-line edit and a patch release.

## Layers

`targets` comes from the first layer that sets it, as a whole list:

1. `[agents] targets` of the manifest (`skills.toml` of the project, or the global one with
   `--global`);
2. `[agents] targets` of `config.toml`;
3. built-in default `["claude"]`.

```toml
# ~/.config/skillcask/config.toml
[agents]
targets = ["claude", "kiro"]

[policy]
min-release-age = "3d"
```

`config.toml` accepts `[agents]` with `targets` only; `local` and unknown keys are exit 1, as in
`[policy]`. There is no `enforce` for targets: links are a local convenience, not a security
rule. No environment variable either; CI rarely needs links, and the manifest can set them.

`init` and `init --global` no longer write `targets`; the template has a commented line naming
the layers. A manifest written by an older version keeps its explicit list and therefore
overrides `config.toml`; the README says to delete the line to follow the config. `status`
prints `agents.targets = [<as written>] (<layer>): <directories>`, or `.agents/skills only`.

The built-in default changes from `[".claude/skills", ".junie/skills"]` to `["claude"]`. In the
global scope that drops `~/.junie/skills`; a Junie user adds `"junie"` to `config.toml`.

## Stale links

On `install`, `update` and `remove`, skillcask removes the links it made (`made_by_skillcask`:
a link to the store, or a copy the block lists) in directories that are no longer targets:

- every directory the previous managed block lists (`.gitignore` in a project, the managed list
  in the global scope);
- in a project also every preset directory, since links of hand-written and project skills are
  committed and not in the block; the user commits the deletion.

Other entries stay; so does an empty directory. A directory behind a symlink is skipped. In home
only the block counts: other links there are the user's. `doctor` reports the same links and
`doctor --fix` removes them.

## Duplicates

Several agents read both `.agents/skills` and `.claude/skills` (Copilot, Cursor, OpenCode, Amp,
Junie, Cline, Goose, Devin). With `claude` in the targets they find each skill twice, under the
same name and pointing to the same directory. Claude Code documents that it loads such a skill
once; for the others it is unverified. This is the situation today with the current default, so
the change does not make it worse; a user who does not run Claude Code drops `claude` from the
targets. To check before release: Copilot CLI and Cursor with one skill in both directories.

## Testing

- Unit: preset resolution per scope, unknown name, path without `/`, layer precedence
  (manifest > config > default), `config.toml` `[agents]` with an unknown key, two entries
  resolving to one directory.
- Integration: `config.toml` targets apply to a project without `targets`; a project list
  overrides; removing `claude` from `config.toml` removes `.claude/skills/<name>` links on the
  next `install` and leaves foreign entries; `--global` with `junie` links `~/.junie/skills`.
- Windows: a preset target follows the existing copy fallback.
