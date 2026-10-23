# Statements of the prototype workload: sysbench oltp_read_only, plus what MySQL clients and
# drivers send around a session. tests/prototype.rs requires our Restore() text to be
# byte-identical to TiDB's, recorded in prototype.jsonl by
#   tools/parser-port/go-baseline/restore.sh <tidb-checkout> crates/parser/tests/data/prototype.sql \
#     > crates/parser/tests/data/prototype.jsonl
# One statement (list) per line.

# sysbench oltp_read_only, prepared statements
SELECT c FROM sbtest1 WHERE id=?
SELECT c FROM sbtest1 WHERE id BETWEEN ? AND ?
SELECT SUM(k) FROM sbtest1 WHERE id BETWEEN ? AND ?
SELECT c FROM sbtest1 WHERE id BETWEEN ? AND ? ORDER BY c
SELECT DISTINCT c FROM sbtest1 WHERE id BETWEEN ? AND ? ORDER BY c
# sysbench oltp_read_only, --db-ps-mode=disable
SELECT c FROM sbtest1 WHERE id=4991
SELECT c FROM sbtest7 WHERE id BETWEEN 5018 AND 5117
SELECT SUM(k) FROM sbtest1 WHERE id BETWEEN 5018 AND 5117
SELECT c FROM sbtest3 WHERE id BETWEEN 5018 AND 5117 ORDER BY c
SELECT DISTINCT c FROM sbtest10 WHERE id BETWEEN 5018 AND 5117 ORDER BY c
BEGIN
COMMIT
START TRANSACTION
START TRANSACTION READ ONLY
START TRANSACTION WITH CONSISTENT SNAPSHOT
ROLLBACK
begin; select c from sbtest1 where id = 1; commit

# connection setup and session queries (mysql CLI, libmysqlclient, Connector/J, go-sql-driver)
SELECT @@version_comment LIMIT 1
select @@version_comment limit 1
SELECT DATABASE(), USER() LIMIT 1
select USER()
SELECT DATABASE()
SELECT VERSION()
SELECT CONNECTION_ID()
SELECT NOW()
SELECT CURRENT_TIMESTAMP(6)
SELECT 1
select 1 from dual
SELECT @@max_allowed_packet
SELECT @@tx_isolation
SELECT @@session.transaction_read_only
SELECT @@GLOBAL.version, @@session.autocommit
/* mysql-connector-java-8.0.30 */SELECT  @@session.auto_increment_increment AS auto_increment_increment, @@character_set_client AS character_set_client, @@character_set_connection AS character_set_connection, @@character_set_results AS character_set_results, @@character_set_server AS character_set_server, @@collation_server AS collation_server, @@collation_connection AS collation_connection, @@init_connect AS init_connect, @@interactive_timeout AS interactive_timeout, @@license AS license, @@lower_case_table_names AS lower_case_table_names, @@max_allowed_packet AS max_allowed_packet, @@net_write_timeout AS net_write_timeout, @@performance_schema AS performance_schema, @@query_cache_size AS query_cache_size, @@query_cache_type AS query_cache_type, @@sql_mode AS sql_mode, @@system_time_zone AS system_time_zone, @@time_zone AS time_zone, @@transaction_isolation AS transaction_isolation, @@wait_timeout AS wait_timeout
USE sbtest
use `sbtest`
SET NAMES utf8mb4
SET NAMES utf8mb4 COLLATE utf8mb4_general_ci
SET NAMES 'utf8' COLLATE 'utf8_general_ci'
SET NAMES latin1
SET NAMES DEFAULT
SET CHARACTER SET utf8mb4
SET CHARSET DEFAULT
SET autocommit=1
SET autocommit=0
SET AUTOCOMMIT = 1
SET @@autocommit = 1
SET SESSION autocommit = 0
SET GLOBAL autocommit = ON
SET sql_mode='STRICT_TRANS_TABLES'
SET @@SESSION.sql_mode = CONCAT(CONCAT(@@sql_mode, ',STRICT_ALL_TABLES'), ',NO_AUTO_VALUE_ON_ZERO')
SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED
SET TRANSACTION ISOLATION LEVEL REPEATABLE READ
SET GLOBAL TRANSACTION ISOLATION LEVEL SERIALIZABLE, READ WRITE
SET SESSION TRANSACTION READ ONLY
SET @@session.transaction_isolation = 'REPEATABLE-READ'
SET character_set_results = NULL
SET net_write_timeout=600
SET SQL_SELECT_LIMIT=DEFAULT
SET time_zone = '+00:00'
SET @a = 1, @@session.max_execution_time = 1000
SET @@tidb_isolation_read_engines = 'tikv'
SHOW VARIABLES LIKE 'sql_mode'
SHOW VARIABLES LIKE 'lower_case_%'
SHOW SESSION VARIABLES LIKE 'version%'
SHOW GLOBAL VARIABLES WHERE Variable_name = 'version'
SHOW VARIABLES
SHOW WARNINGS
SHOW COUNT(*) WARNINGS
SHOW ERRORS
SHOW DATABASES
SHOW TABLES
SHOW FULL TABLES FROM sbtest LIKE 'sbtest%'
SHOW CREATE TABLE sbtest1
SHOW COLUMNS FROM sbtest1
SHOW FULL COLUMNS FROM sbtest1 FROM sbtest
SHOW INDEX FROM sbtest1
SHOW TABLE STATUS LIKE 'sbtest1'
SHOW GLOBAL STATUS LIKE 'Threads%'
SHOW COLLATION
SHOW CHARACTER SET
SHOW ENGINES
SHOW PROCESSLIST
SHOW FULL PROCESSLIST
