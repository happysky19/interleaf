//! Who is writing, which project, the project's rules, and tokens.
//!
//! Nothing has to be configured. A project is named per call by its Overleaf link or id or a git
//! URL, or is the current folder when that is a clone of one; tokens come from the environment
//! or the system keychain. An optional personal file names projects and overrides settings:
//!
//! ```toml
//! # ~/.config/interleaf/config.toml
//! name = "Your Name"
//! initials = "YN"
//! default = "paper"
//! [projects.paper]
//! id = "<id from the Overleaf URL>"   # or git_url = "git@github.com:you/paper.git"
//! dir = "~/papers/paper"
//! results = "~/research/paper"
//! mode = "direct"
//! ```
//!
//! Rules shared by everyone on a paper live in its repository, in `interleaf.toml`
//! (journal, mode, protected, main, engine, max_file_mb, [lint] extra / ignore).
use crate::text::{re, read};
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const PROJECT_FILE: &str = "interleaf.toml";

pub struct Token {
    pub env: &'static str,
    service: &'static str,
    what: &'static str,
}

pub const OVERLEAF_TOKEN: Token = Token {
    env: "OVERLEAF_GIT_TOKEN",
    service: "overleaf-git-token",
    what: "Overleaf git token (Overleaf: Account Settings > Git Integration)",
};
pub const ADS_TOKEN: Token = Token {
    env: "ADS_TOKEN",
    service: "ads-api-token",
    what: "ADS API token (ui.adsabs.harvard.edu: Account > Settings > API Token)",
};

impl Token {
    fn keychain(&self) -> [&str; 5] {
        if cfg!(target_os = "macos") {
            ["/usr/bin/security", "find-generic-password", "-s", self.service, "-w"]
        } else {
            ["secret-tool", "lookup", "service", self.service, ""]
        }
    }

    fn lookup(&self) -> Vec<&str> {
        self.keychain().into_iter().filter(|a| !a.is_empty()).collect()
    }

    pub fn get(&self) -> Option<String> {
        if let Some(v) = env(self.env) {
            return Some(v);
        }
        let cmd = self.lookup();
        let out = Command::new(cmd[0]).args(&cmd[1..]).output().ok()?;
        let v = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        (out.status.success() && !v.is_empty()).then_some(v)
    }

    pub fn missing(&self) -> anyhow::Error {
        let store = if cfg!(target_os = "macos") {
            format!("security add-generic-password -U -a \"$USER\" -s {} -w", self.service)
        } else {
            format!("secret-tool store --label={0} service {0}", self.service)
        };
        anyhow!(
            "no {}: set {}, or store it in the keychain with\n  {store}\n(it prompts for the token)",
            self.what,
            self.env
        )
    }

    /// A git credential helper that reads the token when git asks for it, so the token never
    /// appears in arguments, URLs or git config.
    pub fn helper(&self) -> String {
        let value = match env(self.env) {
            Some(_) => format!("${}", self.env),
            None => format!("$({})", self.lookup().join(" ")),
        };
        format!(r#"!f() {{ test "$1" = get || return 0; echo username=git; echo "password={value}"; }}; f"#)
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

#[derive(Deserialize)]
#[serde(default)]
pub struct Settings {
    pub mode: String,
    pub journal: Option<String>,
    pub main: Option<String>,
    pub engine: String,
    pub protected: Vec<String>,
    pub max_file_mb: f64,
    pub lint: Lint,
    pub branch: Option<String>,
    pub commit_trailer: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct Lint {
    pub extra: Vec<String>,
    pub ignore: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            mode: "suggest".into(),
            journal: None,
            main: None,
            engine: "pdflatex".into(),
            protected: vec![],
            max_file_mb: 20.0,
            lint: Lint::default(),
            branch: None,
            commit_trailer: None,
        }
    }
}

pub struct Project {
    pub name: String,
    /// Overleaf project id; None for other git hosts.
    pub id: Option<String>,
    pub dir: PathBuf,
    pub git_url: String,
    pub results: Option<PathBuf>,
    pub user: String,
    pub initials: String,
    personal: toml::Table,
}

impl Project {
    /// The project's interleaf.toml overlaid by the personal entry.
    pub fn settings(&self) -> Result<Settings> {
        let file = self.dir.join(PROJECT_FILE);
        let mut table: toml::Table = if file.is_file() {
            toml::from_str(&read(&file)?).with_context(|| format!("{PROJECT_FILE} in {}", self.name))?
        } else {
            toml::Table::new()
        };
        table.extend(self.personal.clone());
        toml::Value::Table(table)
            .try_into()
            .with_context(|| format!("settings of {}", self.name))
    }

    /// Overleaf over https uses the Overleaf token; any other remote uses the user's git login.
    pub fn overleaf_https(&self) -> bool {
        self.id.is_some() && self.git_url.starts_with("https://")
    }

    /// What people call the shared copy.
    pub fn upstream(&self) -> &'static str {
        if self.id.is_some() { "Overleaf" } else { "the remote" }
    }

    pub fn web(&self) -> String {
        match &self.id {
            Some(id) if self.git_url.contains("git.overleaf.com") => format!("https://www.overleaf.com/project/{id}"),
            _ => self.git_url.clone(),
        }
    }

    /// Where interleaf keeps its own files for this clone (build, review, figure sources).
    pub fn state(&self) -> PathBuf {
        self.dir.join(".git/interleaf")
    }
}

pub fn config_path() -> PathBuf {
    std::env::var_os("INTERLEAF_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config/interleaf/config.toml"))
}

fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_default()
}

fn expand(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(path),
    }
}

/// Folder that holds clones made for links given per call.
fn workspace() -> PathBuf {
    std::env::var_os("INTERLEAF_WORKSPACE")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").current_dir(dir).args(args).output().ok()?;
    let v = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !v.is_empty()).then_some(v)
}

fn same_remote(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim_end_matches('/').trim_end_matches(".git").to_owned();
    norm(a) == norm(b)
}

/// An Overleaf id, or a git URL, read from what the user gave.
fn remote(s: &str) -> Option<(Option<String>, String)> {
    if let Some(c) = re!(r"^([0-9a-f]{24})$|overleaf\.com/(?:project/)?([0-9a-f]{24})").captures(s) {
        let id = c.get(1).or(c.get(2)).unwrap().as_str().to_owned();
        let url = if s.starts_with("https://git.") || s.starts_with("http://") {
            s.to_owned()
        } else {
            format!("https://git.overleaf.com/{id}")
        };
        return Some((Some(id), url));
    }
    let git =
        s.contains("://") || re!(r"^[\w.-]+@[\w.-]+:").is_match(s) || s.ends_with(".git") || Path::new(s).is_dir();
    git.then(|| (None, s.to_owned()))
}

struct Who {
    name: String,
    initials: String,
}

fn who(cfg: &toml::Table) -> Who {
    let name = cfg
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .or_else(|| git_out(&workspace(), &["config", "user.name"]))
        .unwrap_or_else(|| "me".into());
    let initials = cfg
        .get("initials")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            name.split(|c: char| c.is_whitespace() || c == '-')
                .filter_map(|w| w.chars().next())
                .take(3)
                .collect::<String>()
                .to_uppercase()
        });
    Who { name, initials }
}

fn make(name: &str, id: Option<String>, git_url: String, dir: PathBuf, entry: &toml::Table, who: &Who) -> Project {
    Project {
        name: name.to_owned(),
        id,
        dir,
        git_url,
        results: entry.get("results").and_then(|v| v.as_str()).map(expand),
        user: who.name.clone(),
        initials: who.initials.clone(),
        personal: entry.clone(),
    }
}

/// The current folder, when it is a clone of an Overleaf project or carries interleaf.toml.
fn here(who: &Who) -> Option<Project> {
    let cwd = std::env::current_dir().ok()?;
    let top = PathBuf::from(git_out(&cwd, &["rev-parse", "--show-toplevel"])?);
    let url = git_out(&top, &["config", "--get", "remote.origin.url"])?;
    let (id, url) = remote(&url).unwrap_or((None, url));
    if id.is_none() && !top.join(PROJECT_FILE).is_file() {
        return None;
    }
    let name = top.file_name()?.to_string_lossy().into_owned();
    Some(make(&name, id, url, top, &toml::Table::new(), who))
}

/// The project a tool call is about: a configured name, an Overleaf link or id, a git URL;
/// otherwise the configured default, OVERLEAF_PROJECT_ID, the current folder, or the only
/// configured project.
pub fn project(arg: Option<&str>) -> Result<Project> {
    let path = config_path();
    let cfg: toml::Table = if path.is_file() {
        toml::from_str(&read(&path)?).with_context(|| path.display().to_string())?
    } else {
        toml::Table::new()
    };
    let projects = cfg
        .get("projects")
        .and_then(|v| v.as_table())
        .cloned()
        .unwrap_or_default();
    let who = who(&cfg);
    let wanted = arg
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| cfg.get("default").and_then(|v| v.as_str()).map(str::to_owned))
        .or_else(|| env("OVERLEAF_PROJECT_ID"));
    let known = || projects.keys().cloned().collect::<Vec<_>>().join(", ");
    let name = match wanted {
        Some(w) if projects.contains_key(&w) => w,
        Some(w) => {
            let (id, url) = remote(&w).ok_or_else(|| {
                anyhow!(
                    "unknown project {w:?}: give an Overleaf link or id, a git URL, or one of: {}",
                    known()
                )
            })?;
            let cwd = std::env::current_dir()?;
            let top = git_out(&cwd, &["rev-parse", "--show-toplevel"]).map(PathBuf::from);
            let dir = match top {
                Some(top)
                    if git_out(&top, &["config", "--get", "remote.origin.url"])
                        .is_some_and(|u| same_remote(&u, &url)) =>
                {
                    top
                }
                _ => {
                    let base = id.clone().unwrap_or_else(|| {
                        url.trim_end_matches('/')
                            .trim_end_matches(".git")
                            .rsplit(['/', ':'])
                            .next()
                            .unwrap_or("paper")
                            .to_owned()
                    });
                    workspace().join(base)
                }
            };
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            return Ok(make(&name, id, url, dir, &toml::Table::new(), &who));
        }
        None => {
            if let Some(p) = here(&who) {
                return Ok(p);
            }
            match projects.keys().next() {
                Some(only) if projects.len() == 1 => only.clone(),
                _ => bail!(
                    "say which project: an Overleaf link or id, a git URL, or one of: {}",
                    known()
                ),
            }
        }
    };
    let entry = projects[&name]
        .as_table()
        .ok_or_else(|| anyhow!("projects.{name} must be a table"))?;
    let get = |k: &str| entry.get(k).and_then(|v| v.as_str()).map(str::to_owned);
    let id = get("id");
    let git_url = get("git_url").or_else(|| id.as_ref().map(|id| format!("https://git.overleaf.com/{id}")));
    let Some(git_url) = git_url else {
        bail!("project {name}: give an Overleaf id or a git_url")
    };
    let dir = get("dir")
        .map(|d| expand(&d))
        .unwrap_or_else(|| workspace().join(&name));
    Ok(make(&name, id, git_url, dir, entry, &who))
}
