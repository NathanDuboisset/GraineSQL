-- Self-reference, a diamond, and a two-table cycle. MySQL has no deferrable
-- constraints; seedle suspends FOREIGN_KEY_CHECKS for the transaction instead.
CREATE TABLE employees (
    id         INT PRIMARY KEY,
    manager_id INT NULL,
    name       TEXT NOT NULL,
    FOREIGN KEY (manager_id) REFERENCES employees(id)
);

CREATE TABLE top (id INT PRIMARY KEY, label TEXT NOT NULL);
CREATE TABLE t_left (
    id INT PRIMARY KEY, top_id INT NOT NULL,
    FOREIGN KEY (top_id) REFERENCES top(id)
);
CREATE TABLE t_right (
    id INT PRIMARY KEY, top_id INT NOT NULL,
    FOREIGN KEY (top_id) REFERENCES top(id)
);
CREATE TABLE bottom (
    id       INT PRIMARY KEY,
    left_id  INT NOT NULL,
    right_id INT NOT NULL,
    FOREIGN KEY (left_id) REFERENCES t_left(id),
    FOREIGN KEY (right_id) REFERENCES t_right(id)
);

CREATE TABLE teams (
    id      INT PRIMARY KEY,
    lead_id INT NULL,
    name    TEXT NOT NULL
);
CREATE TABLE members (
    id      INT PRIMARY KEY,
    team_id INT NOT NULL,
    name    TEXT NOT NULL
);
ALTER TABLE teams   ADD FOREIGN KEY (lead_id) REFERENCES members(id);
ALTER TABLE members ADD FOREIGN KEY (team_id) REFERENCES teams(id);
