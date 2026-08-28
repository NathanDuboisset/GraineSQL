CREATE TABLE authors (
    handle VARCHAR(64) PRIMARY KEY,
    name   TEXT NOT NULL,
    bio    TEXT
);

CREATE TABLE posts (
    slug          VARCHAR(128) PRIMARY KEY,
    author_handle VARCHAR(64) NOT NULL,
    title         TEXT NOT NULL,
    body          TEXT NOT NULL,
    metadata      JSON,
    published     TINYINT(1) NOT NULL,
    view_count    BIGINT NOT NULL,
    FOREIGN KEY (author_handle) REFERENCES authors(handle)
);

CREATE TABLE comments (
    id        BIGINT PRIMARY KEY,
    post_slug VARCHAR(128) NOT NULL,
    author    TEXT,
    body      TEXT NOT NULL,
    FOREIGN KEY (post_slug) REFERENCES posts(slug)
);
