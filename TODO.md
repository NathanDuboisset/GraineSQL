# TODO

Ordered roughly by value. Items marked DONE landed recently and are listed for
context on what the next step builds on.

## Where seedle sits

The field splits into three groups:

- **Subsetting + anonymisation**: [Greenmask](https://github.com/GreenmaskIO/greenmask),
  [Basecut](https://xata.io/blog/the-5-best-data-anonymization-tools-for-development-teams-in-2026),
  Tonic Structural, `pg_anonymizer`. Snaplet shut down Aug 2024, Neosync was
  archived Aug 2025, so this space is consolidating.
- **Generation**: [Seedfast](https://seedfa.st/blog/database-seeding), drizzle-seed,
  ORM seeders, factory libraries.
- **Dump/restore**: `pg_dump`, `mysqldump`, Greenmask as a dump proxy.

seedle is none of these. It commits a real, reviewable slice of data to git and
refuses to run when the schema moved under it. Nothing above produces
byte-stable files intended to be diffed and reviewed; Greenmask emits
`pg_restore`-compatible dumps, the generators emit fresh data every run.

The features they have and we do not are listed below.

## Engines

- [ ] **SQLite.** No server, so the fastest possible test suite and the obvious
      target for a `--engine sqlite` smoke test in CI. Needs: `sqlite` sqlx
      feature, `PRAGMA table_info`/`foreign_key_list` introspection, a dialect
      (no schemas, `INSERT OR REPLACE`, `PRAGMA foreign_keys=off`), and type
      affinity mapping rather than declared types.
- [ ] **Verify MySQL against a live server.** Implemented and unit-tested,
      never run. Check first: `TINYINT(1)` as boolean, `BIGINT UNSIGNED` as
      decimal, `TIMESTAMP` vs `DATETIME` zone-awareness, inline enum keying.
- [ ] **MongoDB / document stores.** Different shape: no schema to lock, no FKs
      to order. Would need a separate `Engine` trait with collections instead of
      tables, JSON Schema validators as the drift contract, and `_id` as the
      key. Worth doing only after the relational side is settled; the lock and
      drift model do not transfer as-is.
- [ ] **Redis / key-value.** Weaker fit again: no schema, no referential
      integrity, and the value is mostly "dump these keys". Probably a separate
      tool.
- [x] DONE **Supabase engine.** `engine: supabase` is Postgres plus storage
      discovery, so no `storage:` block and no repeated URL/key vars.

## Correctness

- [x] DONE **Referential closure check on export.** Aborts when a filter
      orphans rows, names the tables, suggests the fix, `--no-fk-check` to
      override. This was the biggest gap against the subsetting tools.
- [ ] **Follow foreign keys to pull required parents.** The check tells you the
      slice is broken; the subsetting tools fix it for you. Given
      `users: {where: "id = 5"}`, walk the FK graph and pull the `orgs` row it
      needs. Design questions: bound the walk (a chain can drag in half the
      database), handle cycles, decide whether pulled-in rows are written to the
      parent's own seed file (yes) and how that interacts with the parent's own
      `where`.
- [ ] **Row-count and content diff before load.** `seedle plan` says what will
      be loaded; it does not say what will change. Show added/updated/removed
      per table by comparing keys against the target.
- [ ] **Composite and partial unique keys as upsert targets.** Partial indexes
      are excluded today; a partial unique index is a legitimate conflict target
      with the right `WHERE`.

## Drift

- [x] DONE **Three severities.** `breaking` aborts, `needs confirmation`
      prompts y/N (or `--yes`), `benign` proceeds. A dropped table or column is
      now confirmation rather than a hard stop: the load works, it just
      discards that data.
- [ ] **Per-table drift policy.** `on_drift: abort | confirm | ignore` so a
      volatile table can be exempt without `--force` disabling the check
      everywhere.
- [ ] **Drift on the data, not just the schema.** `seedle verify` catches an
      edited seed file. It does not catch "the target has rows the seed files do
      not", which is what makes a `truncate_first` decision informed.

## Data handling

- [x] DONE **Load `.sql` files.** The writer's own output reads back, both
      dialects, batching and conflict clauses included.
- [ ] **Anonymisation / masking.** The one feature every tool in the space has
      and seedle does not. Committing production rows to git is a PII problem.
      Minimum viable: per-column `mask: hash | null | fixed | fake_email`,
      deterministic from a salt so the same input maps to the same output and
      the files stay byte-stable. This is the single biggest blocker to using
      seedle against a real production source.
- [ ] **Compression for large seed sets.** `format: jsonl.gz` for tables too
      big to review by eye but still wanted in git-lfs.
- [ ] **Streaming writes.** Each seed file is built in memory. Fine for
      reviewable data, not for a 500 MB table.
- [ ] **`COPY`-based load.** One `INSERT` per row is slow above ~10k rows.
      `COPY FROM STDIN` for `insert`/`truncate_first` modes, keeping the
      per-row path for upserts.

## Ergonomics

- [x] DONE **`process_env: true`.** Read credentials from the process
      environment with no `.env` file.
- [ ] **`seedle add <table>`.** Append a table to the config with its FK
      parents, instead of hand-editing YAML and re-running lock.
- [ ] **`seedle status`.** One command answering "is my seed data current
      against this database": drift, file hashes, row-count deltas.
- [ ] **Better `plan` output.** Show the FK graph as a tree so the load order is
      self-explaining.
- [ ] **Shell completions.** `clap_complete` for bash/zsh/fish.

## Distribution

- [ ] Create `NathanDuboisset/homebrew-tap`, set `CARGO_REGISTRY_TOKEN` and
      `HOMEBREW_TAP_TOKEN`, then tag `v0.1.0`.
- [ ] Decide on public vs private. Homebrew and crates.io both need public
      release artefacts.
- [ ] `gh auth refresh -s workflow` so HTTPS pushes can touch
      `.github/workflows/`.
