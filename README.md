# seedle

Export selected rows — and storage buckets — from a database into deterministic,
diffable seed files. Load them back into another database in foreign-key order.

```
seedle export                 # prod  -> seed/*.jsonl + buckets/   (commit these)
seedle load --source dev      # files -> dev database and its buckets
```

A `seedle.lock` file records the schema the seed files were written against.
Every command checks the live database against it and **aborts on breaking drift
before touching anything**, so a schema change surfaces as a readable diff rather
than a half-applied load.

## Why not `pg_dump`

`pg_dump --data-only` is whole-database, has no per-table row filtering, and its
output ordering is whatever the storage engine felt like — so committing it
produces diff noise on every run. Hand-written seed SQL solves the diff problem
and creates a worse one: it rots silently when the schema moves under it.

seedle exports the tables you name, filtered how you say, in a form that is
byte-identical run to run, and refuses to run at all when the schema no longer
matches what the files were written against.

## Install

```sh
brew install nathanduboisset/tap/seedle   # macOS and Linux
cargo binstall seedle                        # prebuilt binary, no compile
cargo install seedle                         # from source
```

Or download a binary from [Releases](https://github.com/NathanDuboisset/seedle/releases);
each archive ships a `.sha256` beside it.

Building from source needs Rust 1.85+ (2024 edition). Runtime: Postgres 12+ or
MySQL 8+.

## Getting started

```sh
seedle init --url postgres://localhost/myapp   # writes seedle.yaml, lists tables
$EDITOR seedle.yaml                            # trim to the tables you want
seedle lock                                    # snapshot the schema
seedle export                                  # write seed/
git add seed/                                  # commit
```

Then in a fresh environment:

```sh
seedle load --source dev
```

## Configuration

```yaml
version: 1

sources:
  prod:
    engine: postgres
    env_file: .env.prod          # credentials never live in this file
    url_var: DATABASE_URL
    read_only: true              # refuse `load` against this source
  dev:
    engine: postgres
    env_file: .env
    default: true                # used when --source is omitted

export:
  out: seed                      # holds seedle.lock and the data files
  format: jsonl                  # jsonl | csv | sql
  json: unroll                   # nest json columns instead of escaping them
  sql_batch: 100                 # rows per INSERT when format: sql
  concurrency: 4                 # tables exported at once; cannot affect output

load:
  default: upsert                # upsert | insert | skip_existing | truncate_first
  transaction: true
  fix_sequences: true

tables:                          # ONLY these tables are ever touched
  countries: {}
  orgs:
    order_by: [id]
  users:
    where: "created_at > now() - interval '90 days'"   # raw SQL, source dialect
    order_by: [created_at, id]
    limit: 500
    exclude_columns: [password_hash, session_token]
  orders:
    where: "status <> 'draft'"
    load: insert
  settings:
    load: truncate_first
  templates:
    layout: per_row              # one file per row, in templates/
    format: json
    pretty: true
```

Per-table keys: `where`, `order_by`, `limit`, `columns`, `exclude_columns`,
`format`, `layout`, `pretty`, `json`, `load`, `key`.

### Sources

Credentials are read from the `.env` file a source names, not from
`seedle.yaml`, and not through the process environment — so two sources can use
the same variable name without colliding. Exactly one source may be
`default: true`.

`read_only: true` makes `load` refuse the source outright. Use it on anything
you export from.

## Commands

| Command | What it does |
|---|---|
| `seedle init` | Write a starter config, optionally pre-filled from a live database |
| `seedle sources` | List sources, resolve credentials, check connectivity |
| `seedle lock` | Introspect and write `seedle.lock` |
| `seedle lock --check` | Exit non-zero if the live schema differs. The CI gate |
| `seedle diff` | Show the drift, classified breaking/benign |
| `seedle export` | Pull rows into seed files |
| `seedle plan` | Load order, row counts, per-table action. Writes nothing |
| `seedle load` | Push seed files into a database |
| `seedle verify` | Re-hash the seed files and bucket objects against the lock. Needs no network |

Global flags: `--config`, `--source`, `--json`, `-v`, `-q`.
`--tables a,b` narrows `export`, `plan`, and `load`; `--buckets a,b` and
`--no-buckets` do the same for storage.

## Storage buckets

Object storage is the half of a Supabase project a SQL dump cannot capture. Point
a source at its storage service and list the buckets:

```yaml
sources:
  local:
    engine: postgres
    env_file: .env
    url_var: SUPABASE_DB_URL
    storage:
      url_var: SUPABASE_URL                 # http://127.0.0.1:54321
      key_var: SUPABASE_SERVICE_ROLE_KEY    # the service-role key, not anon

buckets:
  project_files: {}
  avatars:
    prefix: "public/"          # only objects under this key prefix
    max_object_bytes: 1048576  # refuse anything larger (default 25 MiB)
```

Each bucket lands as:

```
seed/buckets/project_files/
  bucket.json        settings: public, size limit, allowed mime types
  manifest.jsonl     one line per object: path, size, sha256, content type
  objects/<key>      the bytes, mirroring the object key
```

The manifest carries a **sha256 per object**, and `seedle.lock` records one hash
over the manifest and settings — so a single value covers every byte in the
bucket, and a change to any file, name, or setting moves it. `seedle verify`
re-hashes every object from disk and needs no network.

Loading uploads with upsert, creating the bucket if it is missing, and runs
**after** the database transaction commits — object storage has no transaction to
join, so uploading earlier could leave files behind for rows that were then
rolled back. A local file whose hash no longer matches the manifest is refused
rather than pushed.

Object keys are validated before any bytes move: a key containing `..`, a leading
`/`, or a backslash is refused rather than silently rewritten, so a hostile key
cannot write outside the seed directory.

## Schema drift

The distinction that matters: *breaking* drift would reject or corrupt data, so
it aborts before anything is touched; *benign* drift cannot, so it warns and
proceeds.

**Breaking — aborts:**

- a table or a locked column dropped
- a type narrowed (`text` → `varchar(20)`, `int8` → `int4`, `timestamptz` → `date`)
- nullability tightened to `NOT NULL` with no default
- a new `NOT NULL` column with no default
- the primary key changed, or a unique constraint dropped
- a foreign key added, dropped, or retargeted
- an enum label removed
- a column became generated

**Benign — warns and continues:**

- a new nullable column, or `NOT NULL` with a default
- a type widened (`varchar(50)` → `varchar(200)` → `text`, `int4` → `int8`)
- nullability relaxed
- a new enum label, a new unique constraint, an unlisted new table
- columns physically reordered, or a constraint renamed

```
$ seedle diff

breaking:
  public.countries.region  column added (text, NOT NULL with no default)
                             every insert from the seed files would omit it and be rejected
  public.employees.name    column dropped (was text)
                             the seed files carry a value for it
  public.torture.t_i64     int64 -> int32
                             existing seed values may not fit or may fail to parse

3 breaking, 0 benign. Review, then re-run `seedle lock` to accept.
```

Review, then `seedle lock` to accept the change, or `--force` to proceed anyway.

Note that `export` judges the **source** against the lock while `load` judges the
**target** — each checks the database it is about to act on.

## Determinism

These are enforced by the test suite, not just intended:

1. **Total row ordering.** The generated `SELECT` always ends in a total order:
   your `order_by`, then the primary key, then every remaining column. Ordering
   by a non-unique column alone leaves ties, and ties resolve differently run to
   run.
2. **Pinned collation.** Text ordering uses an explicit binary collation, because
   the default collation is a per-database property.
3. **The lock decides column order,** not the live database — so two databases
   holding the same columns in different physical order still export identically.
4. **Canonical values.** Shortest round-trip floats; decimals kept as exact digit
   strings, never through `f64`; timestamps normalised to UTC with fixed
   fractional precision; bytes as lowercase `\x` hex; intervals as ISO 8601.
5. **Pinned session.** Reads run with `TimeZone=UTC`, `IntervalStyle=iso_8601`,
   `bytea_output=hex`, `DateStyle=ISO`, so no server or role setting can change
   the output.
6. **Stable bytes.** UTF-8, no BOM, LF only, trailing newline. No timestamps,
   hostnames, or versions anywhere in a file body.
7. **Concurrency cannot matter.** Tables export in parallel, but each file has a
   single writer.

## Formats

**jsonl** (default) — one object per row. Best fidelity and the cleanest diffs,
since a changed row is a changed line. `json: unroll` nests json/jsonb columns
instead of escaping them.

**csv** — for interop. CSV cannot natively distinguish `NULL` from the empty
string, so seedle uses the Postgres `COPY ... CSV` convention, which round-trips:

> an **unquoted** empty field is `NULL`; a **quoted** empty field (`""`) is the
> empty string.

**sql** — batched `INSERT` statements with the right dialect quoting and conflict
clause, runnable straight through `psql` or `mysql`. Write-only: seedle does not
read `.sql` back, because that would need a full dialect parser. `load` says so
explicitly rather than failing obscurely.

**`layout: per_row`** — one `.json` file per row, in a directory named after the
table, named from the primary key. For small human-edited tables (templates,
config rows) where a one-line-per-row diff is unreadable. This is where
`pretty: true` applies; pretty-printing is meaningless in jsonl, which is one
line per row by definition.

## Loading

`seedle load` runs inside a single transaction by default, so a failure part-way
through leaves the database exactly as it was.

Per-table modes:

- `upsert` (default) — insert, updating non-key columns on conflict. Re-running
  converges on the file contents.
- `insert` — plain insert; a conflict aborts.
- `skip_existing` — ignore rows that already exist.
- `truncate_first` — empty the table first. Emptying happens as one pass in
  reverse foreign-key order, children before parents.

`TRUNCATE` is deliberately not used: Postgres refuses to truncate a table any
foreign key references, even when the referencing table is empty, and MySQL's
`TRUNCATE` performs an implicit commit that would break the load's atomicity.
`DELETE FROM` has neither problem.

After loading a table with a serial/identity key, the sequence is advanced past
the loaded rows — without this the application's next insert collides with a seed
row.

**Confirmation** is required when the target is not local or when the plan
destroys rows. `--yes` skips it; `--dry-run` does everything except commit.

**Foreign-key cycles** are detected and named. Postgres can only load a cycle
when every constraint in it is `DEFERRABLE`; if so, seedle defers them for the
transaction, and if not it says exactly that rather than failing obscurely.
A self-reference (`manager_id` pointing at the same table) is not a cycle and is
resolved within the table's own batch.

## CI

```sh
seedle lock --check   # fails if the schema moved without the lock being updated
seedle verify         # fails if the seed files no longer match the lock
```

`verify` needs no database.

## Testing

```sh
cargo test                                              # unit tests only
SEEDLE_TEST_PG=postgres://user@localhost cargo test     # + database tests

# Bucket tests additionally need a storage service:
export SEEDLE_TEST_STORAGE_URL=http://127.0.0.1:54321
export SEEDLE_TEST_STORAGE_KEY=<service-role key>
cargo test --test buckets
```

The integration tests create and drop their own databases and buckets, and skip
rather than fail when those variables are unset. The most valuable one is
`export_then_load_then_export_is_byte_identical`: it exercises every encoder and
decoder at once and fails on any asymmetry between them.

## Engine support

**Postgres** is verified end-to-end against a live server: introspection, all
four formats, every load mode, drift classification, cycles, and the full
export → load → re-export round trip.

**MySQL** has the dialect layer (quoting, literals, `ON DUPLICATE KEY UPDATE`,
`INSERT IGNORE`, `UNHEX`, `FOREIGN_KEY_CHECKS`) and `information_schema`
introspection implemented and unit-tested, but it has **not been exercised
against a live MySQL server** — no server was available in the environment where
it was written. Treat it as untested: the type-mapping decisions it makes are
documented in `src/db/mysql.rs`, and the ones worth checking first are

- `TINYINT(1)` read as boolean (a convention, not a guarantee),
- `BIGINT UNSIGNED` carried as an exact decimal, since it overruns `i64`,
- `TIMESTAMP` treated as zone-aware and `DATETIME` as not — which is correct but
  the opposite of what the names suggest,
- enums keyed by their inline declaration, there being no named type.

## Limitations

- A table's rows are read as a stream but each seed file is built in memory. Seed
  data that does not fit in memory does not belong in git either.
- `.sql` output cannot be read back.
- Bucket objects are held in memory one at a time while hashing, so
  `max_object_bytes` (default 25 MiB) guards against pulling something into git
  that does not belong there.
- Supabase Storage rejects non-ASCII object keys itself, so those cannot occur.
- In `json: unroll` mode, a json column whose value is a bare scalar (`null`,
  a number, a string) is written quoted rather than nested. Unrolling those would
  make a JSON `null` indistinguishable from SQL `NULL`, which would silently
  rewrite the row on reload. Objects and arrays — the cases that benefit — nest
  normally.
- Postgres `numeric` NaN and exponent-form decimals are written as quoted strings
  in JSON, since neither survives a JSON number token unchanged.

## Licence

[PolyForm Noncommercial 1.0.0](LICENSE) — **source-available, not open source**.

Use it freely for anything that is not for commercial advantage or monetary
compensation: personal projects, research, education, evaluation. Using it in or
for a business needs a separate licence; open an issue to ask.

Practical consequences worth knowing:

- `cargo install --git` and prebuilt binaries work as normal.
- crates.io accepts a custom licence file, but the crate will not show an
  OSI-approved licence, and some corporate policies auto-reject that.
- Homebrew *core* will not accept a non-OSI formula; the personal tap is fine.
