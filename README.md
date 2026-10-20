# skillcask

A lockfile-first package manager for AI agent skills: directories with a `SKILL.md`, read by
Claude Code, Codex, Junie and others.

skillcask installs skills the way Cargo or Composer install libraries. It reads a manifest with
version constraints and commits a lock with content hashes. In CI it does frozen installs that
fail on drift, and it links every skill into each agent's skill directory. It ships as a single
static binary with no runtime, so it works in a repository in any language.

Skills come from Composer repositories or straight from GitHub. Packagist serves public skill
packages; GitLab's Composer registry, Satis, Private Packagist or the static repository built by
`skillcask publish` serve private ones. Any GitHub repository with `SKILL.md` files works too,
which covers most of what [skills.sh](https://skills.sh) lists. skillcask does not need PHP,
Composer or Node.

## Install

**macOS, Linux**

```sh
brew install puniserv/tap/skillcask
# or, without Homebrew:
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/puniserv/skillcask/releases/latest/download/skillcask-installer.sh | sh
```

**Windows (PowerShell)**

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/puniserv/skillcask/releases/latest/download/skillcask-installer.ps1 | iex"
```

The two installer scripts download the binary for your platform from the latest release and put
it in `~/.cargo/bin` (`%USERPROFILE%\.cargo\bin` on Windows).

**npm, bun**

```sh
npm install -g skillcask      # global
bun add -g skillcask          # global, bun
npx skillcask install --frozen
bunx skillcask install --frozen
```

The npm package is a thin wrapper that downloads the release binary for your platform from
GitHub. It does not check a checksum itself; where that matters, use Homebrew or a release
archive and verify it.

**Cargo**

```sh
cargo install --locked skillcask
```

**Release archives** for macOS, Linux and Windows are on the
[releases page](https://github.com/puniserv/skillcask/releases), with sha256 checksums and GitHub
build provenance attestations:

```sh
gh attestation verify skillcask-x86_64-unknown-linux-gnu.tar.xz -R puniserv/skillcask
```

## Use

```sh
skillcask add netresearch/agent-jira-skill   # resolve, write skills.toml + skills.lock, install
skillcask add github:mattpocock/skills/tdd   # one skill from a GitHub repository
skillcask install --frozen                   # CI: install exactly the lock or exit non-zero
skillcask status                             # manifest vs lock vs installed files
```

`add` creates:

```
skills.toml                  # manifest, commit it
skills.lock                  # lock, commit it
.agents/skills/<name>/       # installed skill (gitignored)
.agents/skills/.skillcask-installed  # what this machine installed (gitignored)
.claude/skills/<name>  -> ../../.agents/skills/<name>
.junie/skills/<name>   -> ../../.agents/skills/<name>
.gitignore                   # managed block
```

Project skills live in `.skills/<name>/SKILL.md` (`[agents] local`). skillcask links each one
into the store (`.agents/skills/<name> -> ../../.skills/<name>`) and from there into every agent
directory. These links are committed, so they work in every clone. A directory written by hand
straight into `.agents/skills/` is linked the same way. A project skill with the same name as a
locked one is an error. When a skill with the same name exists in your home (`~/.claude/skills`,
`~/.agents/skills`, …), `install` and `doctor` print a warning: Claude Code runs the personal one,
Codex lists both.

### skills.toml

```toml
[repositories]
acme = { type = "composer", url = "https://git.example.com/api/v4/group/42/-/packages/composer/packages.json" }
# A repository you review yourself can skip the cooldown; the others keep it.
own = { url = "https://git.example.com/own/packages.json", min-release-age = "0" }
# Default sources, written by `init`; `= false` turns one off. Packagist is always searched last.
packagist = { url = "https://repo.packagist.org" }
github = { type = "github", url = "https://api.github.com" }   # GitHub Enterprise: its API URL

[skills]
"netresearch/agent-jira-skill" = "^3.34"
"acme/agents-md" = { version = "^1.0", repository = "acme" }
"acme/review" = { version = "1.4.2", repository = "acme", skills = ["review"] }
"mattpocock/skills" = { github = true, version = "^1.3", skills = ["tdd", "code-review"], rename = { code-review = "mp-code-review" } }
"anthropics/skills" = { github = true, skills = ["pdf"] }      # default branch

[agents]
targets = [".claude/skills", ".junie/skills"]
local = ".skills"        # project skills

[policy]
min-release-age = "3d"   # cooldown for add/update; "0" disables
allow-ranges = true      # false: every version must be exact
packagist = true         # false: drop the implicit Packagist repository
github-requires = false  # true: dependencies may come from GitHub
allow-sources = ["acme", "own", "packagist", "github"]   # only these sources; default: all
```

Constraints follow Composer semantics: `^0.3` means `>=0.3.0 <0.4.0`, `~1.2` means
`>=1.2 <2.0`, and pre-releases are only selected when the constraint names a stability
(`^1.0@beta`).

### Allowed sources and policy for every project

`allow-sources` lists the only places skills may come from: repository names, `packagist`,
`github`, or `https://` URL prefixes (`https://api.github.com/repos/acme/` allows one GitHub
organisation). Other repositories get no request, and `install` refuses a lock entry from them,
also with `--frozen`, so a committed lock cannot bring them back. Dependencies follow the same
rule.

Policy can also come from `config.toml` next to the global manifest (`~/.config/skillcask`) and
from the environment. The first layer that sets a key wins:

1. `SKILLCASK_ALLOW_SOURCES`, `SKILLCASK_MIN_RELEASE_AGE`, `SKILLCASK_ALLOW_RANGES`,
   `SKILLCASK_PACKAGIST`, `SKILLCASK_GITHUB_REQUIRES` (overriding an enforced key prints a
   warning);
2. keys `config.toml` lists in `enforce`: a project that sets them otherwise fails;
3. `[policy]` in `skills.toml`;
4. the other keys of `config.toml`, as defaults.

```toml
# ~/.config/skillcask/config.toml: only the company registry and GitHub, in every project
[policy]
allow-sources = ["https://gitlab.example.com/", "github"]
enforce = ["allow-sources"]
```

`config.toml` and the environment take URL prefixes, not repository names, since a name means
whatever a project calls it. `skillcask status` prints each key that is set and where it came
from.

### Dependencies

A skill that needs other skills lists them in its `SKILL.md`:

```yaml
---
name: gitlab-branch-mr
version: 1.2.0
requires:
  acme/security-review: ^1.0
  github:mattpocock/skills/pr: main
---
```

`add`, `install` and `update` resolve, lock and install them like your own entries: same
cooldown, same hashes, one version per package (the highest one every requirer accepts). They
stay out of `skills.toml`; the lock records each package's `requires` and switches to
`version = 2`. A dependency is looked up only in your repositories, in order, then Packagist;
a `github:` one needs `github-requires = true`. `remove` drops dependencies nobody needs any
more, and `status` shows them as `via <requirer>`.

### Name conflicts

Skills have no namespaces: an agent knows a skill by its name. Two skills with the same name in
one project are an error. Install one of them under another name with `rename`:

```toml
"mattpocock/skills" = { github = true, rename = { code-review = "mp-code-review" } }
```

skillcask then installs `.agents/skills/mp-code-review` and changes the `name:` line in its
`SKILL.md`; the lock records the original name and the hash of the renamed copy. Nothing else in
the skill changes, so text that mentions the old name (`/code-review`) stays as it is.
`add`, `install` and `update` offer the rename when they hit a conflict in a terminal.

### Personal skills (`--global`)

`skillcask --global` (`-g`) manages skills for your user instead of a project, with the same
manifest, lock and verification:

```sh
skillcask -g add github:mattpocock/skills/grill-me
skillcask -g install --frozen    # e.g. from a dotfiles repository
```

- `skills.toml`, `skills.lock` and an optional `auth.json` live in `~/.config/skillcask`
  (`$XDG_CONFIG_HOME/skillcask`; `SKILLCASK_HOME` overrides both). Keep them in your dotfiles.
- Skills go to `~/.agents/skills` (read by Codex), with links in `~/.claude/skills` and
  `~/.junie/skills` (`[agents] targets`, relative to home). Links are absolute, so a symlinked
  `~/.claude` works. `[agents] local` does not apply.
- Personal skills you wrote by hand stay where they are and are not linked anywhere.
- The list of managed paths is `~/.agents/skills/.skillcask-installed`, local to the machine;
  nothing touches git.

### Commands

Every command except `publish` takes `-g, --global`.

| Command | |
|---|---|
| `init` | Create `skills.toml`. |
| `add <vendor/name>[@constraint] [--repository r]` | Resolve, lock and install. |
| `add github:<owner/repo>[/skill][@version-or-ref]` | The same from a GitHub repository, see below. |
| `remove <vendor/name>…` | Drop from manifest and lock; delete files, links and gitignore lines. |
| `install [--frozen]` | Install the lock. `--frozen` (implied by `CI`) fails when the lock is out of date. |
| `update [vendor/name…]` | Re-resolve within constraints. |
| `outdated` | Newer versions, and when cooled-down ones become eligible. |
| `status` | Exit 1 when manifest, lock or installed files disagree. |
| `doctor [--fix]` | Broken or committed links, stale gitignore block, modified skills. |
| `search <query>` | Packagist (`type=ai-agent-skill`) and skill packages of the other repositories, GitLab included. |
| `auth set\|remove <host>` | Keep a registry token in git's credential helper (Keychain, Credential Manager, libsecret). |
| `publish --vendor v --base-url URL --out DIR [--inline] [--dist-url TPL] [paths…]` | Build a static Composer repository from a monorepo. |
| `<other>` | Runs `skillcask-<other>` from `PATH`, like git and cargo plugins. |

Exit codes: 0 ok, 1 usage, manifest, lock or status problems, 2 authentication, 3 network, 4 hash mismatch,
5 IO or archive error.

Output is coloured in a terminal (`NO_COLOR` turns it off, `CLICOLOR_FORCE` on) and errors name
the next step on a `hint:` line. For coding agents, `--agent` or `SKILLCASK_AGENT=1` prints terse,
uncoloured, tab-separated output with only the rows that need action; it is on by itself when
`AI_AGENT` or `CLAUDECODE=1` is set, unless `SKILLCASK_AGENT=0`.

### Skills from GitHub and skills.sh

[skills.sh](https://skills.sh) lists skills as `owner/repo/skill`, for example
`mattpocock/skills/grill-me`. Put `github:` in front and add it:

```sh
skillcask add github:mattpocock/skills/grill-me   # one skill
skillcask add github:mattpocock/skills/tdd        # a second one from the same repository
skillcask add github:anthropics/skills            # every skill in the repository
```

All skills of one repository share one entry in `skills.toml`:

```toml
"mattpocock/skills" = { github = true, version = "^1.3", skills = ["grill-me", "tdd"] }
```

Which version you get:

- **Repository with version tags** (`v1.3.1`): `add` writes `version = "^<major.minor>"`, like
  for Composer packages, and `update` moves within it. The release time of a tag is its commit
  date.
- **Repository without tags**: the entry follows the default branch; `ref = "main"` names a
  branch, tag or commit instead. The lock pins the commit sha, so `install` keeps installing that
  commit until you run `update`.
- `@` picks explicitly: `github:owner/repo@^2.0` (version tags), `github:owner/repo@main` or
  `@<commit sha>` (ref).
- With `policy.allow-ranges = false` a branch counts as a range: `add` writes the full commit sha
  as `ref`, and a hand-written branch name is rejected.

Skills are found by their `SKILL.md` anywhere in the repository, except inside another skill.
The install name is the frontmatter `name`; when that is not a valid name (`Jira Automation`), the
directory name is used. `skills = [...]` takes these names.

GitHub allows 60 anonymous API requests per hour. `add`, `update` and `outdated` use a few per
entry; `install` from the lock uses none, it downloads the pinned archive. Set `GITHUB_TOKEN`
(GitHub Actions has one) or Composer's `"github-oauth": {"github.com": "<token>"}` in `auth.json`
for a higher limit and for private repositories. `SKILLCASK_GITHUB_API` points at GitHub
Enterprise (`https://ghe.example.com/api/v3`).

skills.sh itself is not queried: its API needs a Vercel token. Search on the website, then add
what you picked.

## Security

- The lock pins the version, the commit reference, the sha256 of the downloaded archive and
  the tree hash of every installed skill. `install` verifies both hashes, so what gets installed
  is what was reviewed.
- HTTPS only, verified against the system trust store, so corporate CAs work. Verification
  cannot be turned off; `SKILLCASK_CA_FILE` replaces the trust store with a PEM bundle.
- Archives with absolute paths, `..`, devices or symlinks that leave their skill are rejected.
  No symlink is ever written from a package. Size and file count are capped (`--max-download`,
  `--max-unpacked`, `--max-files`).
- File names must be valid on every OS: Windows-reserved names, `<>:"|?*\`, control characters
  and paths that collide by case or Unicode form fail a skill that contains them, and are
  skipped outside the selected skills.
- No package code runs, ever. No telemetry.
- `min-release-age` for GitHub sources uses times GitHub sets: release publication for tags,
  push times from the Activity API for branches. A tag without a release, or a ref GitHub lists no
  pushes for, falls back to commit dates, which the committer sets; reviewing what `add`/`update`
  change in `skills.lock` is the real check there.
- Credentials come from Composer's `auth.json` / `COMPOSER_AUTH` (`http-basic`, `bearer`,
  `gitlab-token`, `gitlab-oauth`), then from git's credential helper, and are never printed.
  `skillcask auth set git.example.com` keeps a token in the macOS Keychain, the Windows Credential
  Manager or libsecret instead of a plain-text file (the default username `PRIVATE-TOKEN` sends
  GitLab's header; `--username oauth2` a Bearer token). `SKILLCASK_GIT_CREDENTIALS=0` turns the
  helper off. In GitLab CI use `http-basic` with the
  user `gitlab-ci-token` and the password `$CI_JOB_TOKEN`.

## Publishing skills

**One repository per skill:** a `composer.json` with `"type": "ai-agent-skill"` (the
[netresearch convention](https://github.com/netresearch/composer-agent-skill-plugin)) and semver
tags. Packagist or GitLab's Composer registry picks up the tags; skillcask plays no part.

**Monorepo:** give each `SKILL.md` a `version` in its frontmatter and run

```sh
skillcask publish --vendor acme --base-url https://skills.example.com --out public skills/
```

This writes `packages.json`, `p2/acme/<name>.json` and deterministic zips under `dist/`. Published
versions are immutable, and re-running on unchanged skills changes no file. Serve the directory
over HTTPS.

Hosts that cannot serve `/` inside file names (GitLab's Generic Package Registry) take a single
file instead:

```sh
skillcask publish --inline --vendor acme --out composer/ skills/ \
  --dist-url 'https://git.example.com/api/v4/projects/7/packages/generic/{vendor}-{name}/{version}/{name}-{version}.zip'
```

`--inline` puts every package into `packages.json` and writes no `p2/`. Upload each
`dist/<vendor>/<name>/<version>.zip` to the URL the template gives, and commit `packages.json` to
a repository. A repository `url` with a query string is used as given, so clients can read the
file through the GitLab raw files API
(`.../repository/files/composer%2Fpackages.json/raw?ref=main`) with a `gitlab-token`.

## Design

See [docs/specs/2026-10-19-skillcask-v1.md](docs/specs/2026-10-19-skillcask-v1.md).

## License

MIT OR Apache-2.0.
