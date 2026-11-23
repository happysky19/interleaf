//! Wording lint and journal limits (data in rules.toml).
use crate::config::Settings;
use crate::tex::{Doc, word_count};
use crate::text::{env_spans, re};
use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::LazyLock;

#[derive(Deserialize)]
struct Rules {
    lint: BTreeMap<String, Group>,
    journal: BTreeMap<String, Journal>,
}

#[derive(Deserialize)]
struct Group {
    why: String,
    patterns: Vec<String>,
}

#[derive(Deserialize)]
struct Journal {
    name: String,
    source: String,
    abstract_words: Option<usize>,
    text_words: Option<usize>,
    display_items: Option<usize>,
    keypoints: Option<usize>,
    keypoint_chars: Option<usize>,
    pls_words: Option<usize>,
    significance_words: Option<usize>,
    publication_units: Option<f64>,
    #[serde(default)]
    require: Vec<String>,
    #[serde(default)]
    recommend: Vec<String>,
}

static RULES: LazyLock<Rules> = LazyLock::new(|| toml::from_str(include_str!("rules.toml")).expect("rules.toml"));

const FLOATS: [&str; 3] = ["figure", "table", "deluxetable"];

pub fn lint(doc: &Doc, s: &Settings) -> Result<Vec<String>> {
    let mut groups: Vec<(&str, Vec<Regex>)> = vec![];
    let compile = |p: &String| Regex::new(&format!("(?i){p}")).with_context(|| format!("wording pattern {p:?}"));
    for g in RULES.lint.values() {
        groups.push((
            &g.why,
            g.patterns
                .iter()
                .filter(|p| !s.lint.ignore.contains(p))
                .map(compile)
                .collect::<Result<_>>()?,
        ));
    }
    groups.push(("project rule", s.lint.extra.iter().map(compile).collect::<Result<_>>()?));
    let begin = doc
        .lines
        .iter()
        .position(|l| l.code.contains("\\begin{document}"))
        .unwrap_or(0);
    let mut hits = vec![];
    for l in &doc.lines[begin..] {
        for (why, pats) in &groups {
            for p in pats {
                hits.extend(
                    p.find_iter(&l.code)
                        .map(|m| format!("{}:{}: '{}'  ({why})", l.file, l.n, m.as_str())),
                );
            }
        }
    }
    Ok(hits)
}

fn cut(text: &str, spans: Vec<(usize, usize, std::ops::Range<usize>)>) -> String {
    let mut out = text.to_owned();
    for (start, end, _) in spans.into_iter().rev() {
        out.replace_range(start..end, " ");
    }
    out
}

/// Main text: body without abstract, floats, and everything from appendix, Methods,
/// acknowledgments or references on.
fn main_text(doc: &Doc) -> String {
    let body = doc.body();
    let end = re!(r"(?i)\\appendix|\\section\*?\{(?:Methods|Materials and Methods|Acknowledg|Open Research|Data Availability)|\\begin\{acknowledg|\\bibliography\{|\\begin\{thebibliography\}|\\acknowledg")
        .find(body)
        .map_or(body.len(), |m| m.start());
    let body = cut(&body[..end], env_spans(&body[..end], &["abstract"]));
    cut(&body, env_spans(&body, &FLOATS))
}

/// Text of the section with this title, up to the next section or the end of the document.
fn section_text<'a>(doc: &'a Doc, title: &str) -> Option<&'a str> {
    let head = Regex::new(&format!(r"(?i)\\section\*?\{{{}\}}", regex::escape(title))).unwrap();
    let start = head.find(&doc.text)?.end();
    let rest = &doc.text[start..];
    let stop = [rest.find("\\section"), rest.find("\\end{document}")]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(rest.len());
    Some(&rest[..stop])
}

pub fn journal(doc: &Doc, name: &str) -> Vec<String> {
    let Some(j) = RULES.journal.get(name) else {
        return vec![format!(
            "unknown journal '{name}'; known: {}",
            RULES.journal.keys().cloned().collect::<Vec<_>>().join(", ")
        )];
    };
    let mut out = vec![format!("{}  ({})", j.name, j.source)];
    let row = |out: &mut Vec<String>, label: &str, value: f64, limit: f64, approx: bool| {
        let over = if value > limit { "  OVER" } else { "" };
        out.push(format!(
            "  {label}: {}{value} / {limit}{over}",
            if approx { "~" } else { "" }
        ));
    };
    if let Some(limit) = j.abstract_words {
        match doc.env("abstract") {
            Some(ab) => row(&mut out, "abstract words", word_count(ab) as f64, limit as f64, false),
            None => out.push("  abstract: none found".into()),
        }
    }
    if let Some(limit) = j.text_words {
        row(
            &mut out,
            "main text words",
            word_count(&main_text(doc)) as f64,
            limit as f64,
            true,
        );
    }
    let items = env_spans(&doc.text, &FLOATS).len();
    if let Some(limit) = j.display_items {
        row(&mut out, "figures + tables", items as f64, limit as f64, false);
    }
    if let Some(limit) = j.keypoints {
        let env = doc.env("keypoints").unwrap_or("");
        let points: Vec<&str> = env.split("\\item").skip(1).collect();
        row(&mut out, "key points", points.len() as f64, limit as f64, false);
        let max = j.keypoint_chars.unwrap_or(usize::MAX);
        for (i, k) in points.iter().enumerate() {
            let chars = re!(r"\\[a-zA-Z]+|[{}$]")
                .replace_all(k, "")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .count();
            if chars > max {
                out.push(format!("  key point {}: {chars} / {max} characters  OVER", i + 1));
            }
        }
    }
    if let Some(limit) = j.pls_words {
        match section_text(doc, "Plain Language Summary") {
            Some(pls) => row(
                &mut out,
                "plain language summary words",
                word_count(pls) as f64,
                limit as f64,
                false,
            ),
            None => out.push("  plain language summary: none found".into()),
        }
    }
    if let Some(limit) = j.significance_words {
        let m = re!(r"(?sm)\\significancestatement\{(.*?)\}\s*$")
            .captures(&doc.text)
            .or_else(|| re!(r"(?s)\\begin\{significance(?:statement)?\}(.*?)\\end").captures(&doc.text));
        match m {
            Some(m) => row(
                &mut out,
                "significance words",
                word_count(&m[1]) as f64,
                limit as f64,
                false,
            ),
            None => out.push("  significance statement: none found".into()),
        }
    }
    if let Some(limit) = j.publication_units {
        let body = cut(doc.body(), env_spans(doc.body(), &["keypoints"]));
        let units = ((word_count(&body) as f64 / 500.0 + items as f64) * 10.0).round() / 10.0;
        row(
            &mut out,
            "publication units (words/500 + figures + tables)",
            units,
            limit,
            true,
        );
    }
    let low = doc.text.to_lowercase();
    out.extend(
        j.require
            .iter()
            .filter(|r| !low.contains(r.as_str()))
            .map(|r| format!("  missing: '{r}'")),
    );
    out.extend(
        j.recommend
            .iter()
            .filter(|r| !doc.text.contains(r.as_str()))
            .map(|r| format!("  recommended, not found: {r}")),
    );
    out
}
