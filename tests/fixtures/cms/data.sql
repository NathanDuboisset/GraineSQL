INSERT INTO authors VALUES ('amelie', 'Amélie Dupont', 'Writes about typography.');
INSERT INTO authors VALUES ('bjorn', 'Björn Ålesund', NULL);
INSERT INTO authors VALUES ('chika', '中村 千夏', 'Editor. Reachable at chika@example.test');

INSERT INTO posts VALUES (
    'quoting-in-sql',
    'amelie',
    'Quoting in SQL: it''s harder than it looks',
    'A literal apostrophe: it''s here.
A second line follows a newline.
A tab:	and a backslash: \ and a quote: "',
    -- Keys are already sorted. MySQL normalises JSON key order on storage while
    -- Postgres preserves it, so only pre-sorted input reads back the same on
    -- both.
    '{"draft":false,"reading_minutes":7,"tags":["sql","quoting"]}',
    TRUE,
    14203
);

INSERT INTO posts VALUES (
    'unicode-everywhere',
    'chika',
    'Unicode everywhere 🌱',
    'Emoji 🌱, accents éàü, CJK 日本語, and an empty JSON object next.',
    '{}',
    TRUE,
    0
);

INSERT INTO posts VALUES (
    'no-metadata',
    'bjorn',
    'A post with no metadata',
    'The metadata column is null here, which is not the same as an empty object.',
    NULL,
    FALSE,
    3
);

INSERT INTO comments VALUES (1, 'quoting-in-sql', 'dana', 'Useful, thanks!');
INSERT INTO comments VALUES (2, 'quoting-in-sql', NULL, 'Anonymous: what about "double quotes"?');
INSERT INTO comments VALUES (3, 'unicode-everywhere', 'amelie', 'The 🌱 renders fine for me.');
INSERT INTO comments VALUES (4, 'no-metadata', 'bjorn', '');
