# seedle

Export selected rows and storage buckets from a database into deterministic,
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

`pg_dump --data-only` is whole-database, has no per-table filtering, and its row
order is unspecified, so committing it produces diff noise on every run.
Hand-written seed SQL diffs cleanly but rots silently when the schema moves.

seedle exports the tables you name, filtered how you say, byte-identically run
to run, and refuses to run when the schema no longer matches the files.

## Install

```sh
brew install nathanduboisset/tap/seedle   # macOS and Linux
cargo binstall seedle                        # prebuilt binary, no compile
cargo install seedle                         # from source
```

Or download a binary from [Releases](https://github.com/NathanDuboisset/seedle/releases);
each archive ships a `.sha256`.

Needs Rust 1.85+ to build. Postgres 12+, Supabase, MySQL 8+ or SQLite at
runtime.

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
    process_env: true            # read credentials from the environment
    default: true                # used when --source is omitted
  local:
    engine: supabase             # postgres plus storage, no extra config
    env_file: .env

export:
  out: seed                      # holds seedle.lock and the data files
  format: jsonl                  # jsonl | jsonl.gz | csv | sql
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

Credentials never live in `seedle.yaml`. A source reads them from either an
`env_file` it names or, with `process_env: true`, the process environment. An
`env_file` still falls back to the environment for a variable it does not
contain, which is how CI usually supplies them.

`url_var` names the variable holding the connection URL; it defaults to
`DATABASE_URL`, or to `SUPABASE_DB_URL` then `DATABASE_URL` for
`engine: supabase`.

Exactly one source may be `default: true`. `read_only: true` makes `load` refuse
the source outright; use it on anything you export from.

### Engines

| `engine:` | Notes |
|---|---|
| `postgres` | Verified end to end |
| `supabase` | Postgres, plus storage discovery so buckets need no config |
| `mysql` | Verified end to end |
| `sqlite` | Verified end to end. `url: sqlite://app.db`, or a bare path |

`engine: supabase` looks for the storage URL in `SUPABASE_URL`,
`NEXT_PUBLIC_SUPABASE_URL`, `VITE_SUPABASE_URL` or `PUBLIC_SUPABASE_URL`, and
the key in `SUPABASE_SERVICE_ROLE_KEY`, `SUPABASE_SECRET_KEY` or
`SERVICE_ROLE_KEY`. A `storage:` block overrides either.

## Commands

| Command | What it does |
|---|---|
| `seedle init` | Write a starter config, optionally pre-filled from a live database |
| `seedle add TABLE...` | Append tables to the config, with the parents they need |
| `seedle sources` | List sources, resolve credentials, check connectivity |
| `seedle lock` | Introspect and write `seedle.lock` |
| `seedle lock --check` | Exit non-zero if the live schema differs. The CI gate |
| `seedle diff` | Show the drift, classified by severity |
| `seedle status` | Whether the seed files are current: drift plus row counts |
| `seedle export` | Pull rows and buckets into seed files |
| `seedle plan` | Load order, row counts, per-table action. Writes nothing |
| `seedle plan --tree` | The same, drawn as a dependency tree |
| `seedle load` | Push seed files into a database |
| `seedle verify` | Re-hash the seed files and bucket objects against the lock. Needs no network |
| `seedle completions SHELL` | Print a completion script |

Global flags: `--config`, `--source`, `--json`, `-v`, `-q`, `-y`.
`--tables a,b` narrows `export`, `plan` and `load`; `--buckets a,b` and
`--no-buckets` do the same for storage.

```
$ seedle plan --tree
load order for source "dev":
public.countries  3 rows
public.orgs  3 rows
  `- public.users  4 rows
     `- public.orders  2 rows

4 tables, 12 rows total
```

Completions install the usual way:

```sh
seedle completions bash > /etc/bash_completion.d/seedle
seedle completions zsh  > "${fpath[1]}/_seedle"
seedle completions fish > ~/.config/fish/completions/seedle.fish
```

### Referential completeness

A per-table `where` can orphan rows in another table, and the failure would
otherwise surface as a foreign-key violation partway through a load. `export`
checks every foreign key in the export against the export and aborts if any
value has no matching parent row:

```
$ seedle export
error: the export is not referentially complete:
  public.orders(user_id) -> public.users(id): 12 values with no matching row
    e.g. 9f2c4b1e-...; 0d1e2f3a-...
    widen the filter on public.users, or narrow the one on public.orders
```

`--no-fk-check` skips it.

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
  manifest.jsonl     one line per object: path, size, sha256, content type
  objects/<key>      the bytes, mirroring the object key
```

The manifest carries a sha256 per object, and `seedle.lock` records one hash
over the manifest, so a single value covers every byte in the bucket.
`seedle verify` re-hashes every object from disk with no network access.

Bucket *settings* (public, size limit, allowed mime types) are schema, created by
migrations. seedle records them in `seedle.lock` to check against and never
writes them: **a bucket that does not exist in the target is an error, not
something seedle creates.**

Loading uploads with upsert, after the database transaction commits. Object
storage has no transaction to join, so uploading earlier could leave files behind
for rows that were then rolled back. A local file whose hash no longer matches
the manifest is refused rather than pushed.

Object keys containing `..`, a leading `/`, or a backslash are refused rather
than rewritten, so a key cannot write outside the seed directory.

## Schema drift

Three severities, by what the change would do to a load.

**breaking** aborts. The load would be rejected or would corrupt data.

- a type narrowed (`text` to `varchar(20)`, `int8` to `int4`)
- nullability tightened to `NOT NULL` with no default
- a new `NOT NULL` column with no default
- the primary key changed, or a unique constraint dropped
- a foreign key added, dropped, or retargeted
- an enum label removed
- a column became generated
- a bucket missing, or its size limit below the largest exported object

**needs confirmation** prompts y/N, or accepts `--yes`. The load succeeds but
silently stops carrying data.

- a table dropped
- a column dropped that the seed files hold data for

**benign** proceeds.

- a new nullable column, or `NOT NULL` with a default
- a type widened (`varchar(50)` to `varchar(200)` to `text`, `int4` to `int8`)
- nullability relaxed
- a new enum label, a new unique constraint, an unlisted new table
- columns reordered, or a constraint renamed
- a bucket's visibility or mime allowlist changed

```
$ seedle diff

breaking:
  public.countries.region  column added (text, NOT NULL with no default)
                             every insert from the seed files would omit it and be rejected
  public.torture.t_i64     int64 -> int32
                             existing seed values may not fit or may fail to parse

needs confirmation:
  public.employees.name    column dropped (was text)
                             the seed files carry data for it, which will be discarded

2 breaking, 1 needing confirmation, 0 benign. Review, then re-run `seedle lock` to accept.
```

`seedle lock` accepts the change; `--force` proceeds without it.

`export` judges the source against the lock, `load` judges the target: each
checks the database it is about to act on.

## Determinism

These are enforced by the test suite, not just intended:

1. **Total row ordering.** The generated `SELECT` always ends in a total order:
   your `order_by`, then the primary key, then every remaining column. Ordering
   by a non-unique column alone leaves ties, and ties resolve differently run to
   run.
2. **Pinned collation.** Text ordering uses an explicit binary collation, because
   the default collation is a per-database property.
3. **The lock decides column order,** not the live database, so two databases
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

**jsonl** (default), one object per row. Best fidelity, cleanest diffs, since a
changed row is a changed line. `json: unroll` nests json columns instead of
escaping them.

**csv**: for interop. CSV cannot natively distinguish `NULL` from the empty
string, so seedle uses the Postgres `COPY ... CSV` convention, which round-trips:

> an **unquoted** empty field is `NULL`; a **quoted** empty field (`""`) is the
> empty string.

**sql**: batched `INSERT` statements with dialect-correct quoting and conflict
clauses, runnable through `psql` or `mysql`. `load` reads them back, accepting
the shape seedle writes and rejecting anything else rather than guessing.

**`layout: per_row`**: one `.json` file per row, named from the primary key, in
a directory named after the table. For small hand-edited tables where a
one-line-per-row diff is unreadable. `pretty: true` applies here only; jsonl is
one line per row by definition.

## Loading

`seedle load` runs inside a single transaction by default, so a failure part-way
through leaves the database exactly as it was.

Per-table modes:

- `upsert` (default), insert, updating non-key columns on conflict. Re-running
  converges on the file contents.
- `insert`, plain insert; a conflict aborts.
- `skip_existing`, ignore rows that already exist.
- `truncate_first`, empty the table first. Emptying happens as one pass in
  reverse foreign-key order, children before parents.

`DELETE FROM` rather than `TRUNCATE`: Postgres refuses to truncate a table any
foreign key references even when the referencing table is empty, and MySQL's
`TRUNCATE` commits implicitly, which would break atomicity.

After loading a serial/identity key, the sequence is advanced past the loaded
rows, so the application's next insert does not collide.

Confirmation is required when the target is not local or the plan destroys rows.
`--yes` skips it, `--dry-run` does everything except commit.

Foreign-key cycles are detected and named. Postgres can load one only when every
constraint in it is `DEFERRABLE`, in which case seedle defers them for the
transaction; otherwise it says so. A self-reference is not a cycle.

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
  rewrite the row on reload. Objects and arrays, the cases that benefit, nest
  normally.
- Postgres `numeric` NaN and exponent-form decimals are written as quoted strings
  in JSON, since neither survives a JSON number token unchanged.

## Licence

[PolyForm Noncommercial 1.0.0](LICENSE), **source-available, not open source**.

Use it freely for anything that is not for commercial advantage or monetary
compensation: personal projects, research, education, evaluation. Using it in or
for a business needs a separate licence; open an issue to ask.

Practical consequences worth knowing:

- `cargo install --git` and prebuilt binaries work as normal.
- crates.io accepts a custom licence file, but the crate will not show an
  OSI-approved licence, and some corporate policies auto-reject that.
- Homebrew *core* will not accept a non-OSI formula; the personal tap is fine.
