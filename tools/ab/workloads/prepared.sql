# Prepared statements with more than one parameter, against sbtest.sbtest1 as loaded by
# scripts/sysbench-prepare.sh (ids 1..100000, 8 regions: a boundary at 12501).
#
# Why (#353): every `?` once got parameter order 0 (#352), so a statement with several
# parameters read the first value for all of them. Nothing in the comparison caught it:
# sysbench's prepared point get has one parameter, and its range queries fell back at the
# Projection. Each statement below runs once per `-- ab:execute` line: via the text protocol
# with the values as literals, and as EXECUTEs of one PREPARE (the first EXECUTE plans, the
# later ones execute the same statement again, so the plan cache can hit). Most EXECUTEs bind
# distinct values, so a parameter read from the wrong position gives other rows; a few repeat
# one value on purpose (a single-row range, duplicates in IN).
# Every statement binds the same types on every EXECUTE; a later EXECUTE with another type is
# in prepared_type_change.sql.

-- ab:template prepared_range
-- ab:execute 10, 20
-- ab:execute 500, 503
-- ab:execute 20, 10
-- ab:execute 99995, 100005
SELECT c FROM sbtest1 WHERE id BETWEEN ? AND ?;
-- ab:execute 1, 5
-- ab:execute 12490, 12510
-- ab:execute 7, 7
-- ab:execute 8, 7
SELECT id, k FROM sbtest1 WHERE id >= ? AND id <= ?;
-- ab:execute 100, 105
-- ab:execute 0, 3
SELECT id, c FROM sbtest1 WHERE id > ? AND id < ?;
-- ab:execute 1, 2, 3
-- ab:execute 42, 7, 100001
-- ab:execute 5, 5, 5
-- ab:execute 3, 2, 1
SELECT id, k FROM sbtest1 WHERE id IN (?, ?, ?);
-- ab:execute 1, 50, 1, 10
-- ab:execute 1, 50, 40, 60
SELECT c FROM sbtest1 WHERE id IN (?, ?) AND id BETWEEN ? AND ?;

# Only the primary key column: no Projection above the range scan, so on the new stack these
# reach the table reader itself.
-- ab:template prepared_pk_only
-- ab:execute 1, 10
-- ab:execute 12495, 12505
-- ab:execute 100, 90
-- ab:execute -5, 3
-- ab:execute 99998, 100003
SELECT id FROM sbtest1 WHERE id BETWEEN ? AND ?;
-- ab:execute 1000, 1010
-- ab:execute 50, 51
SELECT id FROM sbtest1 WHERE id >= ? AND id < ?;
-- ab:execute 10, 20, 30
-- ab:execute 99999, 100000, 100001
SELECT id FROM sbtest1 WHERE id IN (?, ?, ?);
-- ab:execute 1, 10
-- ab:execute 20, 25
SELECT id FROM sbtest1 WHERE id BETWEEN ? AND ? ORDER BY id DESC;

# Two columns. sysbench fills k at random, so `k = ? AND id = ?` mostly returns no row; the
# other two forms return the row when each value is read from its own position.
-- ab:template prepared_two_columns
-- ab:execute -1, 7
-- ab:execute 7, 7
SELECT id, k FROM sbtest1 WHERE k = ? AND id = ?;
-- ab:execute 0, 7
-- ab:execute 0, 42
-- ab:execute 2147483647, 7
SELECT id, k FROM sbtest1 WHERE k >= ? AND id = ?;
-- ab:execute 7, 2147483647
-- ab:execute 8, 0
SELECT id FROM sbtest1 WHERE id = ? AND k < ?;

# Parameters outside the range condition: in the select list.
-- ab:template prepared_select_list
-- ab:execute 7, 1
-- ab:execute 8, 2
-- ab:execute 100, 100001
SELECT ?, id FROM sbtest1 WHERE id = ?;
-- ab:execute 'a', 2, 3
-- ab:execute 'b', 5, 6
SELECT ?, ?, c FROM sbtest1 WHERE id = ?;
-- ab:execute 100, 5
-- ab:execute 1000, 6
SELECT id + ? FROM sbtest1 WHERE id = ?;
-- ab:execute 0, 1, 3
-- ab:execute 9, 10, 12
SELECT id, ? FROM sbtest1 WHERE id BETWEEN ? AND ?;

# Parameters outside the range condition: in LIMIT (offset and count in both orders).
-- ab:template prepared_limit
-- ab:execute 5, 1
-- ab:execute 6, 0
-- ab:execute 7, 10
SELECT c FROM sbtest1 WHERE id = ? LIMIT ?;
-- ab:execute 1, 100, 5
-- ab:execute 200, 300, 3
-- ab:execute 1, 3, 10
SELECT id FROM sbtest1 WHERE id BETWEEN ? AND ? ORDER BY id LIMIT ?;
-- ab:execute 1, 100, 5, 3
-- ab:execute 50, 60, 0, 2
SELECT id FROM sbtest1 WHERE id BETWEEN ? AND ? ORDER BY id LIMIT ?, ?;
-- ab:execute 1, 100, 3, 5
-- ab:execute 50, 60, 2, 0
SELECT id FROM sbtest1 WHERE id BETWEEN ? AND ? ORDER BY id LIMIT ? OFFSET ?;
-- ab:execute 99990, 3u
-- ab:execute 1, 2u
SELECT id FROM sbtest1 WHERE id >= ? ORDER BY id LIMIT ?;
