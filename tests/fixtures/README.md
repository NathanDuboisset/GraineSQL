# Fixtures

Each directory is a complete seedle project: a config, a schema per engine, the
data it was built from, and the seed files themselves. `tests/fixtures.rs` makes
two checks per fixture.

**Anchor.** Build a database from `data.sql`, export, and compare to the
committed seed files. This ties the fixture to a source seedle never wrote, so
an edited or stale file fails.

**Replay.** For every engine the fixture declares, load the committed seed files
and export them again; the result must be byte-identical.

The anchor matters more than it looks. Replay alone only proves the loader and
the exporter agree with each other, which they would even if both were wrong in
the same way, and it passes just as happily on a file someone edited by hand.

The seed files are generated, not hand-written: `just regen-fixtures` (or the
`UPDATE_FIXTURES=1` env var) rebuilds them from `data.sql` through seedle
itself. Reviewing the resulting diff is the point, so regenerate deliberately.

| Fixture | Exercises | Engines |
|---|---|---|
| `ecommerce` | a six-deep foreign-key chain, composite keys, money | all |
| `cms` | json documents, unicode, embedded quotes and newlines, nulls | all |
| `analytics` | numeric and temporal extremes, bytes, arrays, enums | postgres, mysql |
| `graph` | self-references, a deferrable cycle, diamond dependencies | all |

## Why some fixtures skip SQLite

SQLite has no exact decimal type. A column declared `DECIMAL(14,4)` has NUMERIC
affinity, so `19.9900` is stored as the float `19.99` and the trailing zeros are
gone before seedle ever sees the value. The fixtures that share files across all
three engines therefore avoid trailing-zero decimals; `analytics` keeps them and
skips SQLite, because losing them silently is exactly the kind of thing these
tests exist to catch.
