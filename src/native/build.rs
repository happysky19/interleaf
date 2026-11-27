//! Isolated local builds from exported assets and a versioned native text overlay.
use super::{Client, NativeSession, check_receipt_dir, edits, need};
use anyhow::{Context, Result, anyhow, ensure};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
};

const ZIP_LIMIT: u64 = 32 * 1024 * 1024;
const FILE_LIMIT: u64 = 32 * 1024 * 1024;
const EXPANDED_LIMIT: u64 = 256 * 1024 * 1024;
const TEXT_LIMIT: usize = 32 * 1024 * 1024;
const LIMITATIONS: [&str; 2] = [
    "Project assets were exported separately from live text; an atomic binary-asset snapshot cannot be certified.",
    "The local TeX installation and fonts may differ from Overleaf's build environment.",
];

#[derive(PartialEq, Eq)]
struct Document {
    id: String,
    version: u64,
    text: String,
}
struct Capture {
    docs: BTreeMap<String, Document>,
    compiler: String,
}
fn capture(client: &mut Client) -> Result<Capture> {
    let mut socket = client.socket()?;
    let docs = edits::documents(&socket.project)?;
    let compiler = socket.project["compiler"].as_str().unwrap_or("pdflatex").to_owned();
    let mut total = 0usize;
    let mut result = BTreeMap::new();
    for (path, id) in docs {
        let snapshot = socket.join(&id)?;
        total = total
            .checked_add(snapshot.text.len())
            .ok_or_else(|| anyhow!("native text snapshot too large"))?;
        ensure!(total <= TEXT_LIMIT, "native text snapshot exceeds 32 MiB");
        result.insert(
            path,
            Document {
                id,
                version: snapshot.version,
                text: snapshot.text,
            },
        );
        socket.leave()?;
    }
    ensure!(
        !socket.concurrent,
        "native project changed during text snapshot; read again"
    );
    Ok(Capture { docs: result, compiler })
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn download(client: &mut Client) -> Result<Vec<u8>> {
    let mut response = client
        .http
        .get(format!("{}/project/{}/download/zip", client.origin, client.project))
        .header("Cookie", &client.cookie)
        .header("Accept", "application/zip")
        .call()
        .map_err(|_| anyhow!("native ZIP download failed; redirects and automatic login are disabled"))?;
    ensure!(response.status().as_u16() == 200, "native ZIP download refused");
    let kind = response
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.split(';').next());
    ensure!(
        matches!(kind, Some("application/zip" | "application/x-zip-compressed")),
        "native download is not a ZIP response"
    );
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(ZIP_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow!("native ZIP response invalid or timed out"))?;
    ensure!(bytes.len() as u64 <= ZIP_LIMIT, "native ZIP exceeds 32 MiB");
    Ok(bytes)
}
fn extract_archive(bytes: &[u8], root: &Path) -> Result<()> {
    ensure!(bytes.len() as u64 <= ZIP_LIMIT, "native ZIP exceeds 32 MiB");
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).context("invalid native ZIP archive")?;
    ensure!(
        !archive.is_empty() && archive.len() <= 1024,
        "native ZIP must contain 1–1024 entries"
    );
    // ZipArchive indexes by name and can hide duplicate central-directory records.
    let mut offset = usize::try_from(archive.central_directory_start())?;
    let mut count = 0usize;
    while bytes.get(offset..offset.saturating_add(4)) == Some(b"PK\x01\x02") {
        let header = bytes
            .get(offset..offset.saturating_add(46))
            .ok_or_else(|| anyhow!("truncated ZIP directory"))?;
        let lengths: usize = [28, 30, 32]
            .iter()
            .map(|i| u16::from_le_bytes([header[*i], header[*i + 1]]) as usize)
            .sum();
        offset = offset
            .checked_add(46 + lengths)
            .ok_or_else(|| anyhow!("invalid ZIP directory length"))?;
        ensure!(offset <= bytes.len(), "truncated ZIP directory");
        count += 1;
        ensure!(count <= 1024, "native ZIP exceeds 1024 entries");
    }
    ensure!(count == archive.len(), "duplicate or inconsistent native ZIP directory");
    let mut names = BTreeSet::new();
    let mut total = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).context("unsupported native ZIP entry")?;
        ensure!(
            !entry.encrypted() && !entry.is_symlink(),
            "encrypted or linked ZIP entries are refused"
        );
        let directory = entry.is_dir();
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            ensure!(
                kind == 0 || kind == if directory { 0o040000 } else { 0o100000 },
                "special ZIP entries are refused"
            );
        }
        let raw = std::str::from_utf8(entry.name_raw()).context("ZIP names must be UTF-8")?;
        let name = if directory {
            raw.strip_suffix('/').unwrap_or(raw)
        } else {
            raw
        };
        edits::path(name)?;
        ensure!(
            !name.split('/').any(|part| part.eq_ignore_ascii_case(".git")),
            "ZIP entries may not contain .git"
        );
        ensure!(names.insert(name.to_owned()), "duplicate native ZIP path");
        let size = entry.size();
        ensure!(size <= FILE_LIMIT, "native ZIP entry exceeds 32 MiB");
        total = total
            .checked_add(size)
            .ok_or_else(|| anyhow!("native ZIP expanded size overflow"))?;
        ensure!(total <= EXPANDED_LIMIT, "native ZIP expanded size exceeds 256 MiB");
        let target = root.join(name);
        if directory {
            fs::create_dir_all(target)?;
        } else {
            fs::create_dir_all(target.parent().unwrap())?;
            let mut file = fs::OpenOptions::new().write(true).create_new(true).open(target)?;
            let copied = std::io::copy(&mut (&mut entry).take(FILE_LIMIT + 1), &mut file)
                .context("invalid native ZIP payload")?;
            ensure!(copied == size && copied <= FILE_LIMIT, "native ZIP entry size mismatch");
        }
    }
    Ok(())
}
fn overlay(root: &Path, capture: &Capture) -> Result<()> {
    for (path, doc) in &capture.docs {
        let target = crate::text::project_path(root, Path::new(path))?;
        fs::create_dir_all(target.parent().unwrap())?;
        fs::write(target, doc.text.as_bytes())?;
    }
    Ok(())
}
fn private_directory(path: &Path) -> Result<()> {
    if !path.exists() {
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    check_receipt_dir(path)
}
fn build_root(session: &NativeSession, project: &str) -> Result<PathBuf> {
    private_directory(&session.receipts)?;
    let builds = session.receipts.join("builds");
    private_directory(&builds)?;
    let root = builds.join(project);
    private_directory(&root)?;
    Ok(root)
}
fn persist(root: &Path, build: &crate::tex::Build, manifest: &Value) -> Result<(PathBuf, PathBuf)> {
    ensure!(
        build.success && manifest["text_snapshot_stable"] == true,
        "only current successful native builds may be retained"
    );
    let pdf = build
        .pdf
        .as_ref()
        .ok_or_else(|| anyhow!("successful native build has no PDF"))?;
    let meta = fs::symlink_metadata(pdf)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= 64 * 1024 * 1024,
        "invalid native build PDF"
    );
    let bytes = fs::read(pdf)?;
    ensure!(bytes.starts_with(b"%PDF-"), "native build output is not a PDF");
    let mut manifest = manifest.clone();
    manifest["pdf_sha256"] = json!(digest(&bytes));
    let encoded = serde_json::to_vec_pretty(&manifest)?;
    let target = root.join(digest(&encoded));
    let pdf_target = target.join("paper.pdf");
    let manifest_target = target.join("manifest.json");
    if target.exists() {
        check_receipt_dir(&target)?;
        for path in [&pdf_target, &manifest_target] {
            let meta = fs::symlink_metadata(path)?;
            ensure!(
                meta.is_file() && !meta.file_type().is_symlink(),
                "invalid retained native build"
            );
        }
        ensure!(
            fs::read(&manifest_target)? == encoded && fs::read(&pdf_target)? == bytes,
            "retained native build does not match its manifest"
        );
        return Ok((pdf_target, manifest_target));
    }
    let temp = tempfile::tempdir_in(root)?;
    for (name, data) in [("paper.pdf", bytes.as_slice()), ("manifest.json", encoded.as_slice())] {
        let mut file = tempfile::NamedTempFile::new_in(temp.path())?;
        file.write_all(data)?;
        file.as_file().sync_all()?;
        file.persist(temp.path().join(name))
            .map_err(|_| anyhow!("cannot persist native build"))?;
    }
    File::open(temp.path())?.sync_all()?;
    fs::rename(temp.path(), &target)?;
    File::open(root)?.sync_all()?;
    Ok((pdf_target, manifest_target))
}

pub(super) fn compile(client: &mut Client, session: &NativeSession, args: &Map<String, Value>) -> Result<Value> {
    let main = need(args, "path")?;
    edits::path(main)?;
    ensure!(
        main.ends_with(".tex") && !main.starts_with('-'),
        "native compile path must name a .tex document"
    );
    let before = capture(client)?;
    ensure!(before.docs.contains_key(main), "native compile document not found");
    let engine = args.get("engine").and_then(Value::as_str).unwrap_or(&before.compiler);
    ensure!(
        matches!(engine, "pdflatex" | "xelatex" | "lualatex"),
        "engine must be pdflatex, xelatex or lualatex"
    );
    let archive = download(client)?;
    let root = build_root(session, &client.project)?;
    let temporary = tempfile::tempdir_in(&root)?;
    let source = temporary.path().join("source");
    fs::create_dir(&source)?;
    extract_archive(&archive, &source)?;
    overlay(&source, &before)?;
    let out = temporary.path().join("output");
    let build = crate::tex::compile(&source, main, &out, engine)?;
    // Reconnect only for reads: a long compiler invocation need not keep a collaboration socket alive.
    let after = capture(client);
    let stable = after.as_ref().is_ok_and(|after| after.docs == before.docs);
    let versions: Vec<_> = before
        .docs
        .iter()
        .map(|(path, doc)| {
            json!({
                "path":path,"doc_id":doc.id,"version":doc.version,"sha256":digest(doc.text.as_bytes())
            })
        })
        .collect();
    let mut result = json!({
        "project_id":client.project,"path":main,"engine":engine,
        "build_status":if !build.success {"failed"} else if after.is_err() {"unverified"} else if !stable {"not_current"} else {"succeeded"},
        "success":build.success && stable,"clean":build.clean && stable,"local_build_succeeded":build.success,
        "text_snapshot_stable":stable,"concurrent_changes":if after.is_ok() {json!(!stable)} else {Value::Null},
        "documents":versions,"archive_sha256":digest(&archive),"asset_snapshot_atomic":false,
        "limitations":LIMITATIONS,"pdf":Value::Null,"manifest":Value::Null,
        "report":crate::text::head(&build.report, 32000)
    });
    if let Err(error) = after {
        result["readback_error"] = json!(client.redact(&format!("{error:#}")));
    }
    if build.success && stable {
        let mut manifest = result.clone();
        manifest.as_object_mut().unwrap().remove("report");
        let (pdf, manifest) = persist(&root, &build, &manifest)?;
        result["pdf"] = json!(pdf);
        result["manifest"] = json!(manifest);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;
    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in entries {
            writer.start_file(*name, SimpleFileOptions::default()).unwrap();
            writer.write_all(body).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }
    #[test]
    fn archive_rejects_unsafe_names_duplicates_and_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let bytes = archive(&[("main.tex", b"ZIP text"), ("figures/plot.pdf", b"%PDF-1.4")]);
        extract_archive(&bytes, root.path()).unwrap();
        assert_eq!(std::fs::read(root.path().join("main.tex")).unwrap(), b"ZIP text");
        let live = Capture {
            docs: std::collections::BTreeMap::from([(
                "main.tex".into(),
                Document {
                    id: "111111111111111111111111".into(),
                    version: 95,
                    text: "live native text".into(),
                },
            )]),
            compiler: "pdflatex".into(),
        };
        overlay(root.path(), &live).unwrap();
        assert_eq!(
            std::fs::read(root.path().join("main.tex")).unwrap(),
            b"live native text"
        );
        for name in [
            "../escape",
            "/escape",
            ".git/config",
            "a/.GIT/config",
            "a\\escape",
            "a//escape",
        ] {
            let root = tempfile::tempdir().unwrap();
            assert!(
                extract_archive(&archive(&[(name, b"x")]), root.path()).is_err(),
                "{name}"
            );
        }
        let mut duplicate = archive(&[("main.tex", b"a"), ("evil.tex", b"b")]);
        let old = b"evil.tex";
        for i in 0..=duplicate.len() - old.len() {
            if &duplicate[i..i + old.len()] == old {
                duplicate[i..i + old.len()].copy_from_slice(b"main.tex");
            }
        }
        assert!(extract_archive(&duplicate, tempfile::tempdir().unwrap().path()).is_err());
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .add_symlink("link", "../../escape", SimpleFileOptions::default())
            .unwrap();
        assert!(
            extract_archive(
                &writer.finish().unwrap().into_inner(),
                tempfile::tempdir().unwrap().path()
            )
            .is_err()
        );
        assert!(extract_archive(b"not a ZIP", tempfile::tempdir().unwrap().path()).is_err());
    }
    #[test]
    #[cfg(unix)]
    fn local_build_retains_only_a_successful_versioned_pdf() {
        const CHILD: &str = "INTERLEAF_NATIVE_BUILD_TEST_ROOT";
        if let Some(root) = std::env::var_os(CHILD) {
            let root = std::path::PathBuf::from(root);
            let source = root.join("source");
            let output = root.join("output");
            std::fs::create_dir(&source).unwrap();
            std::fs::write(source.join("main.tex"), "live native text").unwrap();
            let build = crate::tex::compile(&source, "main.tex", &output, "pdflatex").unwrap();
            assert!(build.success, "{}", build.report);
            let retained = root.join("retained");
            std::fs::create_dir(&retained).unwrap();
            let manifest =
                serde_json::json!({"text_snapshot_stable":true,"documents":[{"path":"main.tex","version":95}]});
            let (pdf, receipt) = persist(&retained, &build, &manifest).unwrap();
            assert_eq!(std::fs::read(&pdf).unwrap(), b"%PDF-1.4\nlive native text\n");
            let record: Value = serde_json::from_slice(&std::fs::read(receipt).unwrap()).unwrap();
            assert_eq!(record["documents"][0]["version"], 95);
            assert!(record["pdf_sha256"].as_str().unwrap().len() == 64);
            assert_eq!(std::fs::read_dir(pdf.parent().unwrap()).unwrap().count(), 2);
            std::fs::write(source.join("main.tex"), "live native text FAIL").unwrap();
            let failed = crate::tex::compile(&source, "main.tex", &output, "pdflatex").unwrap();
            assert!(!failed.success);
            assert!(persist(&retained, &failed, &manifest).is_err());
            assert_eq!(std::fs::read(&pdf).unwrap(), b"%PDF-1.4\nlive native text\n");
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let program = bin.join("latexmk");
        std::fs::write(
            &program,
            r#"#!/bin/sh
[ "$1" = "-norc" ] || exit 3
[ -z "$OL_SESSION_FD$OVERLEAF_COOKIE$INTERLEAF_NATIVE_BUILD_TEST_ROOT" ] || exit 4
out=""
safe=""
for arg in "$@"; do
  case "$arg" in
    -outdir=*) out="${arg#-outdir=}" ;;
    -latexoption=-no-shell-escape) safe=yes ;;
  esac
done
[ -n "$out" ] && [ "$safe" = yes ] || exit 5
/bin/grep -q 'live native text' main.tex || exit 6
printf '%s\n' '%PDF-1.4' 'live native text' > "$out/main.pdf"
printf '%s\n' 'Output written on main.pdf (1 page).' > "$out/main.log"
/bin/grep -q FAIL main.tex && exit 2
exit 0
"#,
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native::build::tests::local_build_retains_only_a_successful_versioned_pdf",
                "--nocapture",
            ])
            .env(CHILD, root.path())
            .env("PATH", &bin)
            .env("OL_SESSION_FD", "99")
            .env("OVERLEAF_COOKIE", "fixture-secret")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
