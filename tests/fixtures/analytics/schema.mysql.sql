-- Numeric and temporal extremes. Postgres and MySQL only: SQLite has no exact
-- decimal, so trailing zeros would be lost before GraineSQL saw the value.
CREATE TABLE accounts (
    id   INT PRIMARY KEY,
    plan ENUM('free','pro','team') NOT NULL
);

CREATE TABLE samples (
    id          INT PRIMARY KEY,
    account_id  INT NOT NULL,
    exact       DECIMAL(30,10),
    small_int   SMALLINT,
    big_int     BIGINT,
    approx      DOUBLE,
    payload     BLOB,
    happened_at TIMESTAMP NULL,
    wall_clock  DATETIME,
    on_day      DATE,
    note        TEXT,
    FOREIGN KEY (account_id) REFERENCES accounts(id)
);
