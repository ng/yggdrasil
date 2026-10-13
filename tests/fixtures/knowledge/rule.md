---
type: Engineering Rule
title: Keep migrations forward only
status: draft
x-producer:
  nested: [one, {two: 2}]
ygg:
  schema_version: 1
  id: 8d63b13c-9904-428a-a070-c959262b0e54
  scope: repo
  repo: 9a467418-ddaa-4ee6-b5f2-5b2d2d7d7040
  legacy_repo_id: null
  user_id: null
  created_by: null
  created_at: 2026-10-08T00:00:00Z
  file_glob: "migrations/*.sql"
  rule_id: forward-only-migrations
  scope_tags: {kind: chore}
  context: ""
  state: pending
  source: proposed
  approval: null
  x-future-policy: preserved
---
Add a new migration rather than changing an applied migration.

Preserve Unicode λ, Markdown `code`, and trailing newline.
