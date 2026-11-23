//! Reading LaTeX: the document as compiled (`\input` expanded), outline, consistency check, and a
//! local compile with errors mapped to file:line.
use crate::text::{self, env_spans, group, head, re, read, strip_comment, tail};
use anyhow::{Result, bail};
use regex::Regex;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Instant, SystemTime};

pub const GFX_EXT: [&str; 5] = [".pdf", ".png", ".jpg", ".jpeg", ".eps"];

fn sec() -> &'static Regex {
    re!(r"\\(part|chapter|section|subsection|subsubsection|paragraph)(\*?)\s*(?:\[[^\]]*\])?\s*\{")
}
fn input() -> &'static Regex {
    re!(r"\\(?:input|include|subfile)\s*\{([^}]+)\}")
}
fn label() -> &'static Regex {
    re!(r"\\label\s*\{([^}]+)\}")
}
fn gfx() -> &'static Regex {
    re!(r"\\(includegraphics\*?\s*(?:\[[^\]]*\])?|plotone|plottwo|fig)\s*\{([^}]+)\}(?:\s*\{([^}]+)\})?")
}

fn level(cmd: &str) -> i32 {
    match cmd {
        "part" => -1,
        "chapter" => 0,
        "section" => 1,
        "subsection" => 2,
        "subsubsection" => 3,
        _ => 4,
    }
}

pub fn find_main(root: &Path, hint: Option<&str>) -> Result<String> {
    if let Some(h) = hint {
        return Ok(h.to_owned());
    }
    let mut cands = vec![];
    for f in text::walk(root).1 {
        if f.extension().is_some_and(|e| e == "tex") {
            let t = read(&root.join(&f))?;
            if re!(r"(?m)^[^%\n]*\\documentclass").is_match(&t) && t.contains("\\begin{document}") {
                let rel = text::rel(&f);
                cands.push((
                    f.file_name().is_some_and(|n| n != "main.tex"),
                    rel.matches('/').count(),
                    rel,
                ));
            }
        }
    }
    match cands.into_iter().min() {
        Some(c) => Ok(c.2),
        None => bail!("no .tex file with \\documentclass and \\begin{{document}}"),
    }
}

fn resolve_input(root: &Path, name: &str) -> Option<String> {
    let name = name.trim();
    [name.to_owned(), format!("{name}.tex")]
        .into_iter()
        .find(|c| root.join(c).is_file())
}

/// Main file with `\input`/`\include` replaced by the files' text (comments kept).
pub fn flatten(root: &Path, main: &str) -> Result<String> {
    fn expand(root: &Path, rel: &str, depth: usize) -> Result<String> {
        let mut out = String::new();
        for line in read(&root.join(rel))?.split_inclusive('\n') {
            let m = input().captures(strip_comment(line));
            match m
                .as_ref()
                .and_then(|c| resolve_input(root, &c[1]).map(|child| (c.get(0).unwrap(), child)))
            {
                Some((m, child)) if depth < 20 => {
                    out += &line[..m.start()];
                    out += &expand(root, &child, depth + 1)?;
                    out += &line[m.end()..];
                }
                _ => out += line,
            }
        }
        Ok(out)
    }
    expand(root, main, 0)
}

pub fn word_count(seg: &str) -> usize {
    let mut seg = re!(r"(?s)\$\$.*?\$\$|\$[^$]*\$|\\\[.*?\\\]|\\\(.*?\\\)")
        .replace_all(seg, " ")
        .into_owned();
    for (start, end, _) in env_spans(
        &seg,
        &[
            "equation",
            "align",
            "gather",
            "multline",
            "eqnarray",
            "table",
            "deluxetable",
            "tabular",
        ],
    )
    .into_iter()
    .rev()
    {
        seg.replace_range(start..end, " ");
    }
    let seg = re!(r"\\(label|ref|eqref|cite\w*|includegraphics|input|include)\*?(\[[^\]]*\])*\{[^}]*\}")
        .replace_all(&seg, " ");
    let seg = re!(r"\\[a-zA-Z@]+\*?").replace_all(&seg, " ");
    re!(r"[A-Za-z][A-Za-z'\-]*").find_iter(&seg).count()
}

pub struct Line {
    pub file: String,
    pub n: usize,
    pub code: String,
}

pub struct Section {
    pub level: i32,
    pub num: String,
    pub title: String,
    pub start: usize,
    pub end: usize,
    pub words: usize,
    pub at: String,
    pub label: Option<String>,
}

/// Comment-stripped text of the document as LaTeX reads it; offsets map back to file:line.
pub struct Doc {
    pub root: PathBuf,
    pub main: String,
    pub lines: Vec<Line>,
    pub missing: Vec<String>,
    pub text: String,
    starts: Vec<usize>,
}

impl Doc {
    pub fn new(root: &Path, main: &str) -> Result<Doc> {
        let mut d = Doc {
            root: root.to_owned(),
            main: main.to_owned(),
            lines: vec![],
            missing: vec![],
            text: String::new(),
            starts: vec![],
        };
        d.walk(main, &mut HashSet::new())?;
        let mut pos = 0;
        for l in &d.lines {
            d.starts.push(pos);
            pos += l.code.len() + 1;
        }
        d.text = d.lines.iter().map(|l| l.code.as_str()).collect::<Vec<_>>().join("\n");
        Ok(d)
    }

    fn walk(&mut self, rel: &str, seen: &mut HashSet<String>) -> Result<()> {
        if !seen.insert(rel.to_owned()) {
            return Ok(());
        }
        let src = read(&self.root.join(rel))?;
        for (i, line) in src.lines().enumerate() {
            let (n, code) = (i + 1, strip_comment(line));
            let Some(c) = input().captures(code) else {
                self.lines.push(Line {
                    file: rel.into(),
                    n,
                    code: code.into(),
                });
                continue;
            };
            let m = c.get(0).unwrap();
            self.lines.push(Line {
                file: rel.into(),
                n,
                code: code[..m.end()].into(),
            });
            match resolve_input(&self.root, &c[1]) {
                Some(child) => self.walk(&child, seen)?,
                None => self.missing.push(format!("{rel}:{n}: {}", m.as_str())),
            }
            if !code[m.end()..].trim().is_empty() {
                self.lines.push(Line {
                    file: rel.into(),
                    n,
                    code: code[m.end()..].into(),
                });
            }
        }
        Ok(())
    }

    fn index(&self, offset: usize) -> usize {
        self.starts.partition_point(|&s| s <= offset).saturating_sub(1)
    }

    pub fn at(&self, offset: usize) -> String {
        let l = &self.lines[self.index(offset)];
        format!("{}:{}", l.file, l.n)
    }

    /// Content of the brace group opening at text[i], and the offset after it.
    pub fn braced(&self, i: usize) -> (&str, usize) {
        match group(&self.text, i) {
            Some((r, end)) => (&self.text[r], end),
            None => (&self.text[(i + 1).min(self.text.len())..], self.text.len()),
        }
    }

    pub fn sections(&self) -> Vec<Section> {
        let mut secs: Vec<Section> = vec![];
        let mut count = [0usize; 7];
        let appendix = self.text.find("\\appendix");
        for c in sec().captures_iter(&self.text) {
            let m = c.get(0).unwrap();
            let (title, end) = self.braced(m.end() - 1);
            let lvl = level(&c[1]);
            let mut num = String::new();
            if c[2].is_empty() && lvl >= 1 {
                let l = lvl as usize;
                count[l] += 1;
                count[l + 1..].fill(0);
                let mut parts: Vec<String> = count[1..=l].iter().map(|n| n.to_string()).collect();
                if appendix.is_some_and(|a| a < m.start()) {
                    parts[0] = char::from(b'@' + count[1] as u8).to_string();
                }
                num = parts.join(".");
            }
            let after = head(&self.text[end..], 200).trim_start();
            let label = re!(r"^\\label\s*\{([^}]+)\}").captures(after).map(|l| l[1].to_owned());
            let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
            secs.push(Section {
                level: lvl,
                num,
                title,
                start: m.start(),
                end: 0,
                words: 0,
                at: self.at(m.start()),
                label,
            });
        }
        for i in 0..secs.len() {
            let end = secs[i + 1..]
                .iter()
                .find(|t| t.level <= secs[i].level)
                .map_or(self.text.len(), |t| t.start);
            secs[i].end = end;
            secs[i].words = word_count(&self.text[secs[i].start..end]);
        }
        secs
    }

    pub fn body(&self) -> &str {
        self.text
            .find("\\begin{document}")
            .map_or(&self.text, |b| &self.text[b..])
    }

    pub fn env(&self, name: &str) -> Option<&str> {
        let open = format!("\\begin{{{name}}}");
        let start = self.text.find(&open)? + open.len();
        let stop = self.text[start..].find(&format!("\\end{{{name}}}"))?;
        Some(&self.text[start..start + stop])
    }
}

pub fn outline(doc: &Doc) -> String {
    let mut out = vec![format!("{}: ~{} words", doc.main, word_count(doc.body()))];
    if let Some(ab) = doc.env("abstract") {
        out.push(format!("  abstract ({} w)", word_count(ab)));
    }
    for s in doc.sections() {
        let lab = s.label.as_ref().map(|l| format!("  <{l}>")).unwrap_or_default();
        out.push(format!(
            "{}{} {}  ({} w)  {}{lab}",
            "  ".repeat(s.level.max(0) as usize),
            s.num,
            s.title,
            s.words,
            s.at
        ));
    }
    let mut floats = vec![];
    for c in re!(r"\\begin\{(figure\*?|table\*?|deluxetable\*?)\}").captures_iter(&doc.text) {
        let m = c.get(0).unwrap();
        let stop = doc.text[m.end()..]
            .find(&format!("\\end{{{}}}", &c[1]))
            .map_or(doc.text.len(), |k| m.end() + k);
        let body = &doc.text[m.end()..stop];
        let lab = label()
            .captures(body)
            .map_or("(no label)".to_owned(), |l| l[1].to_owned());
        let cap = re!(r"\\caption\s*(?:\[[^\]]*\])?\s*\{").find(body);
        let ctext = cap
            .map(|k| {
                head(
                    &doc.braced(m.end() + k.end() - 1)
                        .0
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" "),
                    80,
                )
                .to_owned()
            })
            .unwrap_or_default();
        let g: Vec<&str> = gfx().captures_iter(body).map(|g| g.get(2).unwrap().as_str()).collect();
        floats.push(format!("  {}  {lab}  {}  {ctext}", doc.at(m.start()), g.join(",")));
    }
    if !floats.is_empty() {
        out.push(String::new());
        out.push("floats:".into());
        out.extend(floats);
    }
    let eqs: Vec<String> = env_spans(&doc.text, &["equation", "align", "gather", "multline"])
        .into_iter()
        .filter_map(|(start, end, _)| {
            let labels: Vec<&str> = label()
                .captures_iter(&doc.text[start..end])
                .map(|c| c.get(1).unwrap().as_str())
                .collect();
            (!labels.is_empty()).then(|| format!("  {}  {}", doc.at(start), labels.join(", ")))
        })
        .collect();
    if !eqs.is_empty() {
        out.push(String::new());
        out.push("labelled equations:".into());
        out.extend(eqs);
    }
    out.join("\n")
}

pub fn section_source(doc: &Doc, query: &str) -> Result<String> {
    let secs = doc.sections();
    let (q, ql) = (query.trim(), query.trim().to_lowercase());
    let found = secs
        .iter()
        .find(|s| s.num == q || s.label.as_deref().is_some_and(|l| l.to_lowercase() == ql))
        .or_else(|| secs.iter().find(|s| s.title.to_lowercase().contains(&ql)));
    let Some(s) = found else {
        let all: Vec<String> = secs.iter().map(|t| format!("{} {}", t.num, t.title)).collect();
        return Ok(format!("no match; sections: {}", all.join("; ")));
    };
    let (i0, i1) = (doc.index(s.start), doc.index(s.end.saturating_sub(1)));
    let mut out = vec![];
    let mut cur = "";
    let mut seen = HashSet::new();
    let mut raw: HashMap<&str, Vec<String>> = HashMap::new();
    for l in &doc.lines[i0..=i1] {
        if !seen.insert((&l.file, l.n)) {
            continue;
        }
        if l.file != cur {
            out.push(format!("----- {}", l.file));
            cur = &l.file;
        }
        if !raw.contains_key(l.file.as_str()) {
            raw.insert(
                &l.file,
                read(&doc.root.join(&l.file))?.lines().map(str::to_owned).collect(),
            );
        }
        out.push(format!(
            "{:5}  {}",
            l.n,
            raw[l.file.as_str()].get(l.n - 1).map_or("", |s| s)
        ));
    }
    out.truncate(800);
    Ok(format!("{} {}\n{}", s.num, s.title, out.join("\n")))
}

/// Keys with where they occur, in order of first appearance.
pub type Keys = Vec<(String, Vec<String>)>;

fn collect<'a>(doc: &Doc, found: impl Iterator<Item = (usize, &'a str)>) -> Keys {
    let mut keys: Keys = vec![];
    let mut index = HashMap::new();
    for (pos, list) in found {
        for k in list.split(',').map(str::trim).filter(|k| !k.is_empty() && *k != "*") {
            let i = *index.entry(k.to_owned()).or_insert_with(|| {
                keys.push((k.to_owned(), vec![]));
                keys.len() - 1
            });
            keys[i].1.push(doc.at(pos));
        }
    }
    keys
}

fn keys_in(doc: &Doc, re: &Regex) -> Keys {
    collect(
        doc,
        re.captures_iter(&doc.text)
            .map(|c| (c.get(0).unwrap().start(), c.get(1).unwrap().as_str())),
    )
}

pub fn cites(doc: &Doc) -> Keys {
    let re = re!(r"\\(?:no)?[cC]ite([a-zA-Z]*)\*?\s*(?:\[[^\]]*\]\s*){0,2}\{([^}]*)\}");
    let found = re
        .captures_iter(&doc.text)
        .filter(|c| !c[1].starts_with("text"))
        .map(|c| (c.get(0).unwrap().start(), c.get(2).unwrap().as_str()));
    collect(doc, found)
}

pub fn bib_files(doc: &Doc) -> Vec<String> {
    re!(r"\\(?:bibliography|addbibresource)\s*(?:\[[^\]]*\])?\s*\{([^}]+)\}")
        .captures_iter(&doc.text)
        .flat_map(|m| {
            m[1].replace(' ', "")
                .split(',')
                .map(|b| {
                    if b.ends_with(".bib") {
                        b.to_owned()
                    } else {
                        format!("{b}.bib")
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

pub fn check_structure(doc: &Doc, bib_keys: &BTreeSet<String>) -> Vec<(&'static str, Vec<String>)> {
    let labels = keys_in(doc, label());
    let refs = keys_in(
        doc,
        re!(r"\\(?:ref|eqref|pageref|autoref|nameref|vref|cref|Cref|cpageref|labelcref)\*?\s*\{([^}]+)\}"),
    );
    let cites = cites(doc);
    let has = |keys: &Keys, k: &str| keys.iter().any(|(x, _)| x == k);
    let mut dirs = vec![String::new()];
    if let Some(m) = re!(r"\\graphicspath\s*\{((?:\s*\{[^}]*\})+)\s*\}").captures(&doc.text) {
        dirs.extend(re!(r"\{([^}]*)\}").captures_iter(&m[1]).map(|c| c[1].to_owned()));
    }
    let found = |g: &str| {
        dirs.iter().any(|d| {
            doc.root.join(d).join(g).is_file()
                || GFX_EXT
                    .iter()
                    .any(|e| doc.root.join(d).join(format!("{g}{e}")).is_file())
        })
    };
    let mut graphics = vec![];
    for m in gfx().captures_iter(&doc.text) {
        let second = if &m[1] == "plottwo" { m.get(3) } else { None };
        for g in [m.get(2), second].into_iter().flatten().map(|g| g.as_str()) {
            if !g.contains('#') && !found(g.trim()) {
                graphics.push(format!("{g}  {}", doc.at(m.get(0).unwrap().start())));
            }
        }
    }
    let mut stack: Vec<(String, usize)> = vec![];
    let mut envs = vec![];
    for m in re!(r"\\(begin|end)\s*\{([^}]+)\}").captures_iter(&doc.text) {
        let name = m[2].trim().to_owned();
        let pos = m.get(0).unwrap().start();
        if &m[1] == "begin" {
            stack.push((name, pos));
        } else if stack.last().is_some_and(|(n, _)| *n == name) {
            stack.pop();
        } else {
            let open = stack.last().map_or("none", |(n, _)| n.as_str());
            envs.push(format!("\\end{{{name}}} at {}, open: {open}", doc.at(pos)));
        }
    }
    envs.extend(
        stack
            .iter()
            .filter(|(n, _)| n != "document")
            .map(|(n, p)| format!("\\begin{{{n}}} at {} not closed", doc.at(*p))),
    );
    let todo = re!(r"\\todo\b|\bTODO\b|\bFIXME\b|\?\?\?");
    let nocite_all = doc.text.contains("\\nocite{*}");
    let some = |v: &Vec<String>| v.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    vec![
        (
            "undefined \\ref",
            refs.iter()
                .filter(|(k, _)| !has(&labels, k))
                .map(|(k, v)| format!("{k}  {}", some(v)))
                .collect(),
        ),
        (
            "duplicate \\label",
            labels
                .iter()
                .filter(|(_, v)| v.len() > 1)
                .map(|(k, v)| format!("{k}  {}", v.join(", ")))
                .collect(),
        ),
        (
            "citation keys not in .bib",
            cites
                .iter()
                .filter(|(k, _)| !bib_keys.contains(k))
                .map(|(k, v)| format!("{k}  {}", some(v)))
                .collect(),
        ),
        (
            "missing .bib files",
            bib_files(doc)
                .into_iter()
                .filter(|b| !doc.root.join(b).is_file())
                .collect(),
        ),
        ("missing \\input files", doc.missing.clone()),
        ("missing graphics", graphics),
        ("unbalanced environments", envs),
        (
            "TODO / ??? in text",
            doc.lines
                .iter()
                .filter(|l| todo.is_match(&l.code))
                .map(|l| format!("{}:{}: {}", l.file, l.n, head(l.code.trim(), 90)))
                .collect(),
        ),
        (
            "labels never referenced",
            labels
                .iter()
                .filter(|(k, _)| !has(&refs, k) && !k.starts_with("sec") && !k.starts_with("app"))
                .map(|(k, _)| k.clone())
                .collect(),
        ),
        (
            "bib entries never cited",
            if nocite_all {
                vec![]
            } else {
                bib_keys.iter().filter(|k| !has(&cites, k)).cloned().collect()
            },
        ),
    ]
}

/// Innermost project file open at `pos` in a TeX log: TeX prints `(file` on open and `)` on close.
fn file_at(log: &str, pos: usize, files: &HashSet<String>) -> String {
    let mut stack: Vec<Option<&str>> = vec![];
    for m in re!(r"\(([^\s()]*)|\)").captures_iter(&log[..pos]) {
        match m.get(1) {
            Some(f) if re!(r"^\.?/|\.(tex|bbl|sty|cls|clo|cfg|def|fd|aux)$").is_match(f.as_str()) => {
                stack.push(Some(f.as_str()))
            }
            Some(_) => stack.push(None),
            None => {
                stack.pop();
            }
        }
    }
    stack
        .iter()
        .rev()
        .flatten()
        .map(|f| f.trim_start_matches("./"))
        .find(|f| files.contains(*f))
        .unwrap_or("?")
        .to_owned()
}

pub fn compile(root: &Path, main: &str, outdir: &Path, engine: &str) -> Result<String> {
    let (dirs, files) = text::walk(root);
    std::fs::create_dir_all(outdir)?;
    for d in dirs {
        std::fs::create_dir_all(outdir.join(d))?;
    }
    let flag = match engine {
        "pdflatex" => "-pdf",
        "xelatex" => "-xelatex",
        "lualatex" => "-lualatex",
        _ => bail!("engine must be pdflatex, xelatex or lualatex"),
    };
    let (t0, wall) = (Instant::now(), SystemTime::now());
    let p = Command::new("latexmk")
        .args([flag, "-interaction=nonstopmode", "-file-line-error", "-synctex=1"])
        .arg(format!("-outdir={}", outdir.display()))
        .arg(main)
        .current_dir(root)
        .envs([
            ("max_print_line", "10000"),
            ("error_line", "254"),
            ("half_error_line", "238"),
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| anyhow::anyhow!("cannot run latexmk ({e}); install TeX Live"))?;
    let stem = Path::new(main)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let log = read(&outdir.join(format!("{stem}.log"))).unwrap_or_default();
    let pdf = outdir.join(format!("{stem}.pdf"));
    let fresh = pdf
        .metadata()
        .and_then(|m| m.modified())
        .is_ok_and(|t| t >= wall - std::time::Duration::from_secs(1));
    let files: HashSet<String> = files.iter().map(|f| text::rel(f)).collect();
    let mut errors = vec![];
    for c in re!(r"(?m)^(\.?/?[^:\n]+\.(?:tex|sty|cls|bbl)):(\d+): (.*)$").captures_iter(&log) {
        let end = c.get(0).unwrap().end();
        let ctx = re!(r"(?m)^l\.\d+ (.*)$")
            .captures(head(&log[end..], 400))
            .map(|l| format!("   [{}]", head(&l[1], 80)))
            .unwrap_or_default();
        errors.push(format!("{}:{}: {}{ctx}", c[1].trim_start_matches("./"), &c[2], &c[3]));
    }
    let undef: BTreeSet<String> = re!(
        r"(?:LaTeX|Package natbib) Warning: (Reference|Citation) `([^']+)' on page \d+ undefined on input line (\d+)"
    )
    .captures_iter(&log)
    .map(|m| {
        format!(
            "{} '{}'  {}:{}",
            m[1].to_lowercase(),
            &m[2],
            file_at(&log, m.get(0).unwrap().start(), &files),
            &m[3]
        )
    })
    .collect();
    let over: Vec<String> = re!(r"(?m)^Overfull \\[hv]box \(([\d.]+)pt too \w+\) .*?(?:at lines (\d+)--\d+)?$")
        .captures_iter(&log)
        .filter(|m| m[1].parse::<f64>().is_ok_and(|v| v >= 1.0))
        .map(|m| {
            format!(
                "{}pt  {}:{}",
                &m[1],
                file_at(&log, m.get(0).unwrap().start(), &files),
                m.get(2).map_or("?", |l| l.as_str())
            )
        })
        .collect();
    let mut warns: Vec<String> = vec![];
    for m in re!(r"(?m)^(?:LaTeX|Package (\S+)|Class (\S+)) Warning: (.*(?:\n\(\S+\)\s+.*)*)").captures_iter(&log) {
        let msg = re!(r"\n\(\S+\)\s+")
            .replace_all(&m[3], " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if !re!(r"undefined on input line|^There were undefined|Rerun to get|may have changed").is_match(&msg) {
            let w = format!(
                "[{}] {}",
                m.get(1).or(m.get(2)).map_or("LaTeX", |x| x.as_str()),
                head(&msg, 160)
            );
            if !warns.contains(&w) {
                warns.push(w);
            }
        }
    }
    let bib: Vec<String> = read(&outdir.join(format!("{stem}.blg")))
        .unwrap_or_default()
        .lines()
        .filter(|l| {
            ["Warning--", "I couldn't", "I didn't", "Repeated"]
                .iter()
                .any(|p| l.starts_with(p))
        })
        .map(str::to_owned)
        .collect();
    let pages = re!(r"Output written on .*?\((\d+) pages?")
        .captures(&log)
        .map(|c| format!(", {} pages", &c[1]))
        .unwrap_or_default();
    let state = if p.status.success() && errors.is_empty() {
        "ok"
    } else if fresh {
        "errors"
    } else {
        "failed"
    };
    let mut out = vec![
        format!("{state} ({engine}, {} s)", t0.elapsed().as_secs()),
        format!(
            "pdf: {}{pages}",
            if fresh {
                pdf.display().to_string()
            } else {
                "(none)".into()
            }
        ),
        format!("log: {}", outdir.join(format!("{stem}.log")).display()),
    ];
    let undef: Vec<String> = undef.into_iter().collect();
    for (title, items, n) in [
        ("errors", &errors, 30),
        ("undefined", &undef, 30),
        ("bibtex", &bib, 20),
        ("overfull >= 1pt", &over, 10),
        ("warnings", &warns, 10),
    ] {
        if !items.is_empty() {
            out.push(format!("\n{title} ({}):", items.len()));
            out.extend(items.iter().take(n).map(|x| format!("  {x}")));
        }
    }
    if !fresh {
        let all = format!(
            "{}{}",
            String::from_utf8_lossy(&p.stdout),
            String::from_utf8_lossy(&p.stderr)
        );
        out.push(format!("\n{}", tail(&all, 1500)));
    }
    Ok(out.join("\n"))
}
