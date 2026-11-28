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

The Git workflow below needs git and TeX Live (`latexmk`, `latexdiff`) for `compile` and `review`.
Overleaf Git projects need Overleaf's Git integration. For editor services without Git, see
[native Overleaf collaboration](#native-overleaf-collaboration).

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
  markup; the native backend is described below.
- Before submission, accept or reject all suggestions. `check` reports any that are left.
  Then delete the suggestion block from the preamble.
- interleaf keeps its build output, review PDFs and figure sources in the clone's `.git/interleaf/`.
- Tests: `cargo test`. The end-to-end test runs against a local bare repository that stands in for
  the remote, with a second clone acting as the co-author.

## Native Overleaf collaboration

The Rust backend provides 33 canonical native tools and two
compatibility helpers. It uses Overleaf's editor services directly, without
`overleaf-mcp-native`, Node, npm, a Python adapter or a worker browser. The Git
workflow above remains separate.

### Login and scope

With Fridica, the owner signs in once:

```sh
fridica overleaf-auth login --expected-email owner@example.com
# For a self-hosted Overleaf / Community Edition installation:
fridica overleaf-auth login --base-url https://overleaf.example.org --expected-email owner@example.com
```

Passwords and MFA stay in the owner's isolated Chrome/Chromium window. Later
logins reuse the verified session in the OS credential store. The configured
origin is explicit; a stored session is never silently retargeted to another
host. Native server operations themselves run in Rust.

Existing sessions keep their seven previously authorized operations. The owner
can opt into the expanded tool capabilities without logging in again:

```sh
fridica overleaf-auth grant --all
```

This changes tool capabilities only. It cannot widen the saved account,
project, channel, recipient or expiry restrictions. Normal task admission,
approvals and workspace confinement still apply.

A task may discover projects, search and read while unbound, within its saved
scope. Every project call names an explicit `project_id`. Before a project
mutation, Fridica must confirm that project for the requesting thread; changing
a tool argument does not bind a different paper. Account-level project creation
requires an unbound task with unrestricted account/project scope.

### Native tools

The names below include the `native_` prefix used by the Rust MCP server.

| Capability | Canonical tools | Behavior |
| --- | --- | --- |
| Connection and projects (3) | `native_ping`, `native_list_projects`, `native_open_project` | Check the connection, discover accessible papers and inspect project metadata |
| Find and read (3) | `native_list_files`, `native_search_project`, `native_read_file` | Navigate the tree, search text, read text/version or binary content |
| Create and organize (7) | `native_create_project`, `native_create_doc`, `native_create_folder`, `native_upload_file`, `native_rename_entity`, `native_move_entity`, `native_delete_entity` | Create projects and manage project entities; upload paths remain confined |
| Edit text (2) | `native_edit_file`, `native_find_and_replace` | Minimal full-file diffs or exact replacements; tracking `on`, `off` or `auto` |
| Comments (9) | `native_list_comments`, `native_read_comment_thread`, `native_reply_comment`, `native_add_comment`, `native_resolve_comment`, `native_reopen_comment`, `native_delete_comment`, `native_edit_comment_message`, `native_delete_comment_message` | Read/filter review threads and manage exact comments/messages |
| Track Changes (4) | `native_list_tracked_changes`, `native_set_track_changes`, `native_accept_changes`, `native_reject_changes` | Filter pending records, change only your own switch, review precise selections |
| Cloud build (3) | `native_compile`, `native_read_log`, `native_label_pages` | Request an Overleaf cloud build, inspect its log and locate labels |
| Native history (2) | `native_list_history`, `native_diff_file` | Inspect Overleaf history and file differences |

The two additional helpers are `native_suggest_edit`, the pending-only exact
replacement alias, and `native_compile_local`, a local TeX build from a verified
live-text snapshot and exported assets. `native_compile` uses Overleaf's cloud
compiler; it is not the local snapshot helper. Local compilation needs TeX Live
and `latexmk`, and local fonts/packages can differ from the server. Snapshot
assets are not certified as one atomic Overleaf export.

### Safe editing and review

Read the current file first:

```json
{"project_id":"0123456789abcdef01234567","path":"main.tex"}
```

Pass the returned version to `native_find_and_replace`, for example:

```json
{
  "project_id":"0123456789abcdef01234567",
  "path":"main.tex",
  "old_string":"the old approximation",
  "new_string":"the revised approximation",
  "expected_version":95,
  "track":"on",
  "operation_id":"dcd3d917-b8bc-4c47-83b5-5bfd02567b8c"
}
```

`native_edit_file` accepts complete `new_content`; `replace_all` is available for
exact replacements. Version checks remain strict. Existing pending suggestions
must survive unchanged apart from predicted position shifts; edits that touch
them are refused. A server-side tracking switch can override `track:"off"`;
the result reports effective tracking instead of silently changing that switch.
Legacy `native_suggest_edit` always requests pending tracked changes.

Accept/reject requires precise change IDs or explicit filters/`all`, with exact
versions for every selected document. Message edits/deletes check the original
body or its SHA256. Edits are limited to your own messages; deleting another
author's message requires an explicit project-owner request. Thread deletion
checks every original message body.

Every mutation requires a stable `operation_id` UUID. Keep that ID and its
arguments when checking the receipt. The backend records intent before writing;
a repeated call reads the receipt instead of resubmitting. Lost acknowledgements,
concurrent edits and incomplete readback remain `state:"uncertain"` and an MCP
error. Reconcile by reading the file, pending records or thread. Partial results
are retained; there is no automatic replay or destructive rollback. Preserve
receipt state when moving execution hosts.

### Embedding and validation

Rust hosts use `login`, an in-memory `NativeSession` bound to the verified
identity, and `serve_native` with a per-call authorization callback.
`serve_with_native` additionally exposes the separate Git tools. Explicit HTTPS
origins support cloud and self-hosted deployments; loopback HTTP is for local
development/test setups. Credentials are never forwarded across origins.

mcp-name: io.github.happysky19/interleaf
