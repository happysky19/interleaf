//! Numbers that come from result files live in numbers.tex, one macro per line, each line
//! remembering where its value came from:
//!
//! ```tex
//! \newcommand{\TailPA}{2.5}% src=fits/summary.json key=tail.pa fmt={:.1f}
//! ```
//!
//! so `numbers` can tell which values in the paper no longer match the results.
use crate::text::{re, read, write};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::path::Path;

const FILE: &str = "numbers.tex";
const HEADER: &str =
    "% Values from result files (src, key, format). Change them with the numbers tools, not by hand.\n";

pub struct Row {
    pub name: String,
    pub value: String,
    src: String,
    key: String,
    fmt: String,
}

/// A value formatted Python-style: `{}`, `{:.2f}`, `{:.3e}`, `{:.1%}`, `{:d}`, with text around it.
fn format(fmt: &str, v: &Value) -> Result<String> {
    let c = re!(r"^(.*)\{(?::(?:\.(\d+))?([fed%])?)?\}(.*)$")
        .captures(fmt)
        .ok_or_else(|| anyhow!("format {fmt:?}: use {{}} or {{:.2f}}, {{:.3e}}, {{:.1%}}, {{:d}}"))?;
    let (pre, post) = (&c[1], &c[4]);
    let digits: Option<usize> = c.get(2).map(|d| d.as_str().parse().unwrap());
    let kind = c.get(3).map(|k| k.as_str());
    let body = match (kind, v) {
        (None, Value::String(s)) => s.clone(),
        (None, Value::Number(n)) if digits.is_none() => n.to_string(),
        (_, Value::Number(n)) => {
            let x = n.as_f64().unwrap();
            let p = digits.unwrap_or(6);
            match kind {
                Some("d") => format!("{}", x.round() as i64),
                Some("e") => {
                    let s = format!("{x:.p$e}");
                    let (m, e) = s.split_once('e').unwrap();
                    let e: i32 = e.parse()?;
                    format!("{m}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs())
                }
                Some("%") => format!("{:.p$}\\%", x * 100.0),
                _ => format!("{x:.p$}"),
            }
        }
        _ => bail!("cannot format {v} with {fmt}"),
    };
    Ok(format!("{pre}{body}{post}"))
}

fn value(results: &Path, src: &str, key: &str, fmt: &str) -> Result<String> {
    let path = results.join(src);
    let mut data: Value =
        serde_json::from_str(&read(&path)?).with_context(|| format!("{} is not JSON", path.display()))?;
    for k in key.split('.') {
        data = match data {
            Value::Array(mut a) => {
                let i: usize = k.parse().with_context(|| format!("{k} is not an index"))?;
                if i >= a.len() {
                    bail!("index {i} out of range");
                }
                a.swap_remove(i)
            }
            Value::Object(mut o) => o.remove(k).ok_or_else(|| anyhow!("no key {k}"))?,
            _ => bail!("{key}: {k} is not inside an object or list"),
        };
    }
    format(fmt, &data)
}

pub fn entries(root: &Path) -> Vec<Row> {
    let text = read(&root.join(FILE)).unwrap_or_default();
    re!(r"(?m)^\\newcommand\{\\([A-Za-z]+)\}\{(.*)\}% src=(\S+) key=(\S+) fmt=(\S+)$")
        .captures_iter(&text)
        .map(|c| Row {
            name: c[1].into(),
            value: c[2].into(),
            src: c[3].into(),
            key: c[4].into(),
            fmt: c[5].into(),
        })
        .collect()
}

fn save(root: &Path, mut rows: Vec<Row>) -> Result<()> {
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    let lines: Vec<String> = rows
        .iter()
        .map(|r| {
            format!(
                "\\newcommand{{\\{}}}{{{}}}% src={} key={} fmt={}",
                r.name, r.value, r.src, r.key, r.fmt
            )
        })
        .collect();
    write(&root.join(FILE), &format!("{HEADER}{}\n", lines.join("\n")))
}

pub fn put(root: &Path, results: &Path, name: &str, src: &str, key: &str, fmt: &str) -> Result<String> {
    if !re!(r"^[A-Za-z]+$").is_match(name) {
        bail!("macro names are letters only (LaTeX)");
    }
    if fmt.contains(char::is_whitespace) {
        bail!("the format may not contain spaces");
    }
    let v = value(results, src, key, fmt)?;
    let mut rows: Vec<Row> = entries(root).into_iter().filter(|r| r.name != name).collect();
    rows.push(Row {
        name: name.into(),
        value: v.clone(),
        src: src.into(),
        key: key.into(),
        fmt: fmt.into(),
    });
    save(root, rows)?;
    Ok(v)
}

pub fn refresh(root: &Path, results: &Path, update: bool) -> Result<Vec<String>> {
    let (mut out, mut rows) = (vec![], vec![]);
    for mut r in entries(root) {
        match value(results, &r.src, &r.key, &r.fmt) {
            Err(e) => out.push(format!("\\{}: cannot read {}:{} ({e:#})", r.name, r.src, r.key)),
            Ok(new) if new != r.value => {
                out.push(format!(
                    "\\{}: {} -> {new}{}",
                    r.name,
                    r.value,
                    if update { "  (updated)" } else { "  STALE" }
                ));
                if update {
                    r.value = new;
                }
            }
            Ok(_) => {}
        }
        rows.push(r);
    }
    if update {
        save(root, rows)?;
    }
    Ok(out)
}

pub fn ensure_input(main: &str) -> String {
    if re!(r"(?m)^[^%\n]*\\input\{numbers(?:\.tex)?\}").is_match(main) {
        return main.to_owned();
    }
    match main.find("\\begin{document}") {
        Some(i) => format!("{}\\input{{numbers}}\n{}", &main[..i], &main[i..]),
        None => main.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn formats_like_python() {
        assert_eq!(format("{:.1f}", &json!(2.4567)).unwrap(), "2.5");
        assert_eq!(format("{}", &json!(3.0)).unwrap(), "3.0");
        assert_eq!(format("{:.2e}", &json!(123456.0)).unwrap(), "1.23e+05");
        assert_eq!(format("{:.0f}\\,km", &json!(41.6)).unwrap(), "42\\,km");
        assert_eq!(format("{:.1%}", &json!(0.256)).unwrap(), "25.6\\%");
    }
}
