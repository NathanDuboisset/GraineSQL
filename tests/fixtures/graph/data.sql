INSERT INTO employees VALUES (1, NULL, 'Boss');
INSERT INTO employees VALUES (2, 1, 'Manager');
INSERT INTO employees VALUES (3, 2, 'Worker');
INSERT INTO employees VALUES (4, 2, 'Other worker');

INSERT INTO top VALUES (1, 'root');
INSERT INTO t_left VALUES (10, 1);
INSERT INTO t_right VALUES (20, 1);
INSERT INTO bottom VALUES (100, 10, 20);

INSERT INTO teams VALUES (1, NULL, 'Platform');
INSERT INTO members VALUES (11, 1, 'Ada');
INSERT INTO members VALUES (12, 1, 'Grace');
UPDATE teams SET lead_id = 11 WHERE id = 1;
