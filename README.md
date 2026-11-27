# interleaf

People and agents writing one LaTeX paper together. interleaf is an MCP server: Claude Code, Codex,
Cursor or any other MCP client gets tools to sync the paper, propose edits that a co-author accepts
or rejects, and check what an agent wrote before anyone reads it. The paper lives on Overleaf (cloud
or self-hosted) or in any git repository, such as GitHub or GitLab. One binary, nothing to configure.

- **Suggestion mode** (default). Edits reach the shared copy as `\sgadd{new}`, `\sgdel{old}`,
  `\sgrep{new}{old}`, tagged with the author's initials and coloured in the PDF. Changed equations,
  figures and headings keep the old version as comments and get an `[XY edit]` tag. Co-authors accept
  or reject suggestions by hand or with `resolve`. Works with AASTeX, AGU and plain article classes.
- **Sync with people editing at the same time.** Edits on adjacent lines merge. A real clash
  changes nothing and shows both versions. Push never forces.
- **Checks for agent-written text.** `cite_check` verifies every cited reference against NASA ADS
  (missing, wrong DOI, wrong year, preprint since published). `check` flags unsupported claims and
  machine-like phrasing, besides refs, labels, graphics and journal limits (AAS, AGU, PNAS, Nature,
  Science).
- **Numbers and figures with their source.** `number_set` writes a value from a JSON result file
  into `numbers.tex`, and `numbers` reports values that no longer match. `add_figure` / `figures` do
  the same for figures.
- **Review pack.** `review` produces a latexdiff PDF of everything that changed since a date or
  commit, with the commits and words per section.

## Setup

Needs git, and TeX Live (`latexmk`, `latexdiff`) for `compile` and `review`. Overleaf projects need
Overleaf's Git integration.

1. Install: `cargo install interleaf`.
2. Register it with your client:
   ```bash
   claude mcp add interleaf -s user -- interleaf
   ```
   For Codex, add to `~/.codex/config.toml`:
   ```toml
   [mcp_servers.interleaf]
   command = "interleaf"
   ```
3. Tokens, in the environment or the system keychain:
   - `OVERLEAF_GIT_TOKEN`: Overleaf, Account Settings → Git Integration → Generate token. Not needed
     for GitHub or GitLab, which use your normal git login.
   - `ADS_TOKEN` (for `cite_check` and `cite_add`): <https://ui.adsabs.harvard.edu> → Account →
     Settings → API Token.
   ```bash
   security add-generic-password -U -a "$USER" -s overleaf-git-token -w     # macOS
   security add-generic-password -U -a "$USER" -s ads-api-token -w
   secret-tool store --label=overleaf-git-token service overleaf-git-token  # Linux
   secret-tool store --label=ads-api-token service ads-api-token
   ```

Then give the agent the paper: an Overleaf link, or a git URL. The first call clones it into the
working folder. Inside a clone of an Overleaf project, or of a repository with an `interleaf.toml`,
no link is needed. Suggestions carry the initials of your git `user.name`.

## Personal settings (optional)

`~/.config/interleaf/config.toml` names projects and overrides settings:

```toml
name = "Your Name"
initials = "YN"                       # tag on your suggestions
default = "paper"

[projects.paper]                      # an Overleaf project
id = "<id from the Overleaf URL>"
dir = "~/papers/paper"                # local clone, made on first use
results = "~/research/paper"          # where number_set looks for result files

[projects.notes]                      # any other git remote
git_url = "git@github.com:you/notes.git"
dir = "~/papers/notes"
mode = "direct"                       # your own drafts: edits go in unmarked
```

For a self-hosted Overleaf, give both `id` and its `git_url`. Without `results`, `number_set` reads
result files relative to the working folder.

## Project rules (shared)

An `interleaf.toml` file in the paper's repository applies to everyone who works on it:

```toml
journal = "apjl"              # apj apjl agu grl pnas nature science
mode = "suggest"              # or "direct"
protected = ["abstract.tex"]  # agents may not push changes to these
[lint]
ignore = ['\bclearly\b']
extra = ['\bvery unique\b']
```

A `mode` in your personal settings overrides the project's. The built-in wording rules and journal
limits are in `src/rules.toml`.

## Tools

| tool | |
|---|---|
| `status`, `pull`, `push`, `history` | sync; `push` dry-runs by default and returns the diff |
| `outline`, `read_section` | document map with word counts; one section's source |
| `check`, `compile` | consistency, wording, journal limits, pending suggestions; local build with errors at file:line |
| `cite_check`, `cite_add` | ADS verification; add a reference by DOI, arXiv id, bibcode or ADS query |
| `number_set`, `numbers` | values from result files, and which have gone stale |
| `add_figure`, `figures` | figures from local files, and which have been regenerated |
| `suggestions`, `resolve` | list pending suggestions; accept or reject them by author, file or line range |
| `review` | latexdiff PDF since a date or commit |

## Notes

- Every commit says which agent made it, for whom, and in which mode (`Agent: claude-code for Your Name`).
  In Overleaf's own history it appears under the token owner.
- Overleaf comments and Track Changes are not carried by Git. Suggestion mode uses LaTeX
  markup; the development native comment backend is described below.
- Before submission, accept or reject all suggestions. `check` reports any that are left.
  Then delete the suggestion block from the preamble.
- interleaf keeps its build output, review PDFs and figure sources in the clone's `.git/interleaf/`.
- Tests: `cargo test`. The end-to-end test runs against a local bare repository that stands in for
  the remote, with a second clone acting as the co-author.

## Native Overleaf collaboration (development)

This branch adds seven native tools alongside the existing Git and LaTeX tools:

| Tool | Use |
| --- | --- |
| `native_list_comments` | List review threads and their quoted anchors |
| `native_read_comment_thread` | Read every message in a thread |
| `native_reply_comment` | Reply to an existing thread |
| `native_read_file` | Read current text and its native version |
| `native_list_tracked_changes` | Read the complete pending-change inventory |
| `native_suggest_edit` | Propose one exact replacement as pending Track Changes |
| `native_add_comment` | Create a comment anchored to exact quoted text |

Name the `project_id` on every call. Several papers can share one Fridica channel.
Start an edit or anchored comment by reading the live file:

`native_read_file`:
```json
{"project_id": "0123456789abcdef01234567", "path": "main.tex"}
```

Then use its returned `version` as `expected_version`:

`native_suggest_edit`:
```json
{
  "project_id": "0123456789abcdef01234567",
  "path": "main.tex",
  "old_string": "the old approximation",
  "new_string": "the revised approximation",
  "expected_version": 95,
  "operation_id": "dcd3d917-b8bc-4c47-83b5-5bfd02567b8c"
}
```

Use `occurrence` (1-based) when the quoted text is not unique. Suggestions always
use tracking and strict version checks. Edits that touch an existing pending
change are refused. Readback must confirm the predicted text, next version,
new pending records and preservation of the original records before success.

Each write needs a stable `operation_id` UUID. Keep the ID and arguments unchanged
when checking an outcome. The backend records the attempt before writing;
repeated calls only return that receipt. A lost acknowledgement, concurrent edit
or incomplete readback returns an MCP error with `state: "uncertain"`, safe IDs
and reconciliation details. Read the file, pending changes or comment thread
before further work. A partial comment thread is retained for inspection.

**For launcher integration:** Fridica keeps login, account/project authorization
and saved credentials. Pass its existing `{baseUrl,cookie}` JSON envelope through
an anonymous Unix socket in `OL_SESSION_FD`, and set `INTERLEAF_NATIVE_RECEIPTS`
to private worker state outside the paper checkout. The socket is consumed and
closed on startup. Rust embedders can use `NativeSession::from_reader` and
`serve_with_native`. The supported origin is `https://www.overleaf.com`; the
native backend needs no Node process, browser, cookie file or Git credential.

The native tools appear only with an injected session. This development branch
has not replaced Fridica's running backend; its policy/packaging attachment and
live-account acceptance remain separate steps. No native accept/reject or
untracked text-write tool is exposed.
