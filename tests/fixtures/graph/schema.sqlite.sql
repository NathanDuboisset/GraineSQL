-- Self-reference, a diamond, and a two-table cycle. SQLite defers every
-- foreign key inside a transaction with `PRAGMA defer_foreign_keys`, so the
-- cycle needs no DEFERRABLE declaration.
CREATE TABLE employees (
    id         INTEGER PRIMARY KEY,
    manager_id INTEGER REFERENCES employees(id),
    name       TEXT NOT NULL
);

CREATE TABLE top    (id INTEGER PRIMARY KEY, label TEXT NOT NULL);
CREATE TABLE t_left (id INTEGER PRIMARY KEY, top_id INTEGER NOT NULL REFERENCES top(id));
CREATE TABLE t_right(id INTEGER PRIMARY KEY, top_id INTEGER NOT NULL REFERENCES top(id));
CREATE TABLE bottom (
    id       INTEGER PRIMARY KEY,
    left_id  INTEGER NOT NULL REFERENCES t_left(id),
    right_id INTEGER NOT NULL REFERENCES t_right(id)
);

CREATE TABLE teams (
    id      INTEGER PRIMARY KEY,
    lead_id INTEGER REFERENCES members(id),
    name    TEXT NOT NULL
);
CREATE TABLE members (
    id      INTEGER PRIMARY KEY,
    team_id INTEGER NOT NULL REFERENCES teams(id),
    name    TEXT NOT NULL
);
