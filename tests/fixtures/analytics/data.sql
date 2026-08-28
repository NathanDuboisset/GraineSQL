INSERT INTO accounts VALUES (1, 'pro');
INSERT INTO accounts VALUES (2, 'free');

INSERT INTO samples VALUES (
    1, 1,
    '12345678901234567890.1234567890',
    32767, 9223372036854775807, 3.141592653589793,
    NULL,
    '2024-11-03 01:30:00-04', '2024-01-01 00:00:00', '2024-02-29',
    'the DST repeat hour, resolved by its offset'
);
INSERT INTO samples VALUES (
    2, 1,
    '-0.0000000001',
    -32768, -9223372036854775808, -0.0,
    NULL,
    '2024-11-03 01:30:00-05', '2024-12-31 23:59:59', '2025-01-01',
    'the same wall clock, one hour later'
);
INSERT INTO samples VALUES (
    3, 2,
    '0.0000000000',
    0, 0, 0.1,
    NULL,
    NULL, NULL, NULL,
    NULL
);
