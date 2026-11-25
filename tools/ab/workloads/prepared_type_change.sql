# A later EXECUTE that binds a parameter of another signedness or type than the first EXECUTE
# of the same prepared statement, with the parameter selected as a column (#367), against
# sbtest.sbtest1 as loaded by scripts/sysbench-prepare.sh.
#
# Go v8.5.8 keeps the column definitions of a prepared statement's first EXECUTE and sends
# them again on every later EXECUTE (`pkg/server/internal/resultset/resultset.go:108-131`):
# after a signed first EXECUTE, the unsigned 18446744073709551615 is described as a signed
# BIGINT and a client reads -1; a string after an int is described as BIGINT. tidb-rs
# describes each EXECUTE's own columns. Until the owner rules on #367 these are mismatches.
# They are grouped under the template prepared_type_change (the name of this file), so that a
# ruling can match them by its unit. Each block of `-- ab:execute` lines prepares its statement
# afresh, so its first line is the statement's first EXECUTE.

-- ab:template prepared_type_change

# Over a point get: signed then unsigned, and unsigned then signed.
-- ab:execute 5, 1
-- ab:execute 18446744073709551615u, 1
SELECT ?, c FROM sbtest1 WHERE id = ?;
-- ab:execute 18446744073709551615u, 1
-- ab:execute 5, 1
SELECT ?, c FROM sbtest1 WHERE id = ?;

# Under a TopN over a point get.
-- ab:execute 5, 1
-- ab:execute 18446744073709551615u, 1
SELECT ?, c FROM sbtest1 WHERE id = ? ORDER BY c LIMIT 1;
-- ab:execute 18446744073709551615u, 1
-- ab:execute 5, 1
SELECT ?, c FROM sbtest1 WHERE id = ? ORDER BY c LIMIT 1;

# Over the dual table, never in the plan cache: the cause is the column definitions kept by
# the connection, not the plan cache.
-- ab:execute 5
-- ab:execute 18446744073709551615u
SELECT ? FROM DUAL ORDER BY RAND();
-- ab:execute 18446744073709551615u
-- ab:execute 5
SELECT ? FROM DUAL ORDER BY RAND();
# The same value, only the unsigned flag differs.
-- ab:execute 5
-- ab:execute 5u
SELECT ? FROM DUAL ORDER BY RAND();
# A string after an int, and an int after a string.
-- ab:execute 7
-- ab:execute 'x'
SELECT ? FROM DUAL ORDER BY RAND();
-- ab:execute 'x'
-- ab:execute 7
SELECT ? FROM DUAL ORDER BY RAND();
