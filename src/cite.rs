//! Citations: check .bib entries against NASA ADS and fetch BibTeX from it.
use crate::config::ADS_TOKEN;
use crate::text::{group, head, re};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

const API: &str = "https://api.adsabs.harvard.edu/v1";
const FIELDS: &str = "bibcode,doi,title,author,year,pub,doctype,identifier";

/// A .bib entry: type, key, lower-case fields.
pub struct Entry {
    pub kind: String,
    pub key: String,
    fields: HashMap<String, String>,
}

impl Entry {
    pub fn get(&self, field: &str) -> &str {
        self.fields.get(field).map_or("", |v| v.as_str())
    }
}

pub fn parse_bib(text: &str) -> Vec<Entry> {
    let mut out = vec![];
    for c in re!(r"@(\w+)\s*\{").captures_iter(text) {
        let kind = c[1].to_lowercase();
        if ["string", "preamble", "comment"].contains(&kind.as_str()) {
            continue;
        }
        let open = c.get(0).unwrap().end() - 1;
        let inner = group(text, open).map_or(open + 1..text.len(), |(r, _)| r);
        let (key, body) = text[inner].split_once(',').unwrap_or((&text[open + 1..], ""));
        let mut fields = HashMap::new();
        let mut pos = 0;
        while let Some(f) = re!(r"^[\s,]*(\w+)\s*=\s*").captures(&body[pos..]) {
            let v = pos + f.get(0).unwrap().end();
            let Some(first) = body[v..].chars().next() else { break };
            let (val, next) = match first {
                '{' => match group(body, v) {
                    Some((r, end)) => (&body[r], end),
                    None => (&body[v + 1..], body.len()),
                },
                '"' => {
                    let k = body[v + 1..].find('"').map_or(body.len(), |k| v + 1 + k);
                    (&body[v + 1..k], (k + 1).min(body.len()))
                }
                _ => {
                    let len = body[v..]
                        .find([',', '}', ' ', '\t', '\n', '\r'])
                        .unwrap_or(body.len() - v);
                    (&body[v..v + len], v + len)
                }
            };
            fields.entry(f[1].to_lowercase()).or_insert_with(|| val.to_owned());
            pos = next;
        }
        out.push(Entry {
            kind,
            key: key.trim().to_owned(),
            fields,
        });
    }
    out
}

fn plain(s: &str) -> String {
    let s = re!(r"\\[a-zA-Z]+\s*|[{}$\\]").replace_all(s, " ").to_lowercase();
    re!(r"[^a-z0-9]+")
        .replace_all(&s, " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Share of the shorter title's words found in the other (shortened titles still match).
fn same_title(a: &str, b: &str) -> f64 {
    let (pa, pb) = (plain(a), plain(b));
    let x: std::collections::HashSet<&str> = pa.split(' ').filter(|w| !w.is_empty()).collect();
    let y: std::collections::HashSet<&str> = pb.split(' ').filter(|w| !w.is_empty()).collect();
    if x.is_empty() || y.is_empty() {
        return 0.0;
    }
    x.intersection(&y).count() as f64 / x.len().min(y.len()) as f64
}

fn surname(author: &str) -> String {
    let first = re!(r"\s+and\s+").split(author).next().unwrap_or("");
    let name = match first.split_once(',') {
        Some((last, _)) => last,
        None => first.split_whitespace().last().unwrap_or(""),
    };
    plain(name)
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .build()
        .into()
}

fn token() -> Result<String> {
    ADS_TOKEN.get().ok_or_else(|| ADS_TOKEN.missing())
}

fn search(q: &str, rows: usize) -> Result<Vec<Value>> {
    let res: Value = agent()
        .get(format!("{API}/search/query"))
        .query("q", q)
        .query("fl", FIELDS)
        .query("rows", rows.to_string())
        .header("Authorization", format!("Bearer {}", token()?))
        .call()
        .context("ADS search failed (check the token and the network)")?
        .body_mut()
        .read_json()?;
    Ok(res["response"]["docs"].as_array().cloned().unwrap_or_default())
}

fn s<'a>(d: &'a Value, field: &str) -> &'a str {
    match &d[field] {
        Value::Array(a) => a.first().and_then(Value::as_str).unwrap_or(""),
        v => v.as_str().unwrap_or(""),
    }
}

fn arxiv_id(e: &Entry) -> Option<String> {
    let re = re!(r"(?i)(?:arXiv:)?(\d{4}\.\d{4,5}|[a-z\-]+/\d{7})(?:v\d+)?$");
    ["eprint", "arxiv", "journal", "note"]
        .iter()
        .find_map(|f| re.captures(e.get(f).trim()).map(|c| c[1].to_owned()))
}

fn journal_doi(d: &Value) -> Option<&str> {
    d["doi"]
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .find(|x| !x.starts_with("10.48550/"))
}

/// What ADS has for an entry.
pub enum Record {
    Found { doc: Value, via_title: bool },
    Missing { candidates: Vec<Value> },
}

/// ADS record for each entry: by DOI, then arXiv id, then title. An entry that matches nothing
/// gets candidates by its first author and year.
pub fn lookup(entries: &[Entry]) -> Result<BTreeMap<String, Record>> {
    let mut found = BTreeMap::new();
    let by_doi: HashMap<String, &str> = entries
        .iter()
        .filter(|e| !e.get("doi").is_empty())
        .map(|e| (e.get("doi").trim().to_lowercase(), e.key.as_str()))
        .collect();
    let dois: Vec<&String> = by_doi.keys().collect();
    for chunk in dois.chunks(40) {
        let q = chunk
            .iter()
            .map(|x| format!("doi:\"{x}\""))
            .collect::<Vec<_>>()
            .join(" OR ");
        for d in search(&q, 200)? {
            for x in d["doi"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                if let Some(key) = by_doi.get(&x.to_lowercase()) {
                    found.insert(
                        key.to_string(),
                        Record::Found {
                            doc: d.clone(),
                            via_title: false,
                        },
                    );
                }
            }
        }
    }
    let one = |e: &Entry| -> Result<Record> {
        let year = if e.get("year").is_empty() {
            String::new()
        } else {
            format!(" year:{}", e.get("year"))
        };
        let aid = arxiv_id(e);
        let tries = match &aid {
            Some(id) => vec![format!("identifier:\"arXiv:{id}\"")],
            None => {
                // the phrase query is fast; the word query (slow at ADS) only when the phrase misses
                let title = plain(e.get("title"));
                let words: Vec<&str> = title.split(' ').filter(|w| w.len() > 2).take(8).collect();
                if words.is_empty() {
                    vec![]
                } else {
                    vec![
                        format!("title:\"{title}\""),
                        format!("title:({}){year}", words.join(" ")),
                    ]
                }
            }
        };
        for q in tries {
            let docs = search(&q, 5)?;
            let score = |d: &Value| same_title(e.get("title"), s(d, "title"));
            let best = docs.into_iter().max_by(|x, y| score(x).total_cmp(&score(y)));
            if let Some(d) = best.filter(|d| aid.is_some() || score(d) >= 0.5) {
                return Ok(Record::Found {
                    doc: d,
                    via_title: !e.get("doi").is_empty(),
                });
            }
        }
        let name = surname(e.get("author"));
        let candidates = if name.is_empty() {
            vec![]
        } else {
            search(&format!("first_author:\"{name}\"{year}"), 3)?
        };
        Ok(Record::Missing { candidates })
    };
    let rest: Vec<&Entry> = entries.iter().filter(|e| !found.contains_key(&e.key)).collect();
    let (rest, one) = (&rest, &one);
    let results: Vec<(String, Result<Record>)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..6)
            .map(|w| {
                scope.spawn(move || {
                    rest.iter()
                        .skip(w)
                        .step_by(6)
                        .map(|e| (e.key.clone(), one(e)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    for (key, r) in results {
        found.insert(key, r?);
    }
    Ok(found)
}

pub fn check(entries: &[Entry], records: &BTreeMap<String, Record>) -> Vec<String> {
    let mut problems = vec![];
    for e in entries {
        let k = &e.key;
        let d = match records.get(k) {
            Some(Record::Found { doc, via_title }) => {
                if *via_title {
                    problems.push(format!(
                        "{k}: doi {} not in ADS; by title it is {}, doi {}",
                        e.get("doi"),
                        s(doc, "bibcode"),
                        journal_doi(doc).unwrap_or("none")
                    ));
                }
                doc
            }
            other => {
                let near: Vec<String> = match other {
                    Some(Record::Missing { candidates }) => candidates
                        .iter()
                        .map(|c| format!("{} \"{}\"", s(c, "bibcode"), head(s(c, "title"), 60)))
                        .collect(),
                    _ => vec![],
                };
                let near = if near.is_empty() {
                    String::new()
                } else {
                    format!(" (same author and year: {})", near.join("; "))
                };
                problems.push(format!("{k}: not in ADS{near}"));
                continue;
            }
        };
        let title = s(d, "title");
        if same_title(e.get("title"), title) < 0.8 {
            problems.push(format!(
                "{k}: title differs; ADS {}: \"{}\"",
                s(d, "bibcode"),
                head(title, 90)
            ));
        }
        let first = s(d, "author");
        if !first.is_empty() && !plain(first).contains(&surname(e.get("author"))) {
            problems.push(format!(
                "{k}: first author '{}' vs ADS '{first}'",
                surname(e.get("author"))
            ));
        }
        let preprint = plain(&format!("{} {}", e.get("journal"), e.kind)).contains("arxiv")
            || (e.get("journal").is_empty() && arxiv_id(e).is_some());
        let published = s(d, "doctype") == "article" && !plain(s(d, "pub")).contains("arxiv");
        if preprint && published {
            problems.push(format!(
                "{k}: preprint now published: {} {}, doi {}",
                s(d, "pub"),
                s(d, "year"),
                journal_doi(d).unwrap_or("none")
            ));
        } else if !e.get("year").is_empty() && !s(d, "year").is_empty() && e.get("year").trim() != s(d, "year") {
            problems.push(format!("{k}: year {} vs ADS {}", e.get("year"), s(d, "year")));
        }
        if e.get("doi").is_empty()
            && let Some(doi) = journal_doi(d)
        {
            problems.push(format!("{k}: no doi in .bib; ADS has {doi}"));
        }
    }
    problems
}

/// ADS records for a DOI, arXiv id, bibcode, ADS URL or free query.
pub fn find(identifier: &str) -> Result<Vec<Value>> {
    let mut s = identifier.trim().to_owned();
    if let Some(c) = re!(r"abs/([^/\s]+)").captures(&s) {
        s = c[1].replace("%26", "&");
    }
    let lower = s.to_lowercase();
    if lower.starts_with("10.") || lower.starts_with("doi:") {
        return search(&format!("doi:\"{}\"", s.trim_start_matches("doi:")), 1);
    }
    if let Some(c) = re!(r"(?i)^(?:arXiv:)?(\d{4}\.\d{4,5}|[a-z\-]+/\d{7})(?:v\d+)?$").captures(&s) {
        return search(&format!("identifier:\"arXiv:{}\"", &c[1]), 1);
    }
    if re!(r"^\d{4}[A-Za-z&.]{5}[\w.]{9}[A-Z.]$").is_match(&s) {
        return search(&format!("bibcode:\"{s}\""), 1);
    }
    search(&s, 5)
}

pub fn bibtex(bibcode: &str, key: Option<&str>) -> Result<String> {
    let res: Value = agent()
        .post(format!("{API}/export/bibtex"))
        .header("Authorization", format!("Bearer {}", token()?))
        .send_json(json!({ "bibcode": [bibcode] }))
        .context("ADS export failed")?
        .body_mut()
        .read_json()?;
    let text = res["export"].as_str().unwrap_or("").trim().to_owned();
    Ok(match key {
        Some(k) => re!(r"^(@\w+\{)[^,]+,")
            .replace(&text, format!("${{1}}{k},").as_str())
            .into_owned(),
        None => text,
    })
}

pub fn label(d: &Value) -> String {
    let authors: Vec<&str> = d["author"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .take(2)
        .collect();
    format!(
        "{}  {} {}  {}",
        s(d, "bibcode"),
        authors.join(", "),
        s(d, "year"),
        head(s(d, "title"), 80)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const BIB: &str = "@article{Smith2020, author={Smith, J. A.}, title={Dust trails of short-period comets}, journal={Icarus}, year={2020}}\n\
        @article{Jones2021, author={Jones, Mary and Lee, Ann}, title={Radar shapes of small asteroids}, journal={AJ}, year={2021}}\n\
        @article{Gone2099, author = \"Nobody, A.\", title = {Never written}, year = 2099}\n";

    #[test]
    fn parses_fields() {
        let e = parse_bib(BIB);
        assert_eq!(e.len(), 3);
        assert_eq!((e[1].key.as_str(), e[1].get("year")), ("Jones2021", "2021"));
        assert_eq!((e[2].get("author"), e[2].get("year")), ("Nobody, A.", "2099"));
        assert_eq!(surname(e[1].get("author")), "jones");
    }

    #[test]
    fn checks_records() {
        let entries = parse_bib(BIB);
        let mut records = BTreeMap::new();
        let smith = json!({"bibcode": "2020Icar..340a3600S", "title": ["Dust trails of short-period comets observed in the infrared"],
            "author": ["Smith, John A."], "year": "2020", "doctype": "article", "pub": "Icarus", "doi": ["10.1016/j.icarus.2020.000001"]});
        let other = json!({"bibcode": "2019Icar..321..100O", "title": ["Thermal inertia of near-Earth asteroids"],
            "author": ["Other, A."], "year": "2019", "doctype": "article", "pub": "Icarus", "doi": ["10.1016/j.icarus.2019.000002"]});
        records.insert(
            "Smith2020".to_owned(),
            Record::Found {
                doc: smith,
                via_title: false,
            },
        );
        records.insert(
            "Jones2021".to_owned(),
            Record::Found {
                doc: other,
                via_title: false,
            },
        );
        records.insert("Gone2099".to_owned(), Record::Missing { candidates: vec![] });
        let t = check(&entries, &records).join("\n");
        for want in [
            "Jones2021: title differs",
            "Jones2021: first author",
            "Jones2021: year 2021 vs ADS 2019",
            "Smith2020: no doi",
            "Gone2099: not in ADS",
        ] {
            assert!(t.contains(want), "{want}\n{t}");
        }
        assert!(!t.contains("Smith2020: title"), "{t}");
    }
}
