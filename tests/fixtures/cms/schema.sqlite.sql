CREATE TABLE authors (
    handle TEXT PRIMARY KEY,
    name   TEXT NOT NULL,
    bio    TEXT
);

CREATE TABLE posts (
    slug          TEXT PRIMARY KEY,
    author_handle TEXT NOT NULL REFERENCES authors(handle),
    title         TEXT NOT NULL,
    body          TEXT NOT NULL,
    metadata      JSON,
    published     BOOLEAN NOT NULL,
    view_count    BIGINT NOT NULL
);

CREATE TABLE comments (
    id        BIGINT PRIMARY KEY,
    post_slug TEXT NOT NULL REFERENCES posts(slug),
    author    TEXT,
    body      TEXT NOT NULL
);
