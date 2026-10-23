# Example file workload with a setup section. Setup statements run on A only (writes allowed);
# SET/USE in setup also run on B. Test statements that are not read-only are skipped.
-- ab:setup
DROP DATABASE IF EXISTS ab_sample;
CREATE DATABASE ab_sample;
USE ab_sample;
CREATE TABLE t (id BIGINT PRIMARY KEY, a INT, b VARCHAR(20), c DECIMAL(10,2), d DATETIME(3), KEY ka (a));
INSERT INTO t VALUES (1, 10, 'x', 1.50, '2026-01-02 03:04:05.678'), (2, NULL, 'y', -2.25, NULL),
    (3, 10, '', 0, '1999-12-31 23:59:59.999'), (4, -7, 'Zz', 99999999.99, '2000-01-01 00:00:00');
-- ab:test
SELECT * FROM t ORDER BY id;
SELECT id, a FROM t WHERE a = 10;
--sorted_result
SELECT b FROM t WHERE id > 1;
SELECT COUNT(*), SUM(a), SUM(c), MIN(d), MAX(b) FROM t;
SELECT a, COUNT(*) FROM t GROUP BY a ORDER BY a;
SELECT * FROM t WHERE b = 'none';
SELECT * FROM no_such_table;
SELECT * FROM t FOR UPDATE;
UPDATE t SET a = 1;
