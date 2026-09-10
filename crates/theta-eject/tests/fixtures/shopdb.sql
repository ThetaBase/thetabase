-- The source schema `live_migration.rs` migrates.
--
-- Deliberately built out of the cases that actually go wrong rather than the
-- ones that are easy to write: a composite primary key, a table with no key at
-- all, `numeric` (which cannot survive as a float), arrays containing a comma
-- and the literal text NULL, `jsonb`, `bytea`, a UNIQUE that stops being
-- enforced, a `varchar` bound that stops being enforced, column defaults that
-- do not travel, and timestamps before the epoch and past 2038.
--
--   docker run -d --name thetabase-eject-pg -p 55432:5432 \
--     -e POSTGRES_PASSWORD=thetabase -e POSTGRES_USER=thetabase \
--     -e POSTGRES_DB=shopdb postgres:16
--   psql "postgresql://thetabase:thetabase@127.0.0.1:55432/shopdb" -f this-file

DROP TABLE IF EXISTS customers, orders, audit_log;

CREATE TABLE customers (
  id           bigint PRIMARY KEY,
  email        varchar(120) NOT NULL UNIQUE,
  display_name text,
  signup_at    timestamptz NOT NULL DEFAULT now(),
  is_active    boolean NOT NULL DEFAULT true,
  balance      numeric(12,2) NOT NULL DEFAULT 0,
  view_count   bigint NOT NULL DEFAULT 0,
  tags         text[],
  prefs        jsonb,
  avatar       bytea
);
CREATE INDEX customers_display_name_idx ON customers (display_name);

-- A composite key, so a migration that renders keys carelessly collides rows.
CREATE TABLE orders (
  customer_id bigint NOT NULL,
  seq         int NOT NULL,
  total       numeric(10,2) NOT NULL,
  placed_on   date,
  status      text NOT NULL,
  PRIMARY KEY (customer_id, seq)
);

-- No primary key: ThetaBase addresses rows by one, so this must block rather
-- than have a key invented for it.
CREATE TABLE audit_log (
  happened_at timestamptz NOT NULL,
  note        text
);

INSERT INTO customers (id, email, display_name, signup_at, is_active, balance, view_count, tags, prefs, avatar) VALUES
 (1, 'ada@example.com',      'Ada', '2024-01-31 12:34:56.789+00', true,  1234.56, 42, ARRAY['vip','early'], '{"theme":"dark","n":3}', '\xdeadbeef'),
 (2, 'grace@example.com',    NULL,  '1969-12-31 00:00:00+00',     false,   -0.01,  0, ARRAY[]::text[],      '{}',                     NULL),
 (3, 'kay,quote@example.com','K"y', '2030-06-15 23:59:59.999+00', true,        0,  7, ARRAY['a,b','NULL'],  'null',                   '\x');

INSERT INTO orders (customer_id, seq, total, placed_on, status) VALUES
 (1, 1, 10.00, '2024-02-01', 'shipped'),
 (1, 2, 99.99, NULL,         'pending'),
 (3, 1,  0.00, '2024-03-03', 'cancelled');

INSERT INTO audit_log (happened_at, note) VALUES (now(), 'no primary key here');
