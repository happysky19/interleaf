//! Helpers shared by the modules: files, character-safe slicing, brace groups, diffs.
use anyhow::{Context, Result, bail};
use similar::{Algorithm, DiffTag, capture_diff_slices};
use std::hash::Hash;
use std::ops::Range;
use std::path::{Path, PathBuf};

/// A regular expression compiled once.
macro_rules! re {
    ($s:expr) => {{
        static R: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new($s).unwrap());
        &*R
    }};
}
pub(crate) use re;

pub fn read(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub fn write(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))
}

/// The first `n` characters.
pub fn head(s: &str, n: usize) -> &str {
    s.char_indices().nth(n).map_or(s, |(i, _)| &s[..i])
}

/// The last `n` characters.
pub fn tail(s: &str, n: usize) -> &str {
    let count = s.chars().count();
    s.char_indices()
        .nth(count.saturating_sub(n))
        .map_or(s, |(i, _)| &s[i..])
}

/// Line number (from 1) of byte offset `pos`.
pub fn line_at(text: &str, pos: usize) -> usize {
    text.as_bytes()[..pos].iter().filter(|&&b| b == b'\n').count() + 1
}

/// Directories and files under `root`, relative and sorted, without `.git`.
pub fn walk(root: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    fn go(root: &Path, rel: &Path, dirs: &mut Vec<PathBuf>, files: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(root.join(rel)) else {
            return;
        };
        for e in entries.flatten() {
            let path = rel.join(e.file_name());
            match e.file_type() {
                Ok(t) if t.is_dir() && e.file_name() != ".git" => {
                    dirs.push(path.clone());
                    go(root, &path, dirs, files);
                }
                Ok(t) if t.is_file() => files.push(path),
                _ => {}
            }
        }
    }
    let (mut dirs, mut files) = (Vec::new(), Vec::new());
    go(root, Path::new(""), &mut dirs, &mut files);
    dirs.sort();
    files.sort();
    (dirs, files)
}

pub fn rel(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Byte offset of the first `%` that starts a comment.
pub fn comment_at(line: &str) -> Option<usize> {
    let mut slashes = 0;
    for (i, &c) in line.as_bytes().iter().enumerate() {
        if c == b'%' && slashes % 2 == 0 {
            return Some(i);
        }
        slashes = if c == b'\\' { slashes + 1 } else { 0 };
    }
    None
}

pub fn strip_comment(line: &str) -> &str {
    &line[..comment_at(line).unwrap_or(line.len())]
}

/// Content range of the brace group opening at `text[i]`, and the offset after its closing brace.
pub fn group(text: &str, i: usize) -> Option<(Range<usize>, usize)> {
    let b = text.as_bytes();
    if b.get(i) != Some(&b'{') {
        return None;
    }
    let mut depth = 0;
    for j in i..b.len() {
        if (b[j] == b'{' || b[j] == b'}') && (j == i || b[j - 1] != b'\\') {
            depth += if b[j] == b'{' { 1 } else { -1 };
            if depth == 0 {
                return Some((i + 1..j, j + 1));
            }
        }
    }
    None
}

/// Braces balance, ignoring escaped ones.
pub fn balanced(s: &str) -> bool {
    let b = s.as_bytes();
    let mut depth = 0i32;
    for j in 0..b.len() {
        if (b[j] == b'{' || b[j] == b'}') && (j == 0 || b[j - 1] != b'\\') {
            depth += if b[j] == b'{' { 1 } else { -1 };
            if depth < 0 {
                return false;
            }
        }
    }
    depth == 0
}

/// Spans `\begin{env}...\end{env}` of the named environments (starred forms too), as
/// (start, end, body).
pub fn env_spans(text: &str, names: &[&str]) -> Vec<(usize, usize, Range<usize>)> {
    let begin = regex::Regex::new(&format!(r"\\begin\{{({})(\*?)\}}", names.join("|"))).unwrap();
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(c) = begin.captures_at(text, at) {
        let m = c.get(0).unwrap();
        let end = format!("\\end{{{}{}}}", &c[1], &c[2]);
        match text[m.end()..].find(&end) {
            Some(k) => {
                let stop = m.end() + k;
                out.push((m.start(), stop + end.len(), m.end()..stop));
                at = stop + end.len();
            }
            None => at = m.end(),
        }
    }
    out
}

/// Diff of two sequences as (equal, old range, new range), with adjacent changes merged.
pub fn opcodes<T: Eq + Hash>(a: &[T], b: &[T]) -> Vec<(bool, Range<usize>, Range<usize>)> {
    let mut out: Vec<(bool, Range<usize>, Range<usize>)> = Vec::new();
    for op in capture_diff_slices(Algorithm::Myers, a, b) {
        let (tag, o, n) = op.as_tag_tuple();
        let equal = tag == DiffTag::Equal;
        match out.last_mut() {
            Some((false, lo, ln)) if !equal => {
                lo.end = o.end;
                ln.end = n.end;
            }
            _ => out.push((equal, o, n)),
        }
    }
    out
}

/// Lines with their line endings.
pub fn lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// A manuscript destination must stay inside its checkout, excluding Git metadata and symlinks.
pub fn project_path(root: &Path, rel: &Path) -> Result<PathBuf> {
    use std::path::Component;
    if rel.as_os_str().is_empty() {
        bail!("project path is empty");
    }
    let mut path = root.to_owned();
    for component in rel.components() {
        match component {
            Component::CurDir => continue,
            Component::Normal(name) if !name.to_string_lossy().eq_ignore_ascii_case(".git") => {
                path.push(name);
                if path.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
                    bail!("project path contains a symlink: {}", rel.display());
                }
            }
            _ => bail!("path must stay inside the project and outside .git: {}", rel.display()),
        }
    }
    if path == root {
        bail!("project path must name a file");
    }
    Ok(path)
}

/// Ignore ambient Git repository/index overrides while retaining the caller's Git executable.
pub fn git_command() -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
    ] {
        cmd.env_remove(key);
    }
    cmd
}

/// Compilation does not need account credentials or inherited session descriptors.
pub fn scrub_build_env(cmd: &mut std::process::Command) {
    // TeX (including Lua) can inspect its environment without shell escape.
    // Preserve only executable/search, locale and font/TeX configuration.
    let values: Vec<_> = std::env::vars_os()
        .filter(|(key, _)| {
            let key = key.to_string_lossy();
            matches!(
                key.as_ref(),
                "PATH"
                    | "HOME"
                    | "TMPDIR"
                    | "TMP"
                    | "TEMP"
                    | "LANG"
                    | "LC_ALL"
                    | "LC_CTYPE"
                    | "TZ"
                    | "SOURCE_DATE_EPOCH"
                    | "FONTCONFIG_FILE"
                    | "FONTCONFIG_PATH"
                    | "XDG_CACHE_HOME"
                    | "XDG_CONFIG_HOME"
            ) || key.starts_with("TEX")
                || key.starts_with("BIB")
                || key.starts_with("BST")
        })
        .collect();
    cmd.env_clear().envs(values);
}
