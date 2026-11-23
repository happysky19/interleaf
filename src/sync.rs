//! Clone, pull and push a project through git.
//!
//! Edits stay local until push. A pull with local work first checks for conflicts in memory and,
//! unlike git, accepts edits on adjacent lines (two people writing prose do this constantly).
//! Push pulls first, never forces, and refuses conflict markers, deletions and protected files
//! unless told otherwise.
use crate::config::{OVERLEAF_TOKEN, Project};
use crate::text::{self, opcodes, re, tail};
use anyhow::{Result, bail};
use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

const EXCLUDE: &str =
    ".DS_Store\n*.aux\n*.log\n*.out\n*.fls\n*.fdb_latexmk\n*.synctex.gz\n*.blg\n*.bcf\n*.run.xml\n*~\n*.swp\n";
const TEXT_SUFFIXES: [&str; 6] = ["tex", "bib", "txt", "sty", "cls", "toml"];

pub fn redact(s: &str) -> String {
    re!(r"olp_[A-Za-z0-9]+").replace_all(s, "olp_***").into_owned()
}

/// Rows of a tool's answer, printed as `key: value` with the state first.
#[derive(Default)]
pub struct Report {
    pub state: String,
    rows: Vec<(String, String)>,
}

impl Report {
    pub fn new(state: &str) -> Report {
        Report {
            state: state.into(),
            rows: vec![],
        }
    }

    pub fn set(&mut self, key: &str, value: impl fmt::Display) {
        self.rows.push((key.into(), value.to_string()));
    }

    pub fn list<T: fmt::Display>(&mut self, key: &str, items: impl IntoIterator<Item = T>) {
        let items: Vec<String> = items.into_iter().map(|x| format!("\n  {x}")).collect();
        self.rows.push((
            key.into(),
            if items.is_empty() {
                " none".into()
            } else {
                items.concat()
            },
        ));
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.rows.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn with_state(mut self, state: impl Into<String>) -> Report {
        self.state = state.into();
        self
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut out = vec![];
        if !self.state.is_empty() {
            out.push(format!("state: {}", self.state));
        }
        for (k, v) in &self.rows {
            out.push(match v.starts_with(['\n', ' ']) {
                true => format!("{k}:{v}"),
                false if v.contains('\n') => format!("{k}:\n{v}"),
                false => format!("{k}: {v}"),
            });
        }
        f.write_str(&out.join("\n"))
    }
}

/// Three-way line merge. Conflict only when both sides change overlapping base lines
/// differently, insert different text at one point, or one inserts inside the other's block.
pub fn merge3(base: &[&str], ours: &[&str], theirs: &[&str]) -> Option<String> {
    type Edit = (usize, usize, Vec<String>);
    let edits = |new: &[&str]| -> Vec<Edit> {
        opcodes(base, new)
            .into_iter()
            .filter(|(equal, ..)| !equal)
            .map(|(_, o, n)| (o.start, o.end, new[n].iter().map(|s| s.to_string()).collect()))
            .collect()
    };
    let a = edits(ours);
    let b: Vec<Edit> = edits(theirs).into_iter().filter(|e| !a.contains(e)).collect();
    let clash = |x: &Edit, y: &Edit| {
        let ((x1, x2), (y1, y2)) = ((x.0, x.1), (y.0, y.1));
        if x1.max(y1) < x2.min(y2) {
            return true;
        }
        if x1 == x2 && y1 == y2 {
            return x1 == y1;
        }
        (x1 == x2 && y1 < x1 && x1 < y2) || (y1 == y2 && x1 < y1 && y1 < x2)
    };
    if a.iter().any(|x| b.iter().any(|y| clash(x, y))) {
        return None;
    }
    let mut all: Vec<&Edit> = a.iter().chain(&b).collect();
    all.sort_by_key(|e| (e.0, e.1 != e.0));
    let (mut out, mut pos) = (String::new(), 0);
    for (i1, i2, rep) in all {
        out.extend(base[pos..*i1].iter().copied());
        out.extend(rep.iter().map(String::as_str));
        pos = *i2;
    }
    out.extend(base[pos..].iter().copied());
    Some(out)
}

pub type Rows = Vec<(char, String)>;
pub type Transform<'a> = &'a dyn Fn(&Rows) -> Result<BTreeMap<String, String>>;

pub struct Push<'a> {
    pub message: &'a str,
    pub dry_run: bool,
    pub transform: Option<Transform<'a>>,
    pub allow_deletions: bool,
    pub protected: &'a [String],
    pub max_mb: f64,
    pub trailer: String,
}

pub struct Repo {
    pub p: Project,
    branch: OnceCell<String>,
}

impl Repo {
    pub fn new(p: Project, branch: Option<String>) -> Repo {
        let cell = OnceCell::new();
        if let Some(b) = branch {
            let _ = cell.set(b);
        }
        Repo { p, branch: cell }
    }

    pub fn dir(&self) -> &Path {
        &self.p.dir
    }

    fn run(&self, cwd: &Path, args: &[&str], env: &[(&str, &str)], input: Option<&str>) -> Result<Output> {
        let mut cmd = Command::new("git");
        if self.p.overleaf_https() {
            cmd.args(["-c", "credential.helper=", "-c"])
                .arg(format!("credential.helper={}", OVERLEAF_TOKEN.helper()));
        }
        cmd.args([
            "-c",
            "core.quotepath=off",
            "-c",
            "http.lowSpeedLimit=1000",
            "-c",
            "http.lowSpeedTime=60",
        ])
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .envs(env.iter().copied())
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        if let Some(text) = input {
            child.stdin.take().unwrap().write_all(text.as_bytes())?;
        }
        Ok(child.wait_with_output()?)
    }

    fn checked(&self, cwd: &Path, args: &[&str], env: &[(&str, &str)], input: Option<&str>) -> Result<String> {
        let out = self.run(cwd, args, env, input)?;
        if !out.status.success() {
            let msg = if out.stderr.is_empty() {
                &out.stdout
            } else {
                &out.stderr
            };
            let msg = String::from_utf8_lossy(msg);
            bail!(
                "git {}: {}",
                args[..args.len().min(2)].join(" "),
                redact(tail(msg.trim(), 1500))
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// Run git in the clone; trimmed stdout, error on failure.
    pub fn git(&self, args: &[&str]) -> Result<String> {
        self.checked(self.dir(), args, &[], None)
    }

    /// Run git in the clone; whether it succeeded.
    pub fn ok(&self, args: &[&str]) -> Result<bool> {
        Ok(self.run(self.dir(), args, &[], None)?.status.success())
    }

    fn need_token(&self) -> Result<()> {
        if self.p.overleaf_https() && OVERLEAF_TOKEN.get().is_none() {
            return Err(OVERLEAF_TOKEN.missing());
        }
        Ok(())
    }

    /// Held while one client syncs the clone.
    pub fn lock(&self) -> Result<File> {
        std::fs::create_dir_all(self.p.state())?;
        let f = File::create(self.p.state().join("lock"))?;
        if f.try_lock().is_err() {
            bail!("another client is syncing this project; retry shortly");
        }
        Ok(f)
    }

    pub fn branch(&self) -> &str {
        self.branch.get_or_init(|| {
            let head = self
                .git(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
                .unwrap_or_default();
            head.strip_prefix("origin/")
                .filter(|b| !b.is_empty())
                .unwrap_or("master")
                .to_owned()
        })
    }

    pub fn remote(&self) -> String {
        format!("origin/{}", self.branch())
    }

    /// Clone on first use; give an existing clone an identity for commits if it has none.
    pub fn open(&self) -> Result<()> {
        let dir = self.dir();
        if !dir.join(".git").is_dir() {
            self.need_token()?;
            if dir.read_dir().is_ok_and(|mut d| d.next().is_some()) {
                bail!("{} is not empty and not a clone", dir.display());
            }
            let parent = dir.parent().unwrap_or(Path::new("."));
            std::fs::create_dir_all(parent)?;
            self.checked(parent, &["clone", &self.p.git_url, &dir.to_string_lossy()], &[], None)?;
            text::write(&dir.join(".git/info/exclude"), EXCLUDE)?;
        }
        if self.git(&["config", "user.name"]).unwrap_or_default().is_empty() {
            self.git(&["config", "user.name", &self.p.user])?;
            self.git(&["config", "user.email", "interleaf@localhost"])?;
        }
        Ok(())
    }

    pub fn fetch(&self) -> Result<()> {
        self.need_token()?;
        self.git(&["fetch", "--prune", "origin"])?;
        Ok(())
    }

    fn count(&self, range: &str) -> Result<usize> {
        Ok(self.git(&["rev-list", "--count", range])?.parse()?)
    }

    /// A file at a revision, None if it is not there.
    pub fn show(&self, rev: &str, path: &str) -> Result<Option<String>> {
        let out = self.run(self.dir(), &["show", &format!("{rev}:{path}")], &[], None)?;
        Ok(out
            .status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned()))
    }

    /// Commit object of the working tree (plus overrides) on top of HEAD, built in a temporary
    /// index. None if it equals HEAD.
    pub fn snapshot(&self, overrides: &BTreeMap<String, String>) -> Result<Option<String>> {
        let td = tempfile::tempdir()?;
        let index = td.path().join("index");
        let env = [("GIT_INDEX_FILE", index.to_str().unwrap())];
        let dir = self.dir();
        self.checked(dir, &["read-tree", "HEAD"], &env, None)?;
        self.checked(dir, &["add", "-A"], &env, None)?;
        for (path, text) in overrides {
            let blob = self.checked(dir, &["hash-object", "-w", "--stdin"], &[], Some(text))?;
            self.checked(
                dir,
                &["update-index", "--add", "--cacheinfo", &format!("100644,{blob},{path}")],
                &env,
                None,
            )?;
        }
        let tree = self.checked(dir, &["write-tree"], &env, None)?;
        if tree == self.git(&["rev-parse", "HEAD^{tree}"])? {
            return Ok(None);
        }
        Ok(Some(self.git(&[
            "commit-tree",
            &tree,
            "-p",
            "HEAD",
            "-m",
            "snapshot",
        ])?))
    }

    pub fn work(&self) -> Result<Option<String>> {
        self.snapshot(&BTreeMap::new())
    }

    pub fn changes(&self, a: &str, b: &str) -> Result<Rows> {
        Ok(self
            .git(&["diff", "--name-status", a, b])?
            .lines()
            .filter_map(|l| {
                let parts: Vec<&str> = l.split('\t').collect();
                Some((parts.first()?.chars().next()?, parts.last()?.to_string()))
            })
            .collect())
    }

    fn unmerged(&self) -> Result<Vec<String>> {
        let set: BTreeSet<String> = self
            .git(&["ls-files", "-u"])?
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(_, p)| p.to_owned())
            .collect();
        Ok(set.into_iter().collect())
    }

    fn markers<'a>(&self, paths: impl IntoIterator<Item = &'a String>) -> Vec<String> {
        let mut hits = vec![];
        for rel in paths {
            let f = self.dir().join(rel);
            let textual = f
                .extension()
                .is_some_and(|e| TEXT_SUFFIXES.contains(&e.to_string_lossy().as_ref()));
            if textual && f.is_file() {
                let txt = text::read(&f).unwrap_or_default();
                hits.extend(
                    re!(r"(?m)^(<{7}|>{7})( |$)")
                        .find_iter(&txt)
                        .map(|m| format!("{rel}:{}", text::line_at(&txt, m.start()))),
                );
            }
        }
        hits
    }

    fn refuse_markers(hits: Vec<String>) -> Result<()> {
        if !hits.is_empty() {
            bail!("conflict markers left at {}", hits[..hits.len().min(20)].join(", "));
        }
        Ok(())
    }

    /// Clear the unmerged index a 'markers' pull leaves, once the markers are edited out.
    fn settle(&self) -> Result<()> {
        let um = self.unmerged()?;
        if !um.is_empty() {
            Self::refuse_markers(self.markers(&um))?;
            self.git(&["reset", "-q"])?;
        }
        Ok(())
    }

    pub fn log(&self, range: &str, n: usize, files: bool) -> Result<String> {
        let n = format!("-n{n}");
        let mut args = vec!["log", &n, "--date=format:%Y-%m-%d %H:%M", "--format=%h  %ad  %an  %s"];
        if files {
            args.push("--name-status");
        }
        args.push(range);
        self.git(&args)
    }

    pub fn status(&self) -> Result<Report> {
        self.fetch()?;
        let mut r = Report::default();
        r.set("clone", self.dir().display());
        r.set("branch", self.branch());
        let local = match self.work()? {
            Some(w) => self.changes("HEAD", &w)?,
            None => vec![],
        };
        r.list("local changes", local.iter().map(|(s, p)| format!("{s}  {p}")));
        r.set("unpushed commits", self.count(&format!("{}..HEAD", self.remote()))?);
        let incoming = self.count(&format!("HEAD..{}", self.remote()))?;
        r.set(&format!("new on {}", self.p.upstream()), incoming);
        r.list("unresolved conflict", self.unmerged()?);
        let stashes = self.git(&["stash", "list"])?;
        r.list("backup stashes", stashes.lines().filter(|l| l.contains("interleaf")));
        if incoming > 0 {
            r.set(
                "incoming",
                format!("\n{}", self.log(&format!("HEAD..{}", self.remote()), 20, false)?),
            );
        }
        Ok(r)
    }

    fn fine_merge(&self, ours: &str, theirs: &str, files: &[String]) -> Result<BTreeMap<String, String>> {
        let base = self.git(&["merge-base", ours, theirs])?;
        let mut done = BTreeMap::new();
        for f in files {
            if let (Some(b), Some(o), Some(t)) = (self.show(&base, f)?, self.show(ours, f)?, self.show(theirs, f)?)
                && let Some(m) = merge3(&text::lines(&b), &text::lines(&o), &text::lines(&t))
            {
                done.insert(f.clone(), m);
            }
        }
        Ok(done)
    }

    fn clash_detail(&self, ours: &str, theirs: &str, files: &[String]) -> Result<String> {
        let base = self.git(&["merge-base", ours, theirs])?;
        let mut parts = vec![];
        for f in files {
            let mine = self.git(&["diff", "-U1", &base, ours, "--", f])?;
            let other = self.git(&["diff", "-U1", &base, theirs, "--", f])?;
            parts.push(format!(
                "=== {f}\n--- yours:\n{mine}\n--- {}:\n{other}",
                self.p.upstream()
            ));
        }
        Ok(text::head(&parts.join("\n"), 6000).to_owned())
    }

    /// Bring in the remote's changes, keeping local edits. `markers` writes conflict markers on
    /// a real clash; otherwise a clash changes nothing.
    pub fn pull(&self, markers: bool) -> Result<Report> {
        self.settle()?;
        self.fetch()?;
        let remote = self.remote();
        let behind = self.count(&format!("HEAD..{remote}"))?;
        let ahead = self.count(&format!("{remote}..HEAD"))?;
        if behind == 0 {
            return Ok(Report::new("up to date"));
        }
        let mut r = Report::default();
        r.set(
            "incoming",
            format!("\n{}", self.log(&format!("HEAD..{remote}"), 20, true)?),
        );
        let w = self.work()?;
        if w.is_none() && ahead == 0 {
            self.git(&["merge", "--ff-only", &remote])?;
            return Ok(r.with_state("fast-forward"));
        }
        let ours = match w {
            Some(w) => w,
            None => self.git(&["rev-parse", "HEAD"])?,
        };
        let mt = self.run(
            self.dir(),
            &[
                "merge-tree",
                "--write-tree",
                "--name-only",
                "--no-messages",
                &ours,
                &remote,
            ],
            &[],
            None,
        )?;
        let files: Vec<String> = match mt.status.code() {
            Some(0) => vec![],
            Some(1) => String::from_utf8_lossy(&mt.stdout)
                .lines()
                .skip(1)
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect(),
            _ => bail!("git merge-tree: {}", String::from_utf8_lossy(&mt.stderr).trim()),
        };
        let fine = self.fine_merge(&ours, &remote, &files)?;
        let clash: Vec<String> = files.iter().filter(|f| !fine.contains_key(*f)).cloned().collect();
        if !fine.is_empty() {
            r.list("merged line by line", fine.keys());
        }
        if !clash.is_empty() {
            r.list("conflict", &clash);
            r.set("detail", format!("\n{}", self.clash_detail(&ours, &remote, &clash)?));
            if !markers {
                return Ok(r.with_state("conflict, nothing changed"));
            }
        }
        if ahead > 0 {
            // commits left by a failed push become uncommitted changes again
            let base = self.git(&["merge-base", "HEAD", &remote])?;
            self.git(&["reset", "-q", "--soft", &base])?;
        }
        let label = format!(
            "interleaf backup before merging {}",
            self.git(&["rev-parse", "--short", &remote])?
        );
        self.git(&["stash", "push", "-u", "-m", &label])?;
        self.git(&["merge", "--ff-only", &remote])?;
        let mut ok = self.ok(&["stash", "pop"])?;
        if !ok && clash.is_empty() {
            for (rel, text) in &fine {
                text::write(&self.dir().join(rel), text)?;
            }
            self.git(&["reset", "-q"])?;
            self.git(&["stash", "drop", "-q"])?;
            ok = true;
        }
        if ok {
            return Ok(r.with_state("merged, local changes kept"));
        }
        r.list("markers", self.markers(&self.unmerged()?));
        r.set("backup", format!("git stash list: {label}"));
        Ok(r.with_state("conflict markers written"))
    }

    /// Send local changes as one commit, after pulling. `transform` rewrites changed files
    /// before the commit (suggestion markup).
    pub fn push(&self, o: Push) -> Result<Report> {
        self.settle()?;
        if let Some(w) = self.work()? {
            let changed: Vec<String> = self.changes("HEAD", &w)?.into_iter().map(|(_, p)| p).collect();
            Self::refuse_markers(self.markers(&changed))?;
        }
        let pulled = self.pull(false)?;
        if pulled.state.starts_with("conflict") {
            return Ok(pulled.with_state(format!("blocked: conflict with {}", self.p.upstream())));
        }
        let remote = self.remote();
        let w = self.work()?;
        let head = "HEAD".to_owned();
        let rows = self.changes(&remote, w.as_ref().unwrap_or(&head))?;
        if rows.is_empty() {
            return Ok(Report::new("nothing to push"));
        }
        let overrides = match o.transform {
            Some(t) => t(&rows)?,
            None => BTreeMap::new(),
        };
        let mut r = Report::default();
        r.list("changes", rows.iter().map(|(s, p)| format!("{s}  {p}")));
        if let Some(incoming) = pulled.get("incoming") {
            r.set("incoming merged", incoming.to_owned());
        }
        let direct: Vec<String> = self
            .direct_paths()
            .into_iter()
            .filter(|d| rows.iter().any(|(_, p)| p == d))
            .collect();
        if !direct.is_empty() {
            r.list("sent as is (resolved suggestions)", &direct);
        }
        let dels: Vec<&str> = rows
            .iter()
            .filter(|(s, _)| *s == 'D')
            .map(|(_, p)| p.as_str())
            .collect();
        if !dels.is_empty() && !o.allow_deletions {
            return Ok(r.with_state(format!(
                "blocked: would delete {} (allow_deletions=true)",
                dels.join(", ")
            )));
        }
        let globs: Vec<regex::Regex> = o.protected.iter().map(|g| glob(g)).collect();
        let prot: Vec<&str> = rows
            .iter()
            .map(|(_, p)| p.as_str())
            .filter(|p| globs.iter().any(|g| g.is_match(p)))
            .collect();
        if !prot.is_empty() {
            return Ok(r.with_state(format!("blocked: protected by the project: {}", prot.join(", "))));
        }
        let big: Vec<&str> = rows
            .iter()
            .filter(|(s, p)| {
                *s != 'D'
                    && self
                        .dir()
                        .join(p)
                        .metadata()
                        .is_ok_and(|m| m.len() as f64 > o.max_mb * 1e6)
            })
            .map(|(_, p)| p.as_str())
            .collect();
        if !big.is_empty() {
            return Ok(r.with_state(format!("blocked: over {} MB: {}", o.max_mb, big.join(", "))));
        }
        if o.dry_run {
            let target = match (overrides.is_empty(), &w) {
                (false, _) => self.snapshot(&overrides)?.unwrap_or(head),
                (true, Some(w)) => w.clone(),
                (true, None) => head,
            };
            let diff = format!(
                "{}\n\n{}",
                self.git(&["diff", "--stat", &remote, &target])?,
                self.git(&["diff", "-U2", &remote, &target])?
            );
            r.set("diff", format!("\n{}", text::head(&diff, 15000)));
            return Ok(r.with_state("dry run, nothing pushed"));
        }
        for (p, text) in &overrides {
            text::write(&self.dir().join(p), text)?;
        }
        self.git(&["add", "-A"])?;
        if !self.ok(&["diff", "--cached", "--quiet"])? {
            let message = match o.trailer.as_str() {
                "" => o.message.trim().to_owned(),
                t => format!("{}\n\n{t}", o.message.trim()),
            };
            self.git(&["commit", "-q", "-m", &message])?;
        }
        let refspec = format!("HEAD:{}", self.branch());
        if !self.ok(&["push", "origin", &refspec])? {
            // someone pushed or typed in the browser meanwhile: rebase once and retry
            self.fetch()?;
            if !self.ok(&["rebase", &remote])? {
                self.git(&["rebase", "--abort"])?;
                let up = self.p.upstream();
                return Ok(r.with_state(format!(
                    "push rejected: {up} changed meanwhile and conflicts; commit kept locally"
                )));
            }
            self.git(&["push", "origin", &refspec])?;
        }
        let _ = std::fs::remove_file(self.p.state().join("direct"));
        r.set("commit", self.git(&["rev-parse", "--short", "HEAD"])?);
        Ok(r.with_state("pushed"))
    }

    /// Files whose suggestions were just resolved: push sends them unmarked.
    pub fn direct_paths(&self) -> BTreeSet<String> {
        let f = self.p.state().join("direct");
        text::read(&f)
            .map(|t| t.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    pub fn mark_direct(&self, paths: &[String]) -> Result<()> {
        let mut all = self.direct_paths();
        all.extend(paths.iter().cloned());
        std::fs::create_dir_all(self.p.state())?;
        text::write(
            &self.p.state().join("direct"),
            &(all.into_iter().collect::<Vec<_>>().join("\n") + "\n"),
        )
    }
}

/// A shell-style pattern (`*`, `?`) as a regular expression over the whole path.
fn glob(pattern: &str) -> regex::Regex {
    let body = regex::escape(pattern).replace(r"\*", ".*").replace(r"\?", ".");
    regex::Regex::new(&format!("^{body}$")).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(b: &str, o: &str, t: &str) -> Option<String> {
        merge3(&text::lines(b), &text::lines(o), &text::lines(t))
    }

    #[test]
    fn adjacent_lines_merge() {
        assert_eq!(m("a\nb\nc\n", "a\nB\nc\n", "a\nb\nC\n").as_deref(), Some("a\nB\nC\n"));
    }

    #[test]
    fn same_line_clashes() {
        assert_eq!(m("a\nb\nc\n", "a\nX\nc\n", "a\nY\nc\n"), None);
    }

    #[test]
    fn same_edit_once() {
        assert_eq!(m("a\nb\n", "a\nX\n", "a\nX\n").as_deref(), Some("a\nX\n"));
    }

    #[test]
    fn globs() {
        assert!(glob("sections/*.tex").is_match("sections/intro.tex"));
        assert!(!glob("abstract.tex").is_match("abstract.texx"));
    }
}
