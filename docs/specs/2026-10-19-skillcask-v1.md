# Spec: skillcask — a lockfile-first package manager for AI agent skills

Date: 2026-10-19. Status: v1.

## Problem

Agent skills (a directory with `SKILL.md`, read by Claude Code, Codex, Junie and others) are
copied into repositories by hand or by tools that do not freeze what they install.

- **Vercel Skills CLI** (`npx skills`, v1.7.2, source checked at `vercel-labs/skills@b465908`):
  writes a project `skills-lock.json` with `source`, `ref` (branch or tag) and `computedHash`
  (sha256 of the files), but `experimental_install` re-fetches from `ref`, not from a commit, and
  never checks `computedHash` (`src/install.ts`). A moved tag or branch installs different content
  silently. No semver for git sources, `update` takes the newest, no CI drift check. Needs Node;
  anonymous telemetry on by default.
- **`netresearch/composer-agent-skill-plugin`** (MIT): a Composer plugin with a package
  convention (`"type": "ai-agent-skill"`, 57 such packages on Packagist), a lock for direct git
  sources, and an `AGENTS.md` index instead of agent skill directories. Works only where Composer
  and PHP are present.
- **In-house sync scripts**: semver per skill, committed lock, sha256 per package, CI drift check — but bash, tied to one
  GitLab instance and its Generic Package Registry layout.

Supply-chain security practice asks for a committed lockfile, deterministic installs that fail on drift, pinned versions, a cooldown on
freshly published versions, and no install-time code execution. No skill tool does all of it for
repositories in any language.

## Goal

One static binary that installs agent skills the way Composer or Cargo install libraries: a
manifest with version constraints, a committed lock with content hashes, frozen installs in CI,
and links into every agent's skill directory. Skills are distributed through the Composer
repository protocol, so Packagist serves public skills and GitLab's Composer registry (or any
static Composer repository) serves private ones, without a new registry.

Success: a repository in any language runs `skillcask install --frozen` in CI and gets
byte-identical skills to the ones committed in the lock, or a non-zero exit.

## Non-goals (v1)

- Composer `require` of skill packages (ignored; skills declare dependencies in `SKILL.md`, D16).
- Installing from plain git repositories or URLs other than GitHub (see D11).
- Trust prompts: an entry in `skills.toml` is the decision.
- Running any code from a package (scripts, hooks, plugins). Never, not only in v1.
- GitLab Generic Package Registry archive layouts (`.skill` archives + `CATALOG.md`).
- A hosted registry or web UI.
- Telemetry.

## Decisions

| # | Decision | Why |
|---|---|---|
| D1 | Rust, single static binary. | No runtime in the target repository (PHP, Ruby, .NET, JS alike); one artefact to sign and verify. |
| D2 | Composer repository protocol v2 is the only backend (plus inline `packages`, D12). | Packagist (public), GitLab Composer registry, Satis and Private Packagist (private) all speak it; one client covers them. |
| D3 | Package convention from netresearch: `"type": "ai-agent-skill"`, `extra.ai-agent-skill`. | Existing packages install unchanged; no competing convention. |
| D4 | Manifest `skills.toml`, lock `skills.lock`, both TOML, project root. | Language-neutral, comments allowed, familiar from Cargo. |
| D5 | Canonical install directory `.agents/skills/<name>`; other agents get relative symlinks. | Codex reads `.agents/skills` directly; Vercel uses the same canonical place; one copy on disk. |
| D6 | The lock pins version, commit reference and two sha256 hashes; `install` verifies both. | Closes the gap in Vercel's lock: what is installed is what was reviewed. |
| D7 | Cooldown `min-release-age` (default 3 days) applies to `add` and `update`. | Most malicious releases are pulled within days; installing from the lock is unaffected. |
| D8 | Authentication reads Composer's `auth.json` / `COMPOSER_AUTH`. | Existing GitLab and Private Packagist credentials, including CI job tokens, work as they are. |
| D9 | Publishing supports both one-repo-per-skill (standard Composer) and monorepos (`skillcask publish` builds a static Composer repository). | Many skills live in monorepos; GitLab's Composer registry accepts one package per project. |
| D10 | MIT OR Apache-2.0, no telemetry. | Usual Rust dual licence; nothing leaves the machine except registry requests. |
| D11 | GitHub repositories are a second source (`github = true`), resolved through the GitHub REST API; the lock format is unchanged. | Most public skills (the ones skills.sh lists) live in GitHub repositories without Composer metadata. Pinning the commit sha plus both hashes keeps D6. skills.sh itself is not used: its API needs a Vercel OIDC token. |
| D12 | Repositories may carry every package inline in `packages.json` (Composer v1/Satis format) instead of `metadata-url`; `publish --inline` writes it, `--dist-url` places the zips. | Private monorepos can then be hosted on GitLab with no new infrastructure: `packages.json` lives in git and is read through the raw files API, zips go to the Generic Package Registry, whose file names cannot contain `/`. One file stays small for a few hundred skills. |
| D13 | `--global` manages personal skills: store `~/.agents/skills`, links in `~/.claude/skills` etc., manifest and lock in `~/.config/skillcask`; a skill name clash inside one scope is solved with `rename`, a clash across scopes is a warning. | Agents read personal skills from home and skills have no namespaces. The design is in `docs/specs/2026-10-20-global-and-aliases.md`. |
| D14 | `publish` records the tree hash of each new version in `extra.ai-agent-skill-tree`; without the published zip in `--out`, a version whose `shasum` differs counts as unchanged when its tree matches, and no zip is written for it. | CI that does not keep `dist/` must not fail on every published version when a `zip` or deflate update changes the bytes of the same tree. Downloading every old zip first would cost a request per version on each run. |
| D15 | A repository may set its own `min-release-age`, which overrides `policy.min-release-age` for packages resolved there; GitHub sources keep the policy value. | Internal skills are reviewed in merge requests and published after every merge; a project-wide `"0"` would also drop the cooldown for public sources. |
| D16 | A skill declares dependencies in a top-level `requires:` map of its `SKILL.md` frontmatter; they are resolved from the consumer's repositories (GitHub only with `policy.github-requires = true`), one version per package, and locked with `requires` in a `version = 2` lock. | Skills that call other skills break silently when one is missing. Composer `require` already holds `php` and plugins in existing packages and cannot name GitHub sources; reading the downloaded `SKILL.md` works for every source. The design is in `docs/specs/2026-10-20-skill-dependencies.md`. |
| D17 | A host no Composer source covers is asked from git's credential helper (`git credential fill`), on by default, off with `SKILLCASK_GIT_CREDENTIALS=0`; `skillcask auth set <host>` stores a token there. | Tokens then live in the macOS Keychain, the Windows Credential Manager or libsecret instead of a plain-text `auth.json`, with no new dependency: every developer machine has git and a helper. |
| D18 | Policy has layers: environment (`SKILLCASK_*`), keys a user `config.toml` lists in `enforce`, the project `[policy]`, the rest of `config.toml`, built-in defaults. `allow-sources` limits sources by name, `packagist`, `github` or URL prefix, at resolution and at install from the lock. `init` lists Packagist and GitHub in `[repositories]`; Packagist is always searched last. | Organisations that may only use their private registry need one rule for every project, CI and dependency, which a project cannot loosen unless allowed. Checking the lock too means a committed lock cannot bypass it. The design is in `docs/specs/2026-10-20-source-policy.md`. |
| D19 | Output has two modes: agent (`--agent`, `SKILLCASK_AGENT`, or detected from `AI_AGENT` / `CLAUDECODE`) prints terse, uncoloured, tab-separated lines and only problems; person gets colour in a terminal (off with `NO_COLOR`), hints on their own line and grouped `doctor` output. | Agents pay for padding and `ok` rows in tokens, people need to see at a glance what failed and what to run next. ANSI codes need no dependency. The design is in `docs/specs/2026-10-20-cli-output.md`. |
| D20 | A repository's `min-release-age` (D15) cannot go below a `min-release-age` set by the environment or enforced by `config.toml`; it may raise it. The higher value applies without a message; `status` shows where the policy value comes from. Otherwise D15 applies unchanged. | D15 let a project write `min-release-age = "0"` on any repository and so loosen a cooldown its organisation fixed, against D18. A floor keeps D15's use (a stricter or reviewed internal repository) and the D18 promise. |
| D21 | `GITHUB_TOKEN`/`GH_TOKEN` go only to the hosts of the GitHub APIs the user or the CI runner names: `api.github.com`, `SKILLCASK_GITHUB_API`, GitHub Actions' `GITHUB_API_URL`. A `github` url in `skills.toml` gets a token only from `auth.json`, `COMPOSER_AUTH` or git's credential helper, which are keyed by host. `"github"` in an `allow-sources` from the environment or an enforced key means `<api>/repos/` of those APIs only. | A committed manifest chose the host the environment token was sent to, so a pull request could collect a CI job's token, and widen an enforced `"github"`. Host-keyed credentials (GitLab, GHE in `auth.json`, `gh`) already cannot leave their host; GHE in GitHub Actions keeps working through `GITHUB_API_URL`. GHE set only in `skills.toml` with `GITHUB_TOKEN` on another CI needs `SKILLCASK_GITHUB_API` or a host-keyed token. |

## Project files

```
skills.toml                  # manifest, committed
skills.lock                  # lock, committed, generated
.agents/skills/<name>/       # installed skill (managed: gitignored; hand-written: committed)
.claude/skills/<name>  -> ../../.agents/skills/<name>
.junie/skills/<name>   -> ../../.agents/skills/<name>
.skills/<name>/              # project skill, committed; .agents/skills/<name> links to it
.gitignore                   # managed block
```

### `skills.toml`

```toml
[repositories]
# packagist = { type = "composer", url = "https://repo.packagist.org" }  # implicit default
acme = { type = "composer", url = "https://git.example.com/api/v4/group/42/-/packages/composer/packages.json" }
# own = { url = "…", min-release-age = "0" }   # overrides policy.min-release-age here (D15)

[skills]
"netresearch/agent-jira-skill" = "^2.0"
"acme/agents-md" = { version = "^1.0", repository = "acme" }
"acme/review" = { version = "1.4.2", repository = "acme", skills = ["review"] }

[agents]
targets = [".claude/skills", ".junie/skills"]   # default; .agents/skills is always the store
local = ".skills"                                 # default; project skills

[policy]
min-release-age = "3d"     # default; "0" disables
allow-ranges = true        # false: every version must be exact (x.y.z)
packagist = true           # false: drop the implicit Packagist repository
```

- A skill entry without `repository` is looked up in declared repositories in order, then
  Packagist. The repository that served it is written to the lock. Once locked, `install`, `update` and `add` look
  for it only in that repository; if it is gone from there, they fail rather than take a package
  with the same name from another repository. Setting `repository` moves it on purpose.
- `skills = [...]` selects skills from a package that ships several; default: all.
- `policy.allow-ranges = false` is for organisations whose standard forbids ranges even with a
  lock; `add` then writes the resolved exact version.
- GitHub entries: `{ github = true, version = "^1.3" }` resolves over version tags;
  `{ github = true, ref = "main" }` follows a branch, tag or commit; neither means the default
  branch (`HEAD`). The key is `owner/repo`, lowercase. `repository` cannot be combined with
  `github`. With `allow-ranges = false`, `ref` must be a full commit sha.

### `skills.lock`

```toml
version = 1

[[package]]
name = "acme/agents-md"
constraint = "^1.0"
version = "1.0.2"
repository = "https://git.example.com/api/v4/group/42/-/packages/composer/packages.json"
dist = { type = "zip", url = "https://…/agents-md-1.0.2.zip", reference = "9f2c1e…" }
dist-sha256 = "…"
released = 2026-10-01T09:12:00Z
skills = [{ name = "agents-md", path = "", tree-sha256 = "…" }]
```

- `reference`: `dist.reference`, else `source.reference` from the metadata (a commit sha for
  VCS-backed packages).
- `dist-sha256`: computed by skillcask at lock time. Composer's `dist.shasum` is used only as an
  extra check when present (it is empty for GitHub zipballs).
- `tree-sha256`: hash of the installed skill directory. Definition: for every regular file, sorted
  by relative path with `/` separators, the line `<path>\0<mode>\0<sha256 of content>\n`, where
  mode is `644` or `755`; the hash of the concatenation. No symlink is ever written: a
  relative symlink to a regular file inside the same skill (`CLAUDE.md -> AGENTS.md`) is installed
  and hashed as a copy of that file; any other symlink inside a skill, devices and empty
  directories are rejected at install. Symlinks outside the installed skill directories are
  ignored.
- `ref` (GitHub entries only): `true` when `constraint` is a branch, tag or commit (`ref =` in the
  manifest), `false` when it is a version constraint over tags.
- `selected`: the entry's `skills = [...]`, sorted; absent when the entry installs all skills.
- `ref` and `selected` are written only when they apply, so other entries render as before.
  Locks written before these keys existed still parse and install: an absent `selected` means
  all skills unless the manifest entry has `skills = [...]` (then the locked skill names are
  compared, as before), and an absent `ref` is not compared. A non-frozen `install` adds the
  missing keys; `--frozen` never fails only because they are missing.
- Parsing rejects (exit 1, `skills.lock: invalid …`) a hash that is not 64 lowercase hex digits,
  a dist URL that is not `https://`, a dist type other than `zip`, a package listed twice, and a
  `from` equal to the skill's `name`.
- Written atomically: a temp file in the same directory, then a rename (`skills.toml` too).
- Entries sorted by `name`; no timestamps other than `released`, so parallel branches merge.

## Package format

A skill package is a Composer package with `"type": "ai-agent-skill"`, or any package with
`extra.ai-agent-skill`:

```json
{ "name": "acme/agents-md", "type": "ai-agent-skill", "extra": { "ai-agent-skill": "SKILL.md" } }
```

`extra.ai-agent-skill` is a path to a `SKILL.md` (the skill is its directory) or a list of such
paths for packages with several skills. Missing: `SKILL.md` at the package root. The skill name
is the frontmatter `name`; it must match `[a-z0-9][a-z0-9-]*` and is the directory name under
`.agents/skills/`. Two packages providing the same skill name is an error naming both.

## Resolution

1. Read `packages.json` of each repository (a URL ending in `.json` or carrying a query string is
   used as given, otherwise `/packages.json` is appended). Take the package from the inline
   `packages` map when it is there (Composer v1: version → metadata), else follow `metadata-url`
   (`/p2/%package%.json`). Expand Composer's minified format (`"minified": "composer/2.0"`: each version inherits
   fields from the previous one; `"__unset"` removes a field).
2. Keep versions whose type is `ai-agent-skill` or that carry `extra.ai-agent-skill`.
3. Stability: stable only (`version_normalized` without a stability suffix) unless the
   constraint names one (`1.0.0-beta2`, `@beta`). `dev-*` branches are never resolved.
4. Constraints follow Composer semantics, not npm's: `^1.2` = `>=1.2.0 <2.0.0`, `^0.3` =
   `>=0.3.0 <0.4.0`, `~1.2` = `>=1.2 <2.0`, `~1.2.3` = `>=1.2.3 <1.3.0`, `1.2.*`, `>=`, `<`, `!=`,
   AND by space or comma, OR by `||`.
5. Cooldown: drop versions whose `time` is newer than `now - min-release-age`. A version without
   `time` is kept with a warning (GitLab metadata may omit it). If the cooldown removes every
   match, the error names the newest version and when it becomes eligible.
6. Pick the highest remaining version.

### GitHub sources

1. `version`: list tags (`GET /repos/{o}/{r}/tags`, up to 1,000), keep those that parse as
   versions, apply the constraint. Release time is the tag's commit date, or the publication
   time of its GitHub release when that is later (`GET /repos/{o}/{r}/releases`, first 100),
   fetched newest first until one passes the cooldown. Tags are read once per run.
2. `ref`: `GET /repos/{o}/{r}/commits/{ref}`. For a branch (`HEAD` is the default branch), the
   Activity API (`GET /repos/{o}/{r}/activity?ref={branch}`, first 100) gives push times: the
   eligible commit is the head after the newest push before the cutoff, or the head before the
   oldest listed push when all of them are newer and fewer than 100 are listed. A tag, or a
   branch without listed activity, falls back to commit dates: if the commit is newer than the
   cooldown and the ref is not a sha, take the newest commit on the ref before the cutoff
   (`commits?sha={ref}&until={cutoff}`) and say so.
3. Lock: `repository` = `<api>/repos/{o}/{r}`, `dist.url` = `<api>/repos/{o}/{r}/zipball/{sha}`,
   `reference` = the sha, `version` = the tag, or the first 12 characters of the sha for a ref.
4. Skills: every `SKILL.md` in the archive that is not inside another skill. Name: frontmatter
   `name` if valid, else the directory name (the repository name for a root `SKILL.md`); neither
   valid skips it with a warning.

Commit dates are set by the committer, so where they are the only time (a tag without a release, a
ref without listed pushes, a pinned sha) the cooldown is weaker than with registry release times;
the pinned sha and hashes still make `install` reproduce exactly what was reviewed. GitHub
zipballs are not guaranteed byte-stable; if GitHub regenerates one, `install` stops with a hash
mismatch (exit 4) rather than installing unverified content.

## Commands

| Command | Behaviour |
|---|---|
| `init` | Create `skills.toml` with defaults. |
| `add github:<owner/repo>[/skill][@version-or-ref]` | GitHub entry; `/skill` adds to `skills = [...]`. `@x` is a version constraint when it parses as one, else a ref. Without `@`: version tags if the repository has any (`^<major.minor>`), else the default branch. |
| `add <vendor/name>[@constraint] [--repository r]` | Resolve first (typo or no match leaves files untouched), write manifest and lock, install. No constraint: the existing entry's constraint, else `^<major.minor>` of the resolved version, or the exact `x.y.z` with `allow-ranges = false`. |
| `remove <vendor/name>…` | Drop from manifest and lock; remove installed directories, links and gitignore lines. |
| `install [--frozen]` | Install exactly the lock: download dist, verify `dist-sha256`, unpack to a temp dir, verify every `tree-sha256`, move into place, link, rewrite the gitignore block. Entries in the manifest that do not match the lock (missing, or a changed constraint, repository, GitHub mode, `skills` selection or `rename`) are resolved and locked; with `--frozen` (implied when `CI` is set) that is exit 1 instead. When only `skills` or `rename` changed, or the locked version still satisfies a changed constraint, the locked version is kept. Already-installed skills whose tree hash matches are skipped. |
| `update [vendor/name…]` | Re-resolve within constraints (cooldown applies), rewrite the lock, install. Entries not locked yet are resolved too; other drifted entries get a warning. |
| `outdated` | Per package: locked, newest within constraint, newest overall, eligible date under cooldown. |
| `status` | Manifest vs lock (every reason `install --frozen` checks, and a skill name provided by two packages) vs installed (hash) per skill. Exit 1 on any mismatch. |
| `doctor [--fix]` | Broken or committed managed links, stale gitignore block, modified installed skills (tree hash). `--fix` repairs without downloading. |
| `search <query>` | Packagist search API filtered by `type=ai-agent-skill`; for other repositories, a substring match over the names in `available-packages`, inline `packages` or `provider-includes` (GitLab), then the metadata of up to 30 matches to keep skill packages and show their description. A failing repository is a warning unless all fail. |
| `<other>` | Runs `skillcask-<other>` from `PATH` with the remaining arguments and exits with its code (git/cargo-style plugins). Only executables the user installed; packages never provide them. Migrations from other manifest formats live in plugins, not in the core. |
| `publish` | See Publishing. |

Exit codes: 0 ok; 1 manifest/lock/status problems or drift under `--frozen`; 2 authentication;
3 network or HTTP error; 4 hash mismatch; 5 IO or archive error.

## Installing and linking

- Store: `.agents/skills/<name>/`, replaced atomically (unpack to a sibling temp dir, verify,
  rename). Temp dirs carry the pid of their run, which holds a lock on
  `.agents/skills/.skillcask-run-<pid>` while it lives; `install` removes the ones whose run is
  gone (without a run lock, from older versions: those older than an hour).
- Each target in `agents.targets` gets `<target>/<name>` → relative path to the store.
- Managed skills (present in the lock) and their links are listed in a marked `.gitignore` block,
  regenerated on every `install`, `update`, `remove`. A managed link found in the git index is
  removed from it (`git rm --cached`), never from the working tree.
- Hand-written skills: any directory under `.agents/skills/` with a `SKILL.md` and no lock entry
  is linked into the targets too, and those links are committed (their target is tracked, so they
  resolve in every clone). A name that is both hand-written and locked is an error.
- Project skills: every `<local>/<name>/SKILL.md` (`[agents] local`, default `.skills`) gets
  `.agents/skills/<name>` → `../../<local>/<name>`, then the usual target links. All of them are
  committed. When the project skill is deleted, its links are removed on the next `install`. A
  name that is both a project skill and locked is an error.
- A name clash inside the project (two packages, a project skill, a foreign directory) is an
  error that names the `rename = { … }` fix; in a terminal `add`, `install` and `update` offer it.
  A personal skill with the same name (`~/.claude/skills/<name>`) is a warning. See D13.
- Windows without symlink permission: copy instead of link, and list the copies in the managed
  block.

## Authentication

Sources, first match per host: `COMPOSER_AUTH` (JSON), `./auth.json`, `$COMPOSER_HOME/auth.json`
(default `~/.composer/auth.json` or `~/.config/composer/auth.json`). Supported keys:
`http-basic`, `bearer`, `gitlab-token`, `gitlab-oauth`, `github-oauth` (a `github.com` token is
also used for `api.github.com`). `GITHUB_TOKEN` or `GH_TOKEN` is the last fallback for the hosts
of the GitHub APIs the user or the runner names: `https://api.github.com`, `SKILLCASK_GITHUB_API`
and GitHub Actions' `GITHUB_API_URL`, never a `github` url from `skills.toml` (D21); such a host
needs `github-oauth` in `auth.json` or `skillcask auth set <host> --username oauth2`. Then git's
credential helper (D17): `git credential fill` once per remaining host, never interactive (no
terminal prompt, no askpass, `GCM_INTERACTIVE=never`), off with `SKILLCASK_GIT_CREDENTIALS=0`. The username picks the
header: `PRIVATE-TOKEN` the GitLab header, `oauth2` a Bearer token, any other HTTP Basic.
`skillcask auth set <host> [--username U]` stores a token (asked by git without echo, or read from
a pipe) and `auth remove` deletes only that username's entry. A GitLab CI job reads the registry with
`http-basic` user `gitlab-ci-token`, password `$CI_JOB_TOKEN`. Tokens are never printed, including
in `--verbose` URLs.

## Security

- HTTPS only; `http://` repositories and dist URLs are rejected. TLS through the platform
  verifier (system trust store), so corporate CAs work without flags; no option to disable
  verification.
- Archives: reject absolute paths, `..`, symlinks leaving their skill, hard links and devices; limits on download
  size (50 MiB), unpacked size (100 MiB) and file count (5,000), overridable per command.
- Archive names must work on every OS: no Windows-reserved names (`CON`, `PRN`, `AUX`, `NUL`,
  `COM1`-`COM9`, `LPT1`-`LPT9`, also with an extension), no trailing dot or space, no
  `<>:"|?*\` or control characters, and no two paths that are one name on a case-insensitive
  or Unicode-normalising file system (`README.md` and `readme.md`, `café` composed and
  decomposed). Such entries are not extracted. A selected skill that contains one fails with
  the same error on every OS; outside the selected skills they are skipped, so a stray file in
  a repository does not block its skills. Duplicate entries and mismatched zip headers fail the
  whole archive, and the limits count every entry.
- No package code runs at any point.
- Release artefacts: checksums and build provenance attestations; `skillcask --version` prints
  the commit.

## Publishing

**One repository per skill**: a `composer.json` with the convention above and semver tags.
Packagist reads public repositories; GitLab publishes a tag to its Composer registry
(`POST /projects/:id/packages/composer`). skillcask has no part in it.

**Monorepo**: `skillcask publish --vendor acme --base-url <url> --out <dir> [paths…]` finds
every `SKILL.md` with frontmatter `name` and `version`, and writes a static Composer repository:

```
<dir>/packages.json            # metadata-url: <base-url>/p2/%package%.json, available-packages
<dir>/p2/acme/<name>.json      # all published versions, not minified
<dir>/dist/acme/<name>/<version>.zip
```

- Package name: frontmatter `package:` when set, else `<vendor>/<name>`. The directory a skill sits
  in plays no part, so nested folders (`skills/team/lang/<name>`) give `<vendor>/<name>` too. One
  package comes from one directory: two `SKILL.md` files with the same package name are an error,
  fixed with `package:` in one of them.
- Zips are deterministic (sorted entries, fixed mtime, fixed permissions), so the same tree gives
  the same `shasum`, which `publish` writes into the metadata.
- Versions are immutable: `--out` may hold a previous run; an existing version with a different
  hash is an error, an identical one is kept. Without its zip in `--out`, a version is identical
  when its tree matches `extra.ai-agent-skill-tree` (D14); versions published by an older skillcask get
  that key on the first run whose zip still matches.
- `time` is the publish time of the version, set once, truncated to the second.
- `--dist-url <template>` sets the zip URL of new versions with `{vendor}`, `{name}` and
  `{version}`; default `<base-url>/dist/{vendor}/{name}/{version}.zip`. Zips are written to
  `dist/` either way; uploading them to the template URL is the publisher's job.
- `--inline` (D12) writes all packages into `packages.json` as `packages.<name>.<version>` and no
  `p2/`; `--base-url` is then optional when `--dist-url` is given. Earlier versions are read from
  either layout, so switching keeps them.

**Private hosting on GitLab** (monorepo): CI runs `publish --inline` with a Generic Package
Registry `--dist-url` (`.../packages/generic/{vendor}-{name}/{version}/{name}-{version}.zip`),
uploads new zips, and commits `packages.json` to the repository, which also keeps the state the
next run needs. Clients use the raw files API URL
(`.../repository/files/<path-encoded>/raw?ref=<branch>`) as the repository `url`; a
`gitlab-token` (sent as `PRIVATE-TOKEN`) for the host covers both requests.

## Testing

- Unit: constraint parser (table of Composer's own test cases for `^`, `~`, `*`, stability),
  minified metadata expansion, tree hash, lock round-trip, gitignore block rewrite.
- Integration: a local HTTPS server (test CA) serving a fixture Composer repository, covering
  install, frozen drift, hash mismatch (exit 4), cooldown, auth (`http-basic`, `bearer`),
  archive attacks (zip slip, symlink, size limit), collisions, Windows copy mode (on the Windows
  runner).
- `publish` → `install` round trip against the generated static repository, in both layouts.
- End to end against Packagist with `netresearch/agent-jira-skill`, `#[ignore]` by default.

## Distribution

GitHub repository `skillcask`; releases via `cargo-dist` for macOS (arm64, x86_64), Linux
(x86_64, arm64, musl) and Windows; Homebrew tap; `cargo install skillcask`; checksums and GitHub
artefact attestations. The crate name `skillcask` was free on crates.io on 2026-10-19.

## Acceptance

- `skillcask add netresearch/agent-jira-skill` in an empty git repository creates `skills.toml`,
  `skills.lock`, `.agents/skills/<name>/`, links in `.claude/skills` and `.junie/skills`, and a
  gitignore block; a fresh clone plus `skillcask install --frozen` reproduces the same tree hash.
- Changing one byte of an installed skill makes `status` exit 1; tampering with the served zip
  makes `install` exit 4.
- With `CI=true`, a manifest entry missing from the lock makes `install` exit 1 without writing
  anything.
- A version published 1 hour ago is not selected by `add` with the default policy, and the error
  says when it becomes eligible.
- `publish` on a monorepo with three skills produces a repository that `install` consumes, and a
  second `publish` of unchanged skills changes no file.

## Open questions

1. ~~Private static repository hosting for monorepos.~~ Decided in D12: inline `packages.json`
   in git, zips in the Generic Package Registry. Rejected: GitLab's Composer registry (one package
   per project), GitLab Pages (cookie auth, a CLI cannot authenticate), `p2/` in the Generic
   Package Registry (no `/` in file names), an object store behind a token proxy (new
   infrastructure).
2. ~~Whether `search` should query several Composer repositories at once or only Packagist.~~ It
   queries all of them; see `search` above.
