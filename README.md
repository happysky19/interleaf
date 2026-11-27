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

## Native Overleaf comments (development)

The `dev/native-overleaf` branch adds native `native_list_comments`,
`native_read_comment_thread` and `native_reply_comment` tools. They appear only
when the launcher injects an Overleaf session. Every call requires an explicit
`project_id`, so several papers can share one Fridica channel.

For example, read a review thread with:

```json
{"project_id": "0123456789abcdef01234567", "thread_id": "abcdef0123456789abcdef01"}
```

A reply also takes `content` and a stable `operation_id` UUID. Reuse that UUID
when checking the result. The backend saves an attempt before posting and reads
the reply back before reporting it confirmed. A lost response stays uncertain;
it never silently submits the reply again. Read the thread to reconcile it.

**For launcher integration:** keep login, account/project authorization and saved
credentials in Fridica. Pass its existing `{baseUrl,cookie}` JSON envelope through
an anonymous Unix socket in `OL_SESSION_FD`, and set `INTERLEAF_NATIVE_RECEIPTS`
to a private worker-state directory outside the paper checkout. The socket is
consumed and closed on startup. Rust embedders can use `NativeSession::from_reader`
and `serve_with_native` instead. The current origin is `https://www.overleaf.com`.
No browser, Node process, cookie file or Git credential is used by this backend.

This is the first development slice. Anchored comment creation and native Track
Changes are still pending; the running Fridica integration continues to use its
existing backend until those operations and its policy attachment are complete.
The existing Git and LaTeX tools keep working without a native session.
