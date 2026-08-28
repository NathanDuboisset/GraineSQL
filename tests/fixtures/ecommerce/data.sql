INSERT INTO regions VALUES ('emea', 'Europe, Middle East and Africa');
INSERT INTO regions VALUES ('apac', 'Asia Pacific');
INSERT INTO regions VALUES ('amer', 'Americas');

INSERT INTO countries VALUES ('FR', 'emea', 'France');
INSERT INTO countries VALUES ('DE', 'emea', 'Deutschland');
INSERT INTO countries VALUES ('JP', 'apac', 'Japan');
INSERT INTO countries VALUES ('US', 'amer', 'United States');

INSERT INTO customers VALUES ('cus_0001', 'FR', 'amelie@example.test', 'Amelie Dupont');
INSERT INTO customers VALUES ('cus_0002', 'DE', 'bjorn@example.test', NULL);
INSERT INTO customers VALUES ('cus_0003', 'JP', 'chika@example.test', 'Chika Nakamura');
INSERT INTO customers VALUES ('cus_0004', 'US', 'dana@example.test', 'Dana O''Brien');

INSERT INTO products VALUES ('SKU-KEYBOARD', 'Mechanical keyboard', 12900);
INSERT INTO products VALUES ('SKU-MOUSE', 'Wireless mouse', 4550);
INSERT INTO products VALUES ('SKU-MONITOR', '27-inch monitor', 34999);
INSERT INTO products VALUES ('SKU-CABLE', 'USB-C cable, 2m', 1199);

INSERT INTO orders VALUES (1001, 'cus_0001', '2024-02-29', 17450);
INSERT INTO orders VALUES (1002, 'cus_0003', '2024-07-04', 34999);
INSERT INTO orders VALUES (1003, 'cus_0004', '2024-11-03', 6948);
INSERT INTO orders VALUES (1004, 'cus_0001', '2025-01-15', 1199);

INSERT INTO order_items VALUES (1001, 'SKU-KEYBOARD', 1, 12900);
INSERT INTO order_items VALUES (1001, 'SKU-MOUSE', 1, 4550);
INSERT INTO order_items VALUES (1002, 'SKU-MONITOR', 1, 34999);
INSERT INTO order_items VALUES (1003, 'SKU-CABLE', 3, 1199);
INSERT INTO order_items VALUES (1003, 'SKU-MOUSE', 1, 3351);
INSERT INTO order_items VALUES (1004, 'SKU-CABLE', 1, 1199);
