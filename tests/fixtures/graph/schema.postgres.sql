-- Self-reference, a diamond, and a deferrable two-table cycle.
CREATE TABLE employees (
    id         integer PRIMARY KEY,
    manager_id integer REFERENCES employees(id),
    name       text NOT NULL
);

-- top -> left, top -> right, both -> bottom.
CREATE TABLE top    (id integer PRIMARY KEY, label text NOT NULL);
CREATE TABLE t_left (id integer PRIMARY KEY, top_id integer NOT NULL REFERENCES top(id));
CREATE TABLE t_right(id integer PRIMARY KEY, top_id integer NOT NULL REFERENCES top(id));
CREATE TABLE bottom (
    id       integer PRIMARY KEY,
    left_id  integer NOT NULL REFERENCES t_left(id),
    right_id integer NOT NULL REFERENCES t_right(id)
);

-- Mutually dependent, so the load only works with constraints deferred.
CREATE TABLE teams (
    id       integer PRIMARY KEY,
    lead_id  integer,
    name     text NOT NULL
);
CREATE TABLE members (
    id      integer PRIMARY KEY,
    team_id integer NOT NULL,
    name    text NOT NULL
);
ALTER TABLE teams ADD CONSTRAINT teams_lead_fk
    FOREIGN KEY (lead_id) REFERENCES members(id) DEFERRABLE INITIALLY IMMEDIATE;
ALTER TABLE members ADD CONSTRAINT members_team_fk
    FOREIGN KEY (team_id) REFERENCES teams(id) DEFERRABLE INITIALLY IMMEDIATE;
