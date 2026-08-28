CREATE TABLE authors (
    handle text PRIMARY KEY,
    name   text NOT NULL,
    bio    text
);

CREATE TABLE posts (
    slug          text PRIMARY KEY,
    author_handle text NOT NULL REFERENCES authors(handle),
    title         text NOT NULL,
    body          text NOT NULL,
    metadata      json,
    published     boolean NOT NULL,
    view_count    bigint NOT NULL
);

CREATE TABLE comments (
    id        bigint PRIMARY KEY,
    post_slug text NOT NULL REFERENCES posts(slug),
    author    text,
    body      text NOT NULL
);
