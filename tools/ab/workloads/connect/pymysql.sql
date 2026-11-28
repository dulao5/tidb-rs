# What PyMySQL 1.x sends while it connects with its default settings (issue #547): the
# charset, then `SET AUTOCOMMIT = 0` (its default `autocommit=False`; it sends it because the
# server's OK packet shows autocommit on). Then a read-only transaction as an application runs
# it: its `commit()` and `rollback()` send `COMMIT` / `ROLLBACK`, and `begin()` sends `BEGIN`.
# No data needed.
-- ab:template pymysql
SET NAMES utf8mb4;
SET AUTOCOMMIT = 0;
SELECT @@autocommit, @@session.autocommit;
SELECT 1;
COMMIT;
ROLLBACK;
BEGIN;
SELECT 1;
COMMIT;
SET AUTOCOMMIT = 1;
SELECT @@autocommit;
