# OKF as the central brain — implementation plan

**Status:** proposal, 2026-10-13
**Relates to:** [ADR 0015](../adr/0015-retrieval-scope-reduction.md) (retrieval scope reduction),
[ADR 0017](../adr/0017-learning-capture-and-approval.md) (learning capture and approval),
[ADR 0019](../adr/0019-managed-postgres-and-okf-knowledge.md) (managed Postgres and OKF storage),
[retrieval.md](../retrieval.md) (hybrid retrieval notes), PR #123 (TUI Knowledge tab)

## Goal

Make the OKF corpus the single durable store of what agents learn, per repository
and globally, that any writer can add to and any reader can search, while keeping
the properties ADR 0015 and ADR 0019 established: files are the source of truth,
injection into agents is deterministic, nothing in a hook needs the network, and
no writer can promote a rule into live injection without approval.

## Where we are

The corpus already has most of the spine this plan needs.

| Piece | State today |
| --- | --- |
| Document format | OKF 0.2 Markdown with YAML frontmatter; `type: Note` or `type: Engineering Rule`; `ygg:` profile carries id, scope, repo, state, approval digest |
| Scopes | `global/` and `repos/<uuid>/`, each with `notes/` and `learnings/` |
| Identity | portable repo UUID keyed off Git common dir + origin URL, so clones and worktrees share one id |
| Writers | `ygg remember`, `ygg learn add`, `ygg learn propose` (pending), approve / reject / edit / move |
| Readers | `ygg knowledge browse`, `ygg remember --list`, `ygg learn list`, TUI `[K] Knowledge` (PR #123) |
| Push into agents | session start: newest 5 active notes in scope; PreToolUse Edit/Write: rules whose `file_glob` matches the path; task claim: rules for the task's paths |
| Search | none; grep over the directory |
| Sync | Git transport exists (`src/knowledge/shared.rs`, remote authoritative, pending/confirm) but no CLI writes `shared.json` since the M7 fleet tooling was removed, so it is unreachable |
| Private corpus | plain directory, no history |
| Location | `<data_dir>/knowledge`; macOS default `~/Library/Application Support/ygg`, Linux `~/.local/share/ygg`; overridable with `YGG_KNOWLEDGE_DIR`, `YGG_DATA_DIR`, or settings |
| Vectors | none; managed Postgres ships only `plpgsql` and `uuid-ossp` and the upgrade path refuses other extensions |
| Git dependency | 45 `git` subprocess sites, no availability probe, no `NotFound` handling; knowledge hooks degrade to global-only with a per-call stderr line; repo-scoped writes fail |

## Principles that bound the design

1. **Files are the truth.** Every index (Git objects, BM25, vectors) is a cache rebuilt
   from the Markdown. Deleting a cache loses nothing.
2. **Push is deterministic, pull is searchable.** Session-start notes and edit-time
   glob rules keep today's predicates. Search, duplicate detection and suggestions may
   use ranking. No ranked retrieval auto-injects. This is the line ADR 0015 drew and
   the reason it removed similarity injection.
3. **Hooks are offline and fast.** Nothing in `ygg prime` or the PreToolUse path touches
   the network or a daemon.
4. **Writers never self-approve.** External writers land documents as `pending`
   (ADR 0017). Only `ygg learn approve` moves a rule into injection.
5. **Core before layers.** Files + Git + local BM25 need no credentials. AWS is an
   optional layer behind traits; unset, nothing changes; unreachable, search degrades
   to BM25 without error.

## Steps

Ordered by dependency. Each step is independently shippable and leaves the system
usable if the next never lands.

### Step 0 — Git-backed corpus by default

**Why.** History, recovery and sync for free, using the transport that already exists.
Adding a remote later becomes a config change, not a migration.

**What.**

- `ygg knowledge init` creates a local bare repository beside the corpus
  (`<data_dir>/knowledge-remote.git`) and writes `shared.json` with a `file://` remote
  pointing at it. The transport already accepts absolute local paths, so pending,
  confirm, recover and sync work unchanged with no network.
- `ygg knowledge init --adopt` imports an existing plain-directory corpus as the
  initial commit. The author's current corpus is the first test case.
- `ygg knowledge remote set <url>` pushes existing history to the new remote, rewrites
  `shared.json`, refreshes. Optional `--create-github <owner/name>` shells out to
  `gh repo create --private` when `gh` is present; refuse if the resulting repo is public.
- Init writes a `.gitignore` for `.lookup-*.json`, `.writer.lock`, `.export*`,
  `.sessions/`. The browse walk already skips dot entries.

**Fail-safe without git.**

- Probe `git --version` once and cache it, the way the tmux probe is cached.
- No git: init creates today's plain-directory corpus and prints one line saying
  history and remotes need git. `remote set` refuses with "git required".
- Transport selection already branches on `shared.json` presence, so a corpus created
  without git never touches the Git transport.
- Hooks map the missing binary to a one-time-per-session notice, then fall back to
  global knowledge silently. Real identity failures keep the per-call line.
- `ygg remember` / `ygg learn add` without `--global` and no git say exactly that and
  point at `--global`. The rule that an identity error never silently turns a repo
  note into a global one stays.

**Acceptance.**

- fresh `ygg knowledge init` on a machine with git yields `objects.git`, a bare remote,
  `shared.json`, one initial commit; `ygg knowledge pending` reports none
- `--adopt` on the author's corpus preserves every document byte-for-byte (browse
  revisions unchanged before and after)
- `remote set` to a scratch GitHub repo then `ygg knowledge sync` round-trips a note
  written on a second checkout
- with `git` removed from `PATH`: init succeeds in plain mode, `ygg prime` prints the
  notice once, `ygg remember --global` works, `ygg remember` without `--global` errors
  naming git

### Step 1 — Writer contract and `ygg knowledge add`

**Why.** Other tools, scripts and models should be able to write into the corpus
without reproducing the approval digest and legacy blocks that only `ygg` can produce.

**What.**

- Document the minimal input: `type` (Note | Engineering Rule), body, optional
  `file_glob`, `rule_id`, `context`, `scope` (`repo` default when resolvable, else
  `global`). Publish it as `docs/okf-writer-contract.md` and from
  `ygg knowledge schema`.
- `ygg knowledge add --stdin | --file <path>` parses that minimal frontmatter, fills
  the `ygg:` profile (id, scope, repo, user, created_by, created_at), writes to the
  canonical path, lands as `pending` for rules and `active` for notes, mirroring
  `remember` versus `propose` today.
- `--json` output returns id, path, revision, state so callers can follow up with
  `ygg learn approve`.
- Generic OKF documents (no Yggdrasil profile) remain allowed in the bundle and
  remain never-injected. Browse and the TUI show them under "other".

**Acceptance.**

- `printf -- '---\ntype: Engineering Rule\nfile_glob: src/*.rs\n---\nbody' | ygg knowledge add --stdin`
  produces a document that `ygg learn pending` lists and `ygg knowledge browse` prints
  with a complete profile
- a document with unknown top-level frontmatter keys is rejected with the key named
- an empty body is rejected

### Step 2 — Local search with BM25

**Why.** The corpus is not searchable in any surface. BM25 needs no embedder, no
network, no daemon, and runs in milliseconds.

**What.**

- Add `tantivy` as the embedded index. Index lives under the cache dir (not inside the
  corpus, not in Git), keyed by document id, with `revision` stored so staleness is
  detectable from the files alone. Fields: body, type, repo label, `file_glob`,
  `rule_id`, `context`, scope, state.
- Rebuild incrementally on every browse walk that observes changed revisions; a full
  rebuild is `ygg knowledge reindex`.
- `ygg knowledge search <query> [--repo|--all] [--kind note|rule] [--json]` returns
  ranked hits with path, type, score, and a one-line excerpt.
- TUI `[K]` tab gains `/` filter, following the Chat view's filter pattern:
  type to narrow, Enter to accept, Esc to clear. Matches on body, type, repo label.
- `ygg remember` and `ygg learn propose` run a search on the new text first and print
  the top 3 near matches as a duplicate warning. Non-blocking.

**Acceptance.**

- `ygg knowledge search "make install"` returns the Gatekeeper note in the top 3 on
  the author's corpus
- cold search over 406 documents completes under 50 ms after index build; index build
  under 2 s
- deleting the index directory and running any search rebuilds it transparently
- TUI `/` filter narrows the list and the group headers update their counts

### Step 3 — Optional semantic layer (AWS)

**Why.** BM25 misses paraphrase. A cheap, serverless vector layer closes that gap for
on-demand search and suggestions without touching the push path.

**What.**

- Two traits: `Embedder` (text → `Vec<f32>`) and `VectorStore` (upsert, delete,
  filtered nearest-neighbour). First implementations: Amazon Titan Text Embeddings v2
  via Bedrock at 512 dimensions; Amazon S3 Vectors as the store, one vector bucket and
  index per corpus id. S3 Vectors stores and filters only; the embedding comes from
  Bedrock.
- Embed on write at every revision-producing path (remember, learn add, approve, edit,
  move, `knowledge add`). Vector metadata carries `revision`, `repo`, `scope`, `type`,
  `state`, `user_id`, `file_glob`. Record vector id + embedded revision in the `ygg:`
  profile.
- `ygg knowledge sync` reconciles: diff on-disk revisions against stored metadata, embed
  the gaps, delete orphans. This covers documents that arrive through the Git remote
  from other machines, and is the full recovery path.
- Queries are hybrid: BM25 locally, S3 Vectors remotely with metadata filters for scope
  and state, merged by reciprocal rank fusion. No credentials or no network: BM25 alone,
  silently.
- Configuration: standard AWS credential chain; `region`, `vector_bucket`, `index`,
  `embedder` under a `[knowledge.semantic]` settings table; absent means disabled.
- **Never on the push path.** `ygg prime` and PreToolUse do not call this layer.

**Costs.** At the current corpus size (hundreds of short documents) Titan v2 embedding
and S3 Vectors storage, writes and queries are each a small fraction of a dollar per
month. Verify against the Bedrock and S3 Vectors pricing pages when writing the ADR.

**ADR required.** Amend ADR 0015 with the push-versus-pull rule above, and state
explicitly that document text leaves the machine to Bedrock and S3 in the operator's
account. Notes carry repo paths, PR numbers and incident text, so this is a deliberate
choice.

**Acceptance.**

- with the layer configured, `ygg knowledge search --semantic "signing cache after
  binary replace"` returns the Gatekeeper note; the same query with `AWS_REGION`
  unset returns BM25 results and no error
- `ygg knowledge sync` on a corpus with 10 unembedded documents embeds exactly 10 and
  is a no-op on the second run
- `ygg prime` and the PreToolUse hook make zero network calls with the layer configured
  (verify with a blocked network)

## Location of the corpus

Keep one corpus per user, outside any project checkout. Reasons:

- one bundle spans global plus every repo, so one remote, one index, one sync
- repo identity is per repository (common dir + origin), not per checkout; a directory
  inside a checkout is per clone, diverges across clones, and worktrees would need to
  find the main checkout's copy
- anything under a project is in scope for agent grep, glob and Edit, which bypasses
  the writer lock, revision digests and approval gate
- a nested repo with a different push target inside the project repo is legal for git
  but confusing for every tool that assumes one repo per directory

Visibility fixes that do not move the corpus:

- change the default to `~/.ygg/knowledge` on every platform; it is discoverable and
  sits where per-repo `.ygg/` scratch already trains people to look. Existing installs
  keep working through `YGG_KNOWLEDGE_DIR` / settings, which already take precedence
- add `ygg knowledge path`
- optional, gitignored, per-project symlink `.ygg/knowledge -> <corpus>/repos/<uuid>`;
  opt-in because it re-exposes the files to agents

## Non-goals

- re-enabling similarity-based auto-injection on prompts (ADR 0015)
- mirroring document text into Postgres for `tsvector` search; it reintroduces the
  two-store problem OKF removed
- a server or daemon of any kind for search
- Ollama or any local embedding daemon; if a local embedder is ever wanted it is an
  in-process ONNX model behind the same `Embedder` trait

## Open questions

- Should Step 0 also move the default location (`~/.ygg/knowledge`) in the same change,
  since init is the moment users first see the path?
- Multi-user corpora: documents carry `user_id`; search should default to the current
  user's documents with `--all-users` to widen. Confirm before Step 2.
- Rule approval for externally written rules: should `knowledge add` accept
  `--approve` for a trusted local operator, or always require a second command?
