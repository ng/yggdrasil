# Managed database and knowledge compatibility contracts

These fixtures capture baseline `0203e7a` for ADR 0019. They are inputs to both
legacy SQL tests and the forthcoming OKF adapters, not a declaration that the
cutover or managed runtime is complete.

`configuration.json` defines target resolution. `src/config/database.rs` resolves
it without connecting, starting a process, creating directories, or mutating the
environment. Integration into command dispatch follows the managed runtime spike.
The existing AppConfig remains in use until that runtime can preserve existing
installations. `YGG_DB_MODE` accepts `managed` and `external`; an explicit managed
mode conflicts with any external URL. Empty URLs are errors, not permission to
create a new cluster. Environment values override equivalent TOML fields.

User configuration lives at `$YGG_CONFIG_DIR`, `$XDG_CONFIG_HOME/ygg`, or
`~/.config/ygg`, in that order. Only that directory's `.env` supplies legacy
fallbacks; inherited environment values win. TOML fields are `data_dir`,
`knowledge_dir`, `profile`, and `[database]` with `mode` and `url`. Data paths must
be absolute. Data defaults to `$XDG_DATA_HOME/ygg`, macOS
`~/Library/Application Support/ygg`, or Linux `~/.local/share/ygg`; `YGG_DATA_DIR`
overrides the base. A non-default `YGG_PROFILE` uses `profiles/<profile>` under
that base. Knowledge defaults to `knowledge` under the profile data directory;
`YGG_KNOWLEDGE_DIR` independently overrides it. Database location never enables
sharing or publication of knowledge.

## Legacy data and retrieval

`learnings.json` and `notes.json` preserve UUIDs, nullable fields, Unicode,
newlines, empty context, timestamps, scope tags, activation and approval evidence.
`tests/knowledge_contracts.rs` loads them into temporary tables in an isolated
migrated database and calls the existing repositories. `tests/remember.rs` retains
the broader note regression suite.

- Note lists are newest-first; current repo includes global, null repo without
  `all` means global only. Prime requests five notes.
- A null learning repo filter searches all repos. Non-null searches repo + global.
- Only active learnings match; pending rules never fire.
- SQL `LIKE translate(file_glob, '*?', '%_')` is case-sensitive, crosses directory
  separators, keeps `%` and `_` wildcards, and honors backslash escaping.
- Null file and rule filters mean any. A non-null rule filter excludes a null
  rule ID. Absent/null agent/kind tags are unconstrained; null query dimensions
  match even constrained tags.
- Specificity counts non-null rule ID and file glob, descending, then recency.
  The baseline does not order equal timestamps; OKF adds UUID ascending ties.
- Approving a proposal retains source and records time; a null approval actor
  remains null. Rejecting an active learning does nothing. Legacy manual
  activation becomes explicit legacy evidence, never invented human verification.
- SQL `user_id` exists on both tables but is absent from legacy JSON models.
  Export must read it explicitly and require mapping for empty IDs; it cannot
  infer ownership from these API fixtures or the operator's username.

Legacy JSON learning and memory fields remain adapter contracts. Learn list uses
`count`/`results`, pending also uses `count`/`results`; remember list uses
`count`/`results`. Creation returns the model object. These wrappers still require
end-to-end CLI coverage during M5, as do hook per-session deduplication and offline
prime behavior.

## OKF and identity mapping

Specification: OKF 0.2, knowledge-catalog revision
`1d36d9d31c1fac43ccb74caba1c5483981997e58`,
<https://github.com/GoogleCloudPlatform/knowledge-catalog/blob/1d36d9d31c1fac43ccb74caba1c5483981997e58/okf/SPEC.md>.
The rule.md parser fixture preserves unknown metadata and original body text.
`type: Note` and `type: Engineering Rule` are Yggdrasil's profile. Activation
requires trusted corpus configuration plus Yggdrasil evidence bound to body,
context and scope; OKF descriptive status/verification alone never activates.

Paths use `global/{notes,learnings}/<UUID>.md` or
`repos/<portable-repo-UUID>/{notes,learnings}/<UUID>.md`. Preserve document IDs.
Portable repo IDs bind to canonical URL aliases and Git common-directory identity;
Postgres repo IDs remain explicit legacy mappings. Never use basename identity or
turn unmapped imports into global scope. Bindings/corpus identity are backed up
outside bundles. `approval-input.json` pins compact UTF-8 JSON with sorted keys, explicit nulls,
and exact body/context bytes; `approval-digest.txt` pins its SHA-256. The digest
covers identity, type, scope and matching fields, while excluding display metadata
and telemetry. `tests/okf_documents.rs` tests round trips, bounded alias expansion,
expiry, import deactivation, and digest invalidation without Postgres. Durable
filesystem writes, identity bindings, indexing and adapters remain M4/M5 work.

## Platform release matrix

| Target | Managed release target | Evidence still required |
| --- | --- | --- |
| macOS arm64 | Yes | Signed release artifact, pinned PG16 archive, crash/startup smoke |
| macOS x86_64 | Yes | Signed release artifact, pinned PG16 archive, crash/startup smoke |
| Linux x86_64 | Yes | Runtime libraries, pinned PG16 archive, crash/startup smoke |
| Other targets | External only | Managed support not advertised |

No managed target is validated yet. External PG16 and PG18 must pass the same
coordination suite, plus hostname/CA verification and limited-role tests. Final
SQL knowledge removal also requires guarded cutover, lossless rollback rehearsal,
and 14 days of dogfooding; these fixtures do not waive any release gate.
