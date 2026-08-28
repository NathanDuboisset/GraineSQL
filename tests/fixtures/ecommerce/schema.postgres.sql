-- Money is integer cents, not numeric: SQLite has no exact decimal, so a shared
-- fixture cannot use one. Decimals are covered by `analytics`.
CREATE TABLE regions (
    code varchar(8) PRIMARY KEY,
    name text NOT NULL
);

CREATE TABLE countries (
    code        char(2) PRIMARY KEY,
    region_code varchar(8) NOT NULL REFERENCES regions(code),
    name        text NOT NULL
);

CREATE TABLE customers (
    id           text PRIMARY KEY,
    country_code char(2) NOT NULL REFERENCES countries(code),
    email        varchar(160) NOT NULL UNIQUE,
    display_name text
);

CREATE TABLE products (
    sku         varchar(32) PRIMARY KEY,
    name        text NOT NULL,
    price_cents bigint NOT NULL
);

CREATE TABLE orders (
    id          bigint PRIMARY KEY,
    customer_id text NOT NULL REFERENCES customers(id),
    placed_on   date NOT NULL,
    total_cents bigint NOT NULL
);

-- A composite primary key that is also the foreign key into two parents.
CREATE TABLE order_items (
    order_id         bigint      NOT NULL REFERENCES orders(id),
    product_sku      varchar(32) NOT NULL REFERENCES products(sku),
    quantity         integer     NOT NULL,
    unit_price_cents bigint      NOT NULL,
    PRIMARY KEY (order_id, product_sku)
);
