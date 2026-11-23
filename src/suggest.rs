//! Suggestion mode: edits reach the shared copy as markup a co-author accepts or rejects.
//!
//! Prose: `\sgadd[AB]{new}`, `\sgdel[AB]{old}`, `\sgrep[AB]{new}{old}`.
//! Other blocks (equations, floats, headings, lists, lines with comments) keep the old version
//! as comments and show a tag in the PDF:
//!
//! ```text
//! %% sg[AB] old:
//! %| old line
//! new line
//! \sgnote[AB]
//! %% sg end
//! ```
use crate::text::{balanced, comment_at, group, line_at, opcodes, re, strip_comment};
use anyhow::{Result, anyhow};
use regex::Regex;

pub const PREAMBLE: &str = r"% interleaf suggestions; accept or reject them with the resolve tool
\usepackage{ulem}\normalem\usepackage{xcolor}
\providecommand{\sgtag}[1]{\ifx\relax#1\relax\else\textsuperscript{\tiny#1}\fi}
\providecommand{\sgstrike}{\leavevmode\bgroup\ULdepth=-.55ex\ULset}
\providecommand{\sgadd}[2][]{{\color{blue}#2\sgtag{#1}}}
\providecommand{\sgdel}[2][]{{\color{red}\sgstrike{#2}\sgtag{#1}}}
\providecommand{\sgrep}[3][]{{\color{red}\sgstrike{#3}}{\color{blue}#2\sgtag{#1}}}
\providecommand{\sgbox}[1]{\mbox{#1}}
\providecommand{\sgnote}[1][]{\textsuperscript{\color{blue}[#1 edit]}}
";
const TEXT_ENVS: [&str; 9] = [
    "document",
    "abstract",
    "quote",
    "quotation",
    "center",
    "flushleft",
    "flushright",
    "acknowledgments",
    "acknowledgements",
];

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Blank,
    Prose,
    Block,
    /// Preamble, where no visible tag may go.
    Pre,
}

fn token() -> &'static Regex {
    re!(
        r"\$[^$]*\$|\\[a-zA-Z@]+\*?(?:\[[^\]]*\]|\{(?:[^{}]|\{(?:[^{}]|\{[^{}]*\})*\})*\})*|\\.|\{(?:[^{}]|\{[^{}]*\})*\}|[^\s$\\{}]+|\s+|."
    )
}

fn not_prose(line: &str) -> bool {
    comment_at(line).is_some()
        || re!(r"\\begin|\\end|\\\[|\\\]|\$\$|\\item\b|\\(?:sub)*section|\\chapter|\\paragraph\b|\\label|\\caption|\\input|\\include|\\\\|\\sg(?:add|del|rep|note)\b|^\s*\\[a-zA-Z@]+\*?(?:\[[^\]]*\])?(?:\{[^{}]*\})*\s*$").is_match(line)
}

fn is_space(t: &str) -> bool {
    !t.is_empty() && t.chars().all(char::is_whitespace)
}

/// Argument in braces at `text[i]`, after optional spaces: (content, offset after it).
fn braced(text: &str, i: usize) -> Result<(&str, usize)> {
    let start = i + text[i..].len() - text[i..].trim_start().len();
    group(text, start)
        .map(|(r, end)| (&text[r], end))
        .ok_or_else(|| anyhow!("unbalanced braces after line {}", line_at(text, i)))
}

/// Markup commands `\sgadd[who]{`, `\sgdel`, `\sgrep`: (start, end of name and author, kind, author).
fn commands(text: &str) -> Vec<(usize, usize, String, String)> {
    re!(r"\\sg(add|del|rep)(?:\[([^\]]*)\])?")
        .captures_iter(text)
        .filter(|c| text[c.get(0).unwrap().end()..].trim_start().starts_with('{'))
        .map(|c| {
            let m = c.get(0).unwrap();
            (
                m.start(),
                m.end(),
                c[1].to_owned(),
                c.get(2).map_or("", |a| a.as_str()).to_owned(),
            )
        })
        .collect()
}

/// A block suggestion: the whole span, author, old text (uncommented), new text.
struct Block {
    start: usize,
    end: usize,
    who: String,
    old: String,
    new: String,
}

fn blocks(text: &str) -> Vec<Block> {
    let mut out = vec![];
    let starts: Vec<usize> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i + 1))
        .filter(|&i| i < text.len())
        .collect();
    let line = |k: usize| &text[starts[k]..starts.get(k + 1).copied().unwrap_or(text.len())];
    let mut k = 0;
    while k < starts.len() {
        let Some(c) = re!(r"^%% sg\[([^\]]*)\] old:\n$").captures(line(k)) else {
            k += 1;
            continue;
        };
        let (start, who) = (starts[k], c[1].to_owned());
        let mut j = k + 1;
        let mut old = String::new();
        while j < starts.len() && line(j).starts_with("%|") {
            old += line(j).get(3..).unwrap_or("").trim_end_matches('\n');
            old.push('\n');
            j += 1;
        }
        let body = starts.get(j).copied().unwrap_or(text.len());
        let Some(e) = (j..starts.len()).find(|&e| line(e).trim_end_matches('\n') == "%% sg end") else {
            k = j;
            continue;
        };
        let mut new = text[body..starts[e]].to_owned();
        if let Some(n) = new.strip_suffix(&format!("\\sgnote[{who}]\n")) {
            new = n.to_owned();
        }
        out.push(Block {
            start,
            end: starts[e] + line(e).len(),
            who,
            old,
            new,
        });
        k = e + 1;
    }
    out
}

fn units(text: &str) -> Vec<(Kind, String)> {
    let mut out: Vec<(Kind, String)> = vec![];
    let mut stack: Vec<String> = vec![];
    let mut preamble = text.contains("\\documentclass");
    for line in text.split_inclusive('\n') {
        let kind = if line.trim().is_empty() {
            Kind::Blank
        } else if preamble {
            Kind::Pre
        } else if stack.iter().all(|e| TEXT_ENVS.contains(&e.as_str())) && !not_prose(line) {
            Kind::Prose
        } else {
            Kind::Block
        };
        for m in re!(r"\\(begin|end)\s*\{([^}]+)\}").captures_iter(strip_comment(line)) {
            if &m[1] == "begin" {
                stack.push(m[2].to_owned());
                preamble = preamble && &m[2] != "document";
            } else if stack.last().is_some_and(|e| e == &m[2]) {
                stack.pop();
            }
        }
        match out.last_mut() {
            Some((k, t)) if *k == kind => t.push_str(line),
            _ => out.push((kind, line.to_owned())),
        }
    }
    out
}

/// Protect commands with arguments in struck-out text (natbib citations and footnotes break ulem).
fn boxed(s: &str) -> String {
    token()
        .find_iter(s)
        .map(|t| t.as_str())
        .map(|t| {
            let command = re!(r"^\\[a-zA-Z@]").is_match(t) && t.contains(['[', '{']);
            if command {
                format!("\\sgbox{{{t}}}")
            } else {
                t.to_owned()
            }
        })
        .collect()
}

fn unbox(s: &str) -> Result<String> {
    let mut s = s.to_owned();
    while let Some(i) = s.find("\\sgbox{") {
        let (inner, end) = braced(&s, i + 6)?;
        s = format!("{}{}{}", &s[..i], inner, &s[end..]);
    }
    Ok(s)
}

fn inline(old: &str, new: &str, who: &str) -> Option<String> {
    let a: Vec<&str> = token().find_iter(old).map(|m| m.as_str()).collect();
    let b: Vec<&str> = token().find_iter(new).map(|m| m.as_str()).collect();
    fn key<'a>(t: &&'a str) -> &'a str {
        if is_space(t) { " " } else { t }
    }
    let (ka, kb): (Vec<&str>, Vec<&str>) = (a.iter().map(key).collect(), b.iter().map(key).collect());
    let mut ops: Vec<(bool, std::ops::Range<usize>, std::ops::Range<usize>)> = vec![];
    for (equal, o, n) in opcodes(&ka, &kb) {
        // fold a change, a one-word gap and the next change into one, so the markup reads as phrases
        let gap_is_word = ops.len() >= 2
            && ops
                .last()
                .is_some_and(|(eq, go, _)| *eq && a[go.clone()].iter().filter(|t| !is_space(t)).count() <= 1);
        if !equal && gap_is_word {
            ops.pop();
            let last = ops.last_mut().unwrap();
            *last = (false, last.1.start..o.end, last.2.start..n.end);
        } else {
            ops.push((equal, o, n));
        }
    }
    let mut out = String::new();
    for (equal, o, n) in ops {
        let (os, ns) = (a[o].concat(), b[n].concat());
        if equal || (os.trim().is_empty() && ns.trim().is_empty()) {
            out += &ns;
            continue;
        }
        let r = if ns.trim().is_empty() { &os } else { &ns };
        let (pre, post) = (&r[..r.len() - r.trim_start().len()], &r[r.trim_end().len()..]);
        let (o, n) = (os.trim(), ns.trim());
        if !(balanced(o) && balanced(n)) {
            return None;
        }
        let mark = match (o.is_empty(), n.is_empty()) {
            (true, _) => format!("\\sgadd[{who}]{{{n}}}"),
            (_, true) => format!("\\sgdel[{who}]{{{}}}", boxed(o)),
            _ => format!("\\sgrep[{who}]{{{n}}}{{{}}}", boxed(o)),
        };
        out += &format!("{pre}{mark}{post}");
    }
    Some(out)
}

fn block(old: &str, new: &str, who: &str, tag: bool) -> String {
    let mut out = format!("%% sg[{who}] old:\n");
    for l in old.lines() {
        out += &if l.is_empty() {
            "%|\n".to_owned()
        } else {
            format!("%| {l}\n")
        };
    }
    out += new;
    if !new.is_empty() && !new.ends_with('\n') {
        out.push('\n');
    }
    if tag {
        out += &format!("\\sgnote[{who}]\n");
    }
    out + "%% sg end\n"
}

/// `new` with every change against `old` written as a suggestion by `who`.
pub fn mark(old: &str, new: &str, who: &str) -> String {
    let (a, b) = (units(old), units(new));
    let mut out = String::new();
    let one = |ou: Option<&(Kind, String)>, nu: Option<&(Kind, String)>| match (ou, nu) {
        (None, Some((k, t))) => match k {
            Kind::Blank => t.clone(),
            Kind::Prose if balanced(t) => format!("\\sgadd[{who}]{{{}}}\n", t.trim_end_matches('\n')),
            _ => block("", t, who, *k != Kind::Pre),
        },
        (Some((k, t)), None) => match k {
            Kind::Blank => t.clone(),
            Kind::Prose if balanced(t) => format!("\\sgdel[{who}]{{{}}}\n", boxed(t.trim_end_matches('\n'))),
            _ => block(t, "", who, *k != Kind::Pre),
        },
        (Some((ko, to)), Some((kn, tn))) => {
            let marked = (*ko == Kind::Prose && *kn == Kind::Prose)
                .then(|| inline(to, tn, who))
                .flatten();
            match marked {
                Some(m) => m,
                None if *ko == Kind::Blank && *kn == Kind::Blank => tn.clone(),
                None => block(to, tn, who, *ko != Kind::Pre && *kn != Kind::Pre),
            }
        }
        (None, None) => String::new(),
    };
    let ta: Vec<&str> = a.iter().map(|u| u.1.as_str()).collect();
    let tb: Vec<&str> = b.iter().map(|u| u.1.as_str()).collect();
    for (equal, o, n) in opcodes(&ta, &tb) {
        if equal {
            out.extend(b[n].iter().map(|u| u.1.as_str()));
            continue;
        }
        let (olds, news) = (&a[o], &b[n]);
        let ka: Vec<Kind> = olds.iter().map(|u| u.0).collect();
        let kb: Vec<Kind> = news.iter().map(|u| u.0).collect();
        for (_, k, l) in opcodes(&ka, &kb) {
            for i in 0..k.len().max(l.len()) {
                out += &one(
                    (i < k.len()).then(|| &olds[k.start + i]),
                    (i < l.len()).then(|| &news[l.start + i]),
                );
            }
        }
    }
    out
}

/// Accept or reject the suggestions of `who` (all authors if None); returns the text and how
/// many were resolved.
pub fn resolve(text: &str, accept: bool, who: Option<&str>) -> Result<(String, usize)> {
    let mut n = 0;
    let (mut out, mut pos) = (String::new(), 0);
    for b in blocks(text) {
        if who.is_some_and(|w| w != b.who) {
            continue;
        }
        out += &text[pos..b.start];
        out += if accept { &b.new } else { &b.old };
        pos = b.end;
        n += 1;
    }
    let mut text = out + &text[pos..];
    for (start, end, kind, author) in commands(&text).into_iter().rev() {
        if who.is_some_and(|w| w != author) {
            continue;
        }
        let (first, mut stop) = braced(&text, end)?;
        let first = first.to_owned();
        let second = if kind == "rep" {
            let (s, e) = braced(&text, stop)?;
            stop = e;
            s.to_owned()
        } else {
            String::new()
        };
        let (new, old) = match kind.as_str() {
            "add" => (first, String::new()),
            "del" => (String::new(), first),
            _ => (first, second),
        };
        let replacement = if accept { new } else { unbox(&old)? };
        // a removed phrase between two spaces leaves one space
        let start = if replacement.is_empty() && text[..start].ends_with(' ') && text[stop..].starts_with([' ', '\n']) {
            start - 1
        } else {
            start
        };
        text.replace_range(start..stop, &replacement);
        n += 1;
    }
    Ok((text, n))
}

/// What to push in suggestion mode: the author's earlier suggestions in `upstream` and the new
/// edits in `local`, as one set of suggestions against the text before any of them.
pub fn propose(upstream: &str, local: &str, who: &str) -> Result<String> {
    Ok(mark(
        &resolve(upstream, false, Some(who))?.0,
        &resolve(local, true, Some(who))?.0,
        who,
    ))
}

/// Pending suggestions as `line: [author] change`.
pub fn listing(text: &str, who: Option<&str>) -> Result<Vec<String>> {
    let mut rows = vec![];
    for b in blocks(text).into_iter().filter(|b| who.is_none_or(|w| w == b.who)) {
        let first = b
            .new
            .trim()
            .lines()
            .next()
            .map_or("(removed)".to_owned(), |l| crate::text::head(l, 70).to_owned());
        rows.push((b.start, format!("[{}] block: {first}", b.who)));
    }
    for (start, end, kind, author) in commands(text).into_iter().filter(|c| who.is_none_or(|w| w == c.3)) {
        let (first, stop) = braced(text, end)?;
        let preview = match kind.as_str() {
            "add" => format!("+ {first}"),
            "del" => format!("- {}", unbox(first)?),
            _ => format!("{}  ->  {first}", unbox(braced(text, stop)?.0)?),
        };
        let preview = preview.split_whitespace().collect::<Vec<_>>().join(" ");
        rows.push((start, format!("[{author}] {}", crate::text::head(&preview, 110))));
    }
    rows.sort();
    Ok(rows
        .into_iter()
        .map(|(pos, r)| format!("{}: {r}", line_at(text, pos)))
        .collect())
}

pub fn with_preamble(main: &str) -> String {
    match main.find("\\begin{document}") {
        Some(i) if !main.contains("\\providecommand{\\sgadd}") => format!("{}{PREAMBLE}{}", &main[..i], &main[i..]),
        _ => main.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "\\section{Intro}\nThe probe reached the comet.\nIt was seen \\citep{Smith2020} from the ground.\nSecond line.\n\n\\begin{equation}\nv = 1\n\\end{equation}\n";

    #[test]
    fn prose_and_block_round_trip() {
        let new = OLD
            .replace("probe reached", "small probe finally reached")
            .replace("v = 1", "v = 2")
            .replace(" \\citep{Smith2020} from", " from");
        let marked = mark(OLD, &new, "AB");
        assert!(marked.contains("\\sgrep[AB]{small probe finally}{probe}"), "{marked}");
        assert!(marked.contains("\\sgdel[AB]{\\sgbox{\\citep{Smith2020}}}"), "{marked}");
        assert!(
            marked.contains("%% sg[AB] old:\n") && marked.contains("\\sgnote[AB]\n"),
            "{marked}"
        );
        assert_eq!(resolve(&marked, false, Some("AB")).unwrap().0, OLD);
        assert_eq!(resolve(&marked, true, None).unwrap().0, new);
        assert_eq!(listing(&marked, Some("AB")).unwrap().len(), 3);
    }

    #[test]
    fn second_round_stays_one_set() {
        let first = OLD.replace("Second line.", "Second line, edited.");
        let upstream = mark(OLD, &first, "AB");
        let local = upstream.replace("line, edited.", "line, edited twice.");
        let pushed = propose(&upstream, &local, "AB").unwrap();
        assert_eq!(resolve(&pushed, false, Some("AB")).unwrap().0, OLD);
        assert!(resolve(&pushed, true, Some("AB")).unwrap().0.contains("edited twice"));
    }

    #[test]
    fn other_authors_untouched() {
        let theirs = mark(OLD, &OLD.replace("Second", "2nd"), "CD");
        let (text, n) = resolve(&theirs, true, Some("AB")).unwrap();
        assert_eq!((text.as_str(), n), (theirs.as_str(), 0));
    }
}
