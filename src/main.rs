#![forbid(unsafe_code)]

mod archive;
mod commands;
mod credential;
mod deps;
mod diff;
mod error;
mod github;
mod http;
mod lock;
mod manifest;
mod policy;
mod project;
mod publish;
mod registry;
mod scope;
mod skill;
mod ui;
mod version;

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::archive::Limits;
use crate::commands::Ctx;
use crate::error::{Error, IO, USAGE};
use crate::scope::Scope;

const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("SKILLCASK_COMMIT"), ")");

/// A lockfile-first package manager for AI agent skills.
#[derive(Parser)]
#[command(name = "skillcask", version = VERSION)]
struct Cli {
    /// Print every HTTP request (credentials are never printed).
    #[arg(short, long, global = true)]
    verbose: bool,
    /// Manage personal skills in ~/.agents/skills instead of the project's.
    #[arg(short, long, global = true)]
    global: bool,
    /// Terse output for coding agents: no colour, no padding, problems only. Also SKILLCASK_AGENT=1;
    /// detected from AI_AGENT or CLAUDECODE unless SKILLCASK_AGENT=0.
    #[expect(clippy::doc_markdown, reason = "clap prints this doc comment as help text; backticks would show")]
    #[arg(long, global = true)]
    agent: bool,
    /// Maximum download size, e.g. 50M.
    #[arg(long, global = true, value_parser = parse_size)]
    max_download: Option<u64>,
    /// Maximum unpacked size of one archive, e.g. 100M.
    #[arg(long, global = true, value_parser = parse_size)]
    max_unpacked: Option<u64>,
    /// Maximum number of files in one archive.
    #[arg(long, global = true)]
    max_files: Option<usize>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum AuthCmd {
    /// Store a token for a host; asked without echo, or read from stdin when piped.
    Set {
        /// host or host:port
        host: String,
        /// PRIVATE-TOKEN sends a GitLab PRIVATE-TOKEN header, oauth2 a Bearer token, any other
        /// name HTTP Basic.
        #[arg(long, default_value = credential::PRIVATE_TOKEN)]
        username: String,
    },
    /// Remove the stored token of a host.
    Remove {
        host: String,
        #[arg(long, default_value = credential::PRIVATE_TOKEN)]
        username: String,
    },
}

#[derive(Subcommand)]
enum Cmd {
    /// Create skills.toml with defaults.
    Init,
    /// Resolve a package, add it to skills.toml and skills.lock, install it.
    Add {
        /// vendor/name[@constraint], or github:owner/repo[/skill][@version-or-ref]
        package: String,
        /// Repository name from [repositories].
        #[arg(long)]
        repository: Option<String>,
    },
    /// Remove packages from skills.toml and skills.lock, with their files and links.
    Remove {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Install exactly what skills.lock pins.
    Install {
        /// Fail instead of resolving when skills.lock does not match skills.toml (implied by CI).
        #[arg(long)]
        frozen: bool,
    },
    /// Re-resolve within constraints and rewrite skills.lock.
    Update {
        packages: Vec<String>,
        /// Print a unified diff of each updated or added package's skill files; it does not ask
        /// for confirmation (review with `skillcask diff` first).
        #[arg(long)]
        diff: bool,
    },
    /// Show newer versions per package.
    Outdated {
        /// Exit 1 when `update` would lock a newer version within a package's constraint.
        #[arg(long)]
        strict: bool,
    },
    /// Unified diff of skill files from the locked version to the one `update` would lock.
    Diff {
        /// One package (also a dependency); default: every entry of skills.toml with a newer version.
        package: Option<String>,
        /// Compare with the newest version matching this instead, cooldown not applied.
        #[arg(long)]
        to: Option<String>,
    },
    /// A package's skills, versions, description, license and requirements; for an installed
    /// skill name, which package and skills.toml entry install it.
    Info {
        /// vendor/name[@constraint], github:owner/repo[@version-or-ref], or a skill name
        target: String,
    },
    /// Compare skills.toml, skills.lock and installed files; exit 1 on any mismatch.
    Status,
    /// Check links, the gitignore block and installed files.
    Doctor {
        /// Repair links, the git index and the gitignore block (never downloads).
        #[arg(long)]
        fix: bool,
    },
    /// Search for skill packages.
    Search { query: String },
    /// Keep a registry token in git's credential helper (Keychain, Credential Manager, libsecret).
    #[command(subcommand)]
    Auth(AuthCmd),
    /// Build a static Composer repository from SKILL.md files in a monorepo.
    Publish {
        #[arg(long)]
        vendor: String,
        /// Public https:// URL the output directory will be served from.
        #[arg(long)]
        base_url: Option<String>,
        /// Dist URL template with {vendor}, {name} and {version}.
        #[arg(long)]
        dist_url: Option<String>,
        /// Write all packages into packages.json instead of metadata-url and p2/.
        #[arg(long)]
        inline: bool,
        #[arg(long)]
        out: PathBuf,
        paths: Vec<PathBuf>,
    },
    /// Check skills against the Agent Skills SKILL.md spec; warn about invisible Unicode.
    Validate {
        /// Skill directories, or directories to search for SKILL.md (default: the current one).
        paths: Vec<PathBuf>,
    },
    /// Any other command runs `skillcask-<command>` from PATH, like git and cargo plugins.
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mult) = match s.char_indices().last() {
        Some((i, 'K' | 'k')) => (&s[..i], 1 << 10),
        Some((i, 'M' | 'm')) => (&s[..i], 1 << 20),
        Some((i, 'G' | 'g')) => (&s[..i], 1 << 30),
        _ => (s, 1),
    };
    num.parse::<u64>().ok().and_then(|n| n.checked_mul(mult)).ok_or_else(|| format!("invalid size {s:?}, use e.g. 50M"))
}

fn ci() -> bool {
    std::env::var("CI").is_ok_and(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
}

fn run(cli: Cli) -> Result<(), Error> {
    let d = Limits::default();
    let limits = Limits {
        download: cli.max_download.unwrap_or(d.download),
        unpacked: cli.max_unpacked.unwrap_or(d.unpacked),
        files: cli.max_files.unwrap_or(d.files),
    };
    if cli.global && matches!(cli.cmd, Cmd::External(_) | Cmd::Publish { .. } | Cmd::Auth(_) | Cmd::Validate { .. }) {
        return Err(Error::new(USAGE, "--global does not apply to this command"));
    }
    if let Cmd::External(args) = &cli.cmd {
        return plugin(args);
    }
    if let Cmd::Auth(a) = &cli.cmd {
        return auth(a);
    }
    if let Cmd::Validate { paths } = &cli.cmd {
        return publish::validate(paths);
    }
    let scope = if cli.global {
        Scope::global()?
    } else {
        Scope::project(match cli.cmd {
            Cmd::Init => {
                let cwd = std::env::current_dir()?;
                project::check_root(&cwd)?;
                cwd
            }
            Cmd::Publish { .. } => std::env::current_dir()?,
            _ => project::find_root()?,
        })
    };
    let interactive = !ci() && std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let mut ctx = Ctx::new(scope, cli.verbose, limits, interactive);
    ctx.read_only = matches!(cli.cmd, Cmd::Info { .. } | Cmd::Diff { .. });
    match cli.cmd {
        Cmd::Init => commands::init(&ctx),
        Cmd::Add { package, repository } => commands::add(&ctx, &package, repository),
        Cmd::Remove { packages } => commands::remove(&ctx, &packages),
        Cmd::Install { frozen } => commands::install(&ctx, frozen || ci()),
        Cmd::Update { packages, diff } => commands::update(&ctx, &packages, diff),
        Cmd::Outdated { strict } => commands::outdated(&ctx, strict),
        Cmd::Diff { package, to } => commands::diff(&ctx, package.as_deref(), to.as_deref()),
        Cmd::Info { target } => commands::info(&ctx, &target),
        Cmd::Status => commands::status(&ctx),
        Cmd::Doctor { fix } => commands::doctor(&ctx, fix),
        Cmd::Search { query } => commands::search(&ctx, &query),
        Cmd::External(_) | Cmd::Auth(_) | Cmd::Validate { .. } => unreachable!("handled above"),
        Cmd::Publish { vendor, base_url, dist_url, inline, out, paths } => {
            if vendor.is_empty() {
                return Err(Error::new(USAGE, "--vendor must not be empty"));
            }
            publish::publish(&publish::PublishArgs { vendor, base_url, dist_url, inline, out, paths })
        }
    }
}

fn auth(cmd: &AuthCmd) -> Result<(), Error> {
    let (AuthCmd::Set { host, username } | AuthCmd::Remove { host, username }) = cmd;
    // Accepts a URL too; git's helpers key credentials by host and port.
    let url = url::Url::parse(host).or_else(|_| url::Url::parse(&format!("https://{host}")));
    let Some(key) = url
        .ok()
        .filter(|u| u.scheme() == "https")
        .and_then(|u| Some(u.host_str()?.to_string() + &u.port().map(|p| format!(":{p}")).unwrap_or_default()))
    else {
        return Err(Error::new(USAGE, format!("invalid host {host:?}")));
    };
    match cmd {
        AuthCmd::Set { .. } => {
            let piped = !std::io::stdin().is_terminal();
            let token = if piped { Some(std::io::read_to_string(std::io::stdin())?.trim().to_string()) } else { None };
            let (chars, helpers) = credential::store(&key, username, token.as_deref())?;
            ui::done("stored", format!("a {chars}-character token for {key} with git's credential helper ({helpers})"));
        }
        AuthCmd::Remove { .. } => credential::remove(&key, username)?,
    }
    Ok(())
}

/// Runs a user-installed `skillcask-<name>` executable. Packages never provide these.
fn plugin(args: &[OsString]) -> Result<(), Error> {
    let name = args[0].to_string_lossy();
    // A separator would make Command run a path relative to the cwd instead of searching PATH.
    if name.is_empty() || name.contains(['/', '\\']) {
        return Err(Error::new(USAGE, format!("unknown command {name:?}")));
    }
    let exe = format!("skillcask-{name}");
    let status = std::process::Command::new(&exe).args(&args[1..]).status().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::new(USAGE, format!("unknown command {name:?} (no {exe} on PATH)"))
        } else {
            Error::new(IO, format!("{exe}: {e}"))
        }
    })?;
    #[cfg(unix)]
    if let Some(sig) = std::os::unix::process::ExitStatusExt::signal(&status) {
        std::process::exit(128 + sig);
    }
    std::process::exit(status.code().unwrap_or(1));
}

fn main() {
    // clap exits 2 on usage errors, which here means authentication.
    let cli = Cli::try_parse().unwrap_or_else(|e| {
        let _ = e.print();
        std::process::exit(if e.use_stderr() { USAGE.exit() } else { 0 });
    });
    ui::init(cli.agent);
    if let Err(e) = run(cli) {
        ui::error(&e.msg, e.hint.as_deref());
        std::process::exit(e.code.exit());
    }
}
