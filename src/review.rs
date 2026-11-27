//! Review pack: a latexdiff PDF of what changed upstream since a commit or date, the commits,
//! and words per section before and after.
use crate::sync::Repo;
use crate::tex::{self, Doc};
use crate::text::{re, write};
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn latexdiff() -> Result<Vec<String>> {
    let on_path = std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join("latexdiff"))
            .find(|f| f.is_file())
    });
    if let Some(p) = on_path {
        return Ok(vec![p.to_string_lossy().into_owned()]);
    }
    let out = Command::new("kpsewhich")
        .args(["-format=texmfscripts", "latexdiff.pl"])
        .output();
    match out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()) {
        Ok(script) if !script.is_empty() => Ok(vec!["perl".into(), script]),
        _ => bail!("latexdiff not found (it ships with TeX Live)"),
    }
}

fn export(repo: &Repo, rev: &str, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    let archive = crate::text::git_command()
        .args(["archive", rev])
        .current_dir(repo.dir())
        .stdout(Stdio::piped())
        .spawn()?;
    let status = Command::new("tar")
        .arg("-x")
        .arg("-C")
        .arg(dest)
        .stdin(archive.stdout.unwrap())
        .status()?;
    if !status.success() {
        bail!("could not export {rev}");
    }
    Ok(())
}

fn words(root: &Path, main: &str) -> Result<BTreeMap<String, usize>> {
    Ok(Doc::new(root, main)?
        .sections()
        .into_iter()
        .map(|s| (s.title, s.words))
        .collect())
}

pub fn review(repo: &Repo, main: &str, since: &str, outdir: &Path, engine: &str) -> Result<String> {
    repo.fetch()?;
    let remote = repo.git(&["rev-parse", &repo.remote()])?;
    let rev = if re!(r"^[0-9a-f]{6,40}$").is_match(since) {
        since.to_owned()
    } else {
        let rev = repo.git(&["rev-list", "-1", &format!("--before={since}"), &remote])?;
        if rev.is_empty() {
            bail!("no commit before {since}");
        }
        rev
    };
    let td = tempfile::tempdir()?;
    let (old, new): (PathBuf, PathBuf) = (td.path().join("old"), td.path().join("new"));
    export(repo, &rev, &old)?;
    export(repo, &remote, &new)?;
    write(
        &crate::text::project_path(&new, Path::new("_old.tex"))?,
        &tex::flatten(&old, main)?,
    )?;
    write(
        &crate::text::project_path(&new, Path::new("_new.tex"))?,
        &tex::flatten(&new, main)?,
    )?;
    let ld = latexdiff()?;
    let diff = Command::new(&ld[0])
        .args(&ld[1..])
        .args(["--type=CFONT", "--math-markup=whole", "_old.tex", "_new.tex"])
        .current_dir(&new)
        .output()?;
    if !diff.status.success() {
        bail!("latexdiff failed: {}", String::from_utf8_lossy(&diff.stderr).trim());
    }
    write(
        &crate::text::project_path(&new, Path::new("_review.tex"))?,
        &String::from_utf8_lossy(&diff.stdout),
    )?;
    let build = tex::compile(&new, "_review.tex", outdir, engine)?;
    let (before, after) = (words(&old, main)?, words(&new, main)?);
    let current_pdf = if build.success { build.pdf.as_ref() } else { None };
    let short = &rev[..rev.len().min(8)];
    let fin = outdir.join(format!("review-since-{short}.pdf"));
    if let Some(pdf) = current_pdf {
        std::fs::rename(pdf, &fin)?;
    }
    let mut rows: Vec<String> = after
        .iter()
        .filter(|(t, w)| before.get(*t) != Some(w))
        .map(|(t, w)| format!("  {t}: {} -> {w}", before.get(t).unwrap_or(&0)))
        .collect();
    rows.extend(
        before
            .keys()
            .filter(|t| !after.contains_key(*t))
            .map(|t| format!("  {t}: removed")),
    );
    if rows.is_empty() {
        rows.push("  (unchanged)".into());
    }
    let log = repo.log(&format!("{rev}..{remote}"), 50, false)?;
    let build_details = build
        .report
        .lines()
        .skip(1)
        .filter(|line| !line.starts_with("pdf: "))
        .collect::<Vec<_>>()
        .join("\n");
    let report = [
        format!("changes on {} since {short}", repo.p.upstream()),
        format!(
            "review_source: upstream Git commit {}",
            repo.git(&["rev-parse", &remote])?
        ),
        format!(
            "pdf: {}",
            if current_pdf.is_some() {
                fin.display().to_string()
            } else {
                "(not built)".into()
            }
        ),
        String::new(),
        "commits:".into(),
        if log.is_empty() { "  (none)".into() } else { log },
        String::new(),
        "words per section:".into(),
        rows.join("\n"),
        String::new(),
        format!("build: {}", build.report.lines().next().unwrap_or("")),
        build_details,
    ]
    .join("\n");
    if !build.success {
        bail!("{report}");
    }
    Ok(report)
}
