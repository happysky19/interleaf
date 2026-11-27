//! End-to-end test against a fake Overleaf: a bare git repository is the remote and a second
//! clone plays the co-author typing in the browser. The server runs as a subprocess speaking
//! JSON-RPC, as under Claude Code or Codex.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const MAIN: &str = r"\documentclass{article}
\usepackage{graphicx}
\usepackage{natbib}
\graphicspath{{figures/}}
\begin{document}
\begin{abstract}
We test the tools.
\end{abstract}
\input{sections/intro}
\section{Method}\label{sec:method}
The ejecta follow Eq.~\ref{eq:v} and Fig.~\ref{fig:missing}.
\begin{equation}\label{eq:v}
v = v_0 (a/a_0)^{-1/2},
\end{equation}
for the launch speed.
\begin{figure}
\includegraphics[width=\linewidth]{fig01}
\caption{A model image against the data.}\label{fig:one}
\end{figure}
\input{protected}
\bibliographystyle{plainnat}
\bibliography{refs}
\end{document}
";
const INTRO: &str = r"\section{Introduction}\label{sec:intro}
The probe reached the comet \citep{Smith2020}. We delve into a novel dust model \citep{Jones2021,Nobody2099}.
Second line of the paragraph.
Third line of the paragraph.

Another paragraph.
";
const BIB: &str = "@article{Smith2020, author={Smith, J. A.}, title={Dust trails of short-period comets}, journal={Icarus}, year={2020}}
@article{Jones2021, author={Jones, Mary and Lee, Ann}, title={Radar shapes of small asteroids}, journal={AJ}, year={2021}}
@article{Unused2020, author={X, Y}, title={Unused}, journal={Z}, year={2020}}
";
/// A 1x1 PNG.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49,
    0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00,
    0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

fn sh(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new(args[0])
        .args(&args[1..])
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

fn write(p: &Path, s: &str) {
    std::fs::write(p, s).unwrap();
}

fn edit(p: &Path, old: &str, new: &str) {
    let t = read(p);
    assert!(t.contains(old), "{old:?} not in {}", p.display());
    write(p, &t.replacen(old, new, 1));
}

fn norm(s: &str) -> Vec<String> {
    s.split("\n\n")
        .map(|p| p.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|p| !p.is_empty())
        .collect()
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    id: u64,
}

impl Server {
    fn start(cwd: &Path, config: &Path) -> Server {
        Self::start_with_env(cwd, config, &[])
    }

    fn start_with_env(cwd: &Path, config: &Path, extra: &[(&str, String)]) -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_interleaf"))
            .current_dir(cwd)
            .env("INTERLEAF_CONFIG", config)
            .env_remove("INTERLEAF_WORKSPACE")
            .env_remove("OVERLEAF_PROJECT_ID")
            .envs(extra.iter().map(|(key, value)| (*key, value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let (stdin, stdout) = (
            child.stdin.take().unwrap(),
            BufReader::new(child.stdout.take().unwrap()),
        );
        let mut s = Server {
            child,
            stdin,
            stdout,
            id: 0,
        };
        let init = s.rpc("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "claude-code", "version": "0"}}));
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        s
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        writeln!(
            self.stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params})
        )
        .unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn tool(&mut self, name: &str, args: Value, expect_error: bool) -> String {
        let r = self.rpc("tools/call", json!({"name": name, "arguments": args}));
        let text = r["result"]["content"][0]["text"].as_str().unwrap().to_owned();
        println!("\n### {name} {args}\n{}", &text[..text.len().min(1500)]);
        assert_eq!(r["result"]["isError"], expect_error, "{text}");
        text
    }

    fn call(&mut self, name: &str, args: Value) -> String {
        self.tool(name, args, false)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

struct World {
    td: tempfile::TempDir,
}

impl World {
    fn path(&self, p: &str) -> PathBuf {
        self.td.path().join(p)
    }
    fn coauthor_edit(&self, path: &str, old: &str, new: &str, msg: &str) {
        let seed = self.path("seed");
        sh(&seed, &["git", "pull", "-q", "origin", "main"]);
        edit(&seed.join(path), old, new);
        sh(&seed, &["git", "commit", "-qam", msg]);
        sh(&seed, &["git", "push", "-q", "origin", "main"]);
    }
    fn remote_file(&self, path: &str) -> String {
        sh(&self.path("overleaf.git"), &["git", "show", &format!("main:{path}")])
    }
}

fn world() -> World {
    let w = World {
        td: tempfile::tempdir().unwrap(),
    };
    let (remote, seed) = (w.path("overleaf.git"), w.path("seed"));
    sh(
        w.td.path(),
        &["git", "init", "-q", "--bare", "-b", "main", remote.to_str().unwrap()],
    );
    sh(
        w.td.path(),
        &["git", "clone", "-q", remote.to_str().unwrap(), seed.to_str().unwrap()],
    );
    std::fs::create_dir_all(seed.join("sections")).unwrap();
    std::fs::create_dir_all(seed.join("figures")).unwrap();
    write(&seed.join("main.tex"), MAIN);
    write(&seed.join("sections/intro.tex"), INTRO);
    write(&seed.join("protected.tex"), "Text the PI owns.\n");
    write(&seed.join("refs.bib"), BIB);
    write(
        &seed.join("interleaf.toml"),
        "journal = \"apjl\"\nprotected = [\"protected.tex\"]\n",
    );
    std::fs::write(seed.join("figures/fig01.png"), PNG).unwrap();
    for c in [
        &["git", "config", "user.name", "Co Author"][..],
        &["git", "config", "user.email", "co@example.org"],
        &["git", "add", "-A"],
        &["git", "commit", "-qm", "Overleaf seed"],
        &["git", "push", "-q", "origin", "main"],
    ] {
        sh(&seed, c);
    }
    w
}

#[test]
fn zero_config() {
    let w = world();
    std::fs::create_dir_all(w.path("work")).unwrap();
    let work = std::fs::canonicalize(w.path("work")).unwrap();
    let none = w.path("no-config.toml");
    // a git URL per call: cloned into the working folder
    let mut s = Server::start(&work, &none);
    let t = s.call("status", json!({"project": w.path("overleaf.git").to_str().unwrap()}));
    assert!(
        t.contains(&format!("clone: {}", work.join("overleaf").display())) && t.contains("journal: apjl"),
        "{t}"
    );
    // inside a clone that carries interleaf.toml: no project needed
    let mut s = Server::start(&work.join("overleaf"), &none);
    assert!(s.call("outline", json!({})).contains("2 Method"));
    // elsewhere, with nothing to go on: say what is missing
    let mut s = Server::start(w.td.path(), &none);
    assert!(s.tool("status", json!({}), true).contains("say which project"));
}

#[test]
fn group_workflow() {
    let w = world();
    let (clone, results, remote) = (w.path("clone"), w.path("results"), w.path("overleaf.git"));
    let first = sh(&w.path("seed"), &["git", "rev-parse", "HEAD"]).trim().to_owned();
    std::fs::create_dir_all(&results).unwrap();
    write(&results.join("fit.json"), r#"{"tail": {"pa": 2.4567}}"#);
    let config = w.path("config.toml");
    write(
        &config,
        &format!(
            "name = \"Tester\"\ninitials = \"AB\"\n[projects.t]\ndir = \"{}\"\ngit_url = \"{}\"\nresults = \"{}\"\n",
            clone.display(),
            remote.display(),
            results.display()
        ),
    );
    let mut s = Server::start(w.td.path(), &config);
    assert_eq!(
        s.rpc("tools/list", json!({}))["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        17
    );

    // reading
    let t = s.call("status", json!({}));
    assert!(
        t.contains("mode: suggest") && t.contains("journal: apjl") && t.contains("new on the remote: 0"),
        "{t}"
    );
    let t = s.call("outline", json!({}));
    for want in ["1 Introduction", "2 Method", "sections/intro.tex:1", "fig:one"] {
        assert!(t.contains(want), "{want}");
    }
    assert!(
        s.call("read_section", json!({"section": "Introduction"}))
            .contains("Smith2020")
    );
    let t = s.call("check", json!({}));
    for want in [
        "fig:missing",
        "Nobody2099",
        "Unused2020",
        "'novel'",
        "'delve'",
        "abstract words: 4 / 250",
        r"\software{",
    ] {
        assert!(t.contains(want), "{want}");
    }
    let t = s.call("compile", json!({}));
    assert!(
        (t.starts_with("ok") || t.starts_with("errors"))
            && !t.contains("(none)")
            && t.contains("reference 'fig:missing'  main.tex:11"),
        "{t}"
    );

    // suggestion mode: prose inline, equation as a block, both reversible
    let (intro, main) = (clone.join("sections/intro.tex"), clone.join("main.tex"));
    edit(&intro, "probe reached", "small probe finally reached");
    edit(&main, "^{-1/2}", "^{-0.5}");
    let t = s.call("push", json!({"message": "Suggest intro and equation edits"}));
    assert!(
        t.contains("dry run") && t.contains(r"\sgrep[AB]{small probe finally}{probe}"),
        "{t}"
    );
    assert!(!w.remote_file("sections/intro.tex").contains("finally"));
    assert!(
        s.call(
            "push",
            json!({"message": "Suggest intro and equation edits", "dry_run": false})
        )
        .contains("pushed")
    );
    let (r_intro, r_main) = (w.remote_file("sections/intro.tex"), w.remote_file("main.tex"));
    assert!(r_intro.contains(r"\sgrep[AB]{small probe finally}{probe}"));
    assert!(
        r_main.contains("%% sg[AB] old:")
            && r_main.contains(r"\providecommand{\sgadd}")
            && r_main.contains(r"\sgnote[AB]")
    );
    assert!(sh(&remote, &["git", "log", "-1", "--format=%B", "main"]).contains("Agent: claude-code for Tester"));
    let t = s.call("compile", json!({}));
    assert!(t.starts_with("ok") || !t.contains("errors ("), "{t}");
    assert!(s.call("check", json!({})).contains("pending suggestions: AB 2"));
    assert!(s.call("suggestions", json!({})).contains("small probe finally"));

    // a second round on top of the first stays one suggestion set against the original
    edit(&intro, "Another paragraph.", "Another paragraph, now longer.");
    s.call("push", json!({"message": "More", "dry_run": false}));
    let marked = w.remote_file("sections/intro.tex");
    assert_eq!(
        norm(&interleaf::suggest::resolve(&marked, false, Some("AB")).unwrap().0),
        norm(INTRO)
    );

    // the co-author's agent accepts AB's suggestions; push sends them unmarked
    s.call("pull", json!({}));
    assert!(
        s.call("resolve", json!({"action": "accept", "author": "AB"}))
            .contains("accepted 3")
    );
    let t = s.call("push", json!({"message": "Accept AB", "dry_run": false}));
    assert!(t.contains("sent as is") && t.contains("pushed"), "{t}");
    let fin = w.remote_file("sections/intro.tex");
    assert!(
        !fin.contains(r"\sg") && fin.contains("small probe finally") && fin.contains("now longer"),
        "{fin}"
    );
    let m = w.remote_file("main.tex");
    assert!(m.contains("^{-0.5}") && !m.contains("%% sg"));

    // direct mode, protected files
    edit(&intro, "Second line", "Second line, direct");
    assert!(
        s.call(
            "push",
            json!({"message": "Direct edit", "mode": "direct", "dry_run": false})
        )
        .contains("pushed")
    );
    assert!(w.remote_file("sections/intro.tex").contains("Second line, direct"));
    write(&clone.join("protected.tex"), "Changed.\n");
    assert!(
        s.call("push", json!({"message": "x", "dry_run": false}))
            .contains("protected by the project")
    );
    sh(&clone, &["git", "checkout", "--", "protected.tex"]);

    // co-author on the adjacent line: merged, both kept
    edit(&intro, "Second line, direct", "Second line, mine");
    w.coauthor_edit("sections/intro.tex", "Third line", "Third line, theirs", "web edit");
    let t = s.call("pull", json!({}));
    assert!(
        t.contains("merged, local changes kept") && t.contains("merged line by line"),
        "{t}"
    );
    assert!(read(&intro).contains("mine") && read(&intro).contains("theirs"));
    s.call(
        "push",
        json!({"message": "Adjacent", "mode": "direct", "dry_run": false}),
    );

    // same line: report changes nothing; markers mode; push refuses markers
    edit(&intro, "Second line, mine", "Second line, MINE");
    w.coauthor_edit(
        "sections/intro.tex",
        "Second line, mine",
        "Second line, THEIRS",
        "web edit",
    );
    let before = read(&intro);
    let t = s.call("pull", json!({}));
    assert!(
        t.contains("conflict, nothing changed") && t.contains("THEIRS") && read(&intro) == before,
        "{t}"
    );
    assert!(
        s.call("push", json!({"message": "no", "dry_run": false}))
            .contains("blocked: conflict")
    );
    assert!(
        s.call("pull", json!({"on_conflict": "markers"}))
            .contains("conflict markers written")
    );
    assert!(
        s.tool("push", json!({"message": "no", "dry_run": false}), true)
            .contains("markers left")
    );
    let settled: String = {
        let t = read(&intro);
        let (a, rest) = t.split_once("<<<<<<<").unwrap();
        let (_, b) = rest.split_once(">>>>>>>").unwrap();
        format!("{a}Second line, agreed.\n{}", b.split_once('\n').unwrap().1)
    };
    write(&intro, &settled);
    assert!(
        s.call("push", json!({"message": "Settle", "mode": "direct", "dry_run": false}))
            .contains("pushed")
    );
    assert!(w.remote_file("sections/intro.tex").contains("agreed"));

    // deletion guard
    std::fs::remove_file(clone.join("refs.bib")).unwrap();
    assert!(
        s.call("push", json!({"message": "oops", "dry_run": false}))
            .contains("would delete refs.bib")
    );
    sh(&clone, &["git", "checkout", "--", "refs.bib"]);

    // numbers from result files
    assert!(
        s.call(
            "number_set",
            json!({"name": "TailPA", "source": "fit.json", "key": "tail.pa", "fmt": "{:.1f}"})
        )
        .contains("= 2.5")
    );
    assert!(read(&main).contains(r"\input{numbers}"));
    write(&results.join("fit.json"), r#"{"tail": {"pa": 3.04}}"#);
    assert!(s.call("numbers", json!({})).contains("2.5 -> 3.0  STALE"));
    assert!(s.call("numbers", json!({"update": true})).contains("updated"));
    assert!(read(&clone.join("numbers.tex")).contains(r"\newcommand{\TailPA}{3.0}"));

    // figures with their sources
    let src = w.path("made.png");
    std::fs::write(&src, PNG).unwrap();
    s.call(
        "add_figure",
        json!({"source": src.to_str().unwrap(), "name": "fig02.png"}),
    );
    assert!(s.call("figures", json!({})).starts_with("ok"));
    std::fs::write(&src, [PNG, b"regenerated"].concat()).unwrap();
    assert!(s.call("figures", json!({})).contains("stale"));
    assert!(s.call("figures", json!({"update": true})).contains("updated"));
    assert!(
        s.call(
            "push",
            json!({"message": "Numbers and figure", "mode": "direct", "dry_run": false})
        )
        .contains("pushed")
    );

    // review pack since the seed commit
    let t = s.call("review", json!({"since": &first[..10]}));
    for want in ["review-since-", "build: ok", "Introduction", "Accept AB"] {
        assert!(t.contains(want), "{want}\n{t}");
    }
    assert!(!t.contains("(not built)"));

    // compile error mapped to the file
    let good = read(&intro);
    write(
        &intro,
        &good.replace("Another paragraph", "Another \\undefinedmacro paragraph"),
    );
    let t = s.tool("compile", json!({}), true);
    assert!(
        regex::Regex::new(r"sections/intro\.tex:\d+: Undefined control sequence")
            .unwrap()
            .is_match(&t),
        "{t}"
    );
    write(&intro, &good);

    // race: the co-author pushes between our pull and our push -> one rebase and retry
    let hook = clone.join(".git/hooks/pre-push");
    write(
        &hook,
        &format!(
            "#!/bin/sh\n[ -e \"{flag}\" ] && exit 0\ntouch \"{flag}\"\nunset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE\ncd \"{seed}\" && git pull -q origin main && perl -pi -e 's/\\\\section\\{{Method\\}}/\\\\section{{Methods}}/' main.tex && git commit -qam 'racing web edit' && git push -q origin main\n",
            flag = w.path("raced").display(),
            seed = w.path("seed").display()
        ),
    );
    sh(&clone, &["chmod", "+x", ".git/hooks/pre-push"]);
    write(&intro, &good.replace("Another paragraph", "Yet another paragraph"));
    assert!(
        s.call("push", json!({"message": "Race", "mode": "direct", "dry_run": false}))
            .contains("pushed")
    );
    assert!(w.path("raced").exists());
    assert!(w.remote_file("main.tex").contains(r"\section{Methods}"));
    assert!(w.remote_file("sections/intro.tex").contains("Yet another paragraph"));
    assert!(s.call("history", json!({"n": 3})).contains("Race"));
}

fn legacy_fixture(w: &World) -> (Server, PathBuf) {
    let clone = w.path("clone");
    let cfg = w.path("config.toml");
    write(
        &cfg,
        &format!(
            "name = \"Tester\"\ninitials = \"AB\"\n[projects.t]\ndir = \"{}\"\ngit_url = \"{}\"\n",
            clone.display(),
            w.path("overleaf.git").display()
        ),
    );
    let mut server = Server::start(w.td.path(), &cfg);
    server.call("status", json!({}));
    (server, clone)
}

#[test]
fn rejects_wrong_existing_origin() {
    let w = world();
    let (mut s, clone) = legacy_fixture(&w);
    sh(
        &clone,
        &["git", "remote", "set-url", "origin", w.path("seed").to_str().unwrap()],
    );
    assert!(s.tool("outline", json!({}), true).contains("origin"));
    sh(
        &clone,
        &[
            "git",
            "remote",
            "set-url",
            "origin",
            w.path("overleaf.git").to_str().unwrap(),
        ],
    );
    sh(
        &clone,
        &[
            "git",
            "remote",
            "set-url",
            "--push",
            "origin",
            w.path("seed").to_str().unwrap(),
        ],
    );
    assert!(s.tool("outline", json!({}), true).contains("origin"));
}

#[test]
fn confines_project_destinations() {
    let w = world();
    let (mut s, clone) = legacy_fixture(&w);
    let source = w.path("made.png");
    std::fs::write(&source, PNG).unwrap();
    for name in ["../outside.png", ".git/overwrite.png"] {
        s.tool(
            "add_figure",
            json!({"source": source, "name": name, "subdir": "."}),
            true,
        );
    }
    s.tool(
        "cite_add",
        json!({"identifier":"ignored", "bibfile":"../escape.bib"}),
        true,
    );
    s.tool(
        "resolve",
        json!({"action":"accept", "path": w.path("seed/main.tex")}),
        true,
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(w.path("seed"), clone.join("escape")).unwrap();
        s.tool("add_figure", json!({"source": source, "subdir":"escape"}), true);
    }
    assert!(!w.path("outside.png").exists());
    assert!(!clone.join(".git/overwrite.png").exists());
}

#[test]
fn resolution_marker_cannot_cover_new_edits() {
    let w = world();
    let (mut s, clone) = legacy_fixture(&w);
    let intro = clone.join("sections/intro.tex");
    edit(&intro, "Second line of the paragraph.", "Second line, edited.");
    s.call("push", json!({"message":"suggest", "dry_run":false}));
    s.call(
        "resolve",
        json!({"action":"accept", "author":"AB", "path":"sections/intro.tex"}),
    );
    edit(&intro, "Another paragraph", "Another fresh paragraph");
    assert!(
        s.tool("push", json!({"message":"must not send direct", "dry_run":false}), true)
            .contains("resolved")
    );
    assert!(
        s.call(
            "push",
            json!({"message":"explicit direct", "mode":"direct", "dry_run":false})
        )
        .contains("pushed")
    );
}

#[test]
#[cfg(unix)]
fn build_artifacts_are_current_and_credentials_are_removed() {
    use std::os::unix::fs::PermissionsExt;
    let w = world();
    let (s, clone) = legacy_fixture(&w);
    drop(s);
    let bin = w.path("test-bin");
    std::fs::create_dir(&bin).unwrap();
    let fail = w.path("fail-build");
    let script = r#"#!/bin/sh
[ -z "${OVERLEAF_GIT_TOKEN}${ADS_TOKEN}${OL_COOKIE}${GH_TOKEN}${OPENAI_API_KEY}${ANTHROPIC_API_KEY}${SLACK_BOT_TOKEN}" ] || exit 72
norc=no
for arg do
  case "$arg" in -norc) norc=yes;; -outdir=*) out=${arg#-outdir=};; esac
  main=$arg
done
[ "$norc" = yes ] || exit 73
[ ! -e "${0%/*}/../fail-build" ] || exit 42
stem=${main##*/}
stem=${stem%.tex}
printf 'pdf for current input\n' > "$out/$stem.pdf"
printf 'Output written on %s.pdf (1 page, 1 bytes).\n' "$stem" > "$out/$stem.log"
"#;
    write(&bin.join("latexmk"), script);
    write(&bin.join("latexdiff"), "#!/bin/sh\ncat _new.tex\n");
    for tool in ["latexmk", "latexdiff"] {
        std::fs::set_permissions(bin.join(tool), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let env = vec![
        ("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap())),
        ("OVERLEAF_GIT_TOKEN", "dummy-git".into()),
        ("ADS_TOKEN", "dummy-ads".into()),
        ("OL_COOKIE", "dummy-native".into()),
        ("GH_TOKEN", "dummy-gh".into()),
        ("OPENAI_API_KEY", "dummy-openai".into()),
        ("ANTHROPIC_API_KEY", "dummy-anthropic".into()),
        ("SLACK_BOT_TOKEN", "dummy-slack".into()),
    ];
    let mut s = Server::start_with_env(w.td.path(), &w.path("config.toml"), &env);
    let t = s.call("compile", json!({}));
    assert!(
        t.contains("source_tree_sha256:")
            && t.contains("source_scope: local working tree")
            && t.contains("build_status: succeeded"),
        "{t}"
    );
    let since = sh(&clone, &["git", "rev-parse", "HEAD"]).trim().to_owned();
    let first = s.call("review", json!({"since": since}));
    assert!(
        first.contains("review-since-") && !first.contains("(not built)"),
        "{first}"
    );
    assert!(
        !first.contains("/_review.pdf"),
        "review must not advertise the renamed build path: {first}"
    );
    write(&fail, "fail");
    let failed = s.tool("review", json!({"since": since}), true);
    assert!(
        failed.contains("pdf: (not built)") && failed.contains("build_status: failed"),
        "{failed}"
    );
}
