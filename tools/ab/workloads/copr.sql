# Coprocessor reads over several regions (D07.2): table copr.t has 20000 rows split into 8
# regions at handle multiples of 2500, copr.acct 2000 rows in 4 regions, copr.e is empty.
# Prepare with the statements in specs/D07.2-copr/spec.md §7. Paging is on (128 → 50000 rows
# per page), so a region of 2500 rows takes several pages.
SELECT COUNT(*) FROM copr.t;
SELECT SUM(k), MIN(id), MAX(id) FROM copr.t;
SELECT COUNT(*), SUM(k) FROM copr.t WHERE id >= 2400 AND id < 2600;
SELECT COUNT(*) FROM copr.t WHERE id > 2499 AND id <= 12501;
SELECT k, COUNT(*) FROM copr.t GROUP BY k ORDER BY k;
SELECT id, k, c FROM copr.t ORDER BY id LIMIT 10;
SELECT id, k, c FROM copr.t ORDER BY id DESC LIMIT 10;
SELECT id, c FROM copr.t WHERE id >= 2490 AND id < 2510 ORDER BY id;
SELECT id, c FROM copr.t WHERE id >= 2490 AND id < 2510 ORDER BY id DESC;
SELECT id FROM copr.t WHERE id >= 4990 ORDER BY id LIMIT 30;
SELECT id FROM copr.t WHERE id <= 5010 ORDER BY id DESC LIMIT 30;
SELECT id, k FROM copr.t WHERE id BETWEEN 7400 AND 7600 ORDER BY id;
SELECT id FROM copr.t WHERE id > 19990;
SELECT id FROM copr.t WHERE id > 20000;
SELECT id FROM copr.t WHERE id < 1;
SELECT COUNT(*) FROM copr.t WHERE id BETWEEN 1 AND 129;
SELECT COUNT(*) FROM copr.t WHERE id BETWEEN 1 AND 128;
SELECT id, c FROM copr.t WHERE id BETWEEN 100 AND 400 ORDER BY id LIMIT 200;
SELECT id FROM copr.t ORDER BY id LIMIT 1;
SELECT id FROM copr.t ORDER BY id DESC LIMIT 1;
SELECT id FROM copr.t LIMIT 5;
SELECT COUNT(*) FROM copr.e;
SELECT * FROM copr.e;
SELECT SUM(bal), COUNT(*) FROM copr.acct;
SELECT id, bal FROM copr.acct WHERE id >= 495 AND id < 505 ORDER BY id;
SELECT id, bal FROM copr.acct ORDER BY id DESC LIMIT 5;
