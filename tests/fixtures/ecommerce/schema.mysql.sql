-- Money is integer cents, not DECIMAL: SQLite has no exact decimal, so a shared
-- fixture cannot use one. Decimals are covered by `analytics`.
CREATE TABLE regions (
    code VARCHAR(8) PRIMARY KEY,
    name TEXT NOT NULL
);

CREATE TABLE countries (
    code        CHAR(2) PRIMARY KEY,
    region_code VARCHAR(8) NOT NULL,
    name        TEXT NOT NULL,
    FOREIGN KEY (region_code) REFERENCES regions(code)
);

CREATE TABLE customers (
    id           VARCHAR(64) PRIMARY KEY,
    country_code CHAR(2) NOT NULL,
    email        VARCHAR(160) NOT NULL UNIQUE,
    display_name TEXT,
    FOREIGN KEY (country_code) REFERENCES countries(code)
);

CREATE TABLE products (
    sku         VARCHAR(32) PRIMARY KEY,
    name        TEXT NOT NULL,
    price_cents BIGINT NOT NULL
);

CREATE TABLE orders (
    id          BIGINT PRIMARY KEY,
    customer_id VARCHAR(64) NOT NULL,
    placed_on   DATE NOT NULL,
    total_cents BIGINT NOT NULL,
    FOREIGN KEY (customer_id) REFERENCES customers(id)
);

-- A composite primary key that is also the foreign key into two parents.
CREATE TABLE order_items (
    order_id         BIGINT      NOT NULL,
    product_sku      VARCHAR(32) NOT NULL,
    quantity         INT         NOT NULL,
    unit_price_cents BIGINT      NOT NULL,
    PRIMARY KEY (order_id, product_sku),
    FOREIGN KEY (order_id) REFERENCES orders(id),
    FOREIGN KEY (product_sku) REFERENCES products(sku)
);
