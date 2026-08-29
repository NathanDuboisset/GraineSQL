-- Numeric and temporal extremes. Postgres and MySQL only: SQLite has no exact
-- decimal, so trailing zeros would be lost before GraineSQL saw the value.
CREATE TYPE tier AS ENUM ('free', 'pro', 'team');

CREATE TABLE accounts (
    id   integer PRIMARY KEY,
    plan tier NOT NULL
);

CREATE TABLE samples (
    id           integer PRIMARY KEY,
    account_id   integer NOT NULL REFERENCES accounts(id),
    exact        numeric(30,10),
    small_int    smallint,
    big_int      bigint,
    approx       double precision,
    payload      bytea,
    happened_at  timestamptz,
    wall_clock   timestamp,
    on_day       date,
    note         text
);
