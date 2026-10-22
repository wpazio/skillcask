# Spec: source policy and default sources

Date: 2026-10-20. Status: implemented.

## Problem

An organisation that may only install skills from its private registry has two switches today:
`policy.packagist = false` and leaving out `github = true`. Nothing stops a project, a dependency
(D16) or a committed lock from pulling from another host, and nothing applies one rule to every
project on a machine or in CI. The default sources (Packagist, the GitHub API) are implicit, so a
reader of `skills.toml` does not see them.

## Goals

1. `allow-sources` lists the only sources skills may come from; everything else is refused at
   resolution and at install from the lock.
2. Policy comes in layers: environment, user config, project. The user config can fix a key so a
   project cannot change it, or only provide a default.
3. `init` lists the default sources in `[repositories]`.

## Non-goals

- Repositories shared through the user config (one private registry declared for all projects).
- A system-wide config (`/etc`).

## Layers

Keys: `min-release-age`, `allow-ranges`, `packagist`, `github-requires`, `allow-sources`. The
first layer that sets a key wins:

1. Environment: `SKILLCASK_MIN_RELEASE_AGE`, `SKILLCASK_ALLOW_RANGES`, `SKILLCASK_PACKAGIST`,
   `SKILLCASK_GITHUB_REQUIRES`, `SKILLCASK_ALLOW_SOURCES` (comma-separated). Empty is unset.
2. User config `config.toml` in the config dir of D13 (`$SKILLCASK_HOME`,
   `$XDG_CONFIG_HOME/skillcask`, `~/.config/skillcask`), keys listed in `enforce`. A project that
   sets an enforced key to another value is exit 1 naming the key and the file, unless the
   environment sets the key: then the project's value is unused. An environment value that
   differs from an enforced one wins, with one warning per run on stderr naming the variable,
   both values and the file.
3. `[policy]` of the project (or global) `skills.toml`.
4. User config, keys not in `enforce`.
5. Built-in defaults.

```toml
# ~/.config/skillcask/config.toml
[policy]
allow-sources = ["https://gitlab.example.com/", "github"]
min-release-age = "3d"
enforce = ["allow-sources"]
```

An enforced `min-release-age` also binds the per-repository values of D15 (D28): a project's
`min-release-age` on a repository may be longer, never shorter (exit 1 naming the repository);
a repository without one gets the enforced value. `config.toml` lists the exceptions by
`https://` URL prefix, longest match wins, and only next to `enforce = ["min-release-age"]`:

```toml
[policy.repository-min-release-age]
"https://gitlab.example.com/" = "0"   # reviewed in merge requests
```

Only `[policy]` is read from `config.toml`; any other table is exit 1, so a typo does not
silently drop a rule. CI without the file applies the project and the environment.

## allow-sources

Absent: every source is allowed. Each entry is one of:

- `packagist`: `https://repo.packagist.org`;
- `github`: the GitHub API in use (`<api>/repos/…`);
- an `https://` URL prefix, matched on a `/` boundary (`https://api.github.com/repos/acme/`
  allows the GitHub organisation `acme` only);
- in `skills.toml` only: a repository name from `[repositories]`, matching its URL. In
  `config.toml` and the environment a name would mean whatever a project calls that name, so
  only the forms above are accepted.

A source is the URL a lock entry records in `repository`. Enforcement:

- Lookup searches allowed repositories only; blocked ones get no request. An entry whose
  `repository = "…"` or locked repository is blocked is exit 1, not a fall-through to another
  repository.
- GitHub sources are checked before the first API request.
- Every command that writes or installs checks every lock entry: a package from a blocked source
  is exit 1, also under `install --frozen`.
- Dependencies (D16) follow the same rules.

## Default sources

`init` writes them:

```toml
[repositories]
packagist = { url = "https://repo.packagist.org" }
github = { type = "github", url = "https://api.github.com" }
```

- `packagist = false` and `github = false` turn a default source off. A missing line keeps it,
  so manifests written before this change behave as before; `allow-sources` is the hard block.
- Packagist is searched last wherever its line stands, so a private package is never shadowed
  by a public one with the same name.
- `type = "github"` is accepted only on the key `github`. Its `url` is the API base (GitHub
  Enterprise); `SKILLCASK_GITHUB_API` still overrides it. `min-release-age` is not accepted
  there; GitHub sources use the policy value (D15).
- `policy.packagist = false` keeps working.

## status

After the table, `status` prints each policy key that does not come from the built-in defaults,
with its layer: `allow-sources = ["acme"] (config.toml, enforced)`.

## Testing

- Unit: layer precedence for each layer, enforce conflict, env parsing, URL prefix boundary
  (`https://git.example.com` does not allow `https://git.example.com.evil/`), names refused in
  `config.toml`, Packagist ordered last, `github = false`.
- Integration: allow-sources limited to a private repository blocks a Packagist-only package
  without a request to it; a lock entry from a blocked source fails `install --frozen`; an
  enforced key in `config.toml` beats the project and the environment beats both; a dependency
  from a blocked source fails; `status` shows the origin.
