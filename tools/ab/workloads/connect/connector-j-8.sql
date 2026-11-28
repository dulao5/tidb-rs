# What MySQL Connector/J 8.x sends while it connects and sets up a connection (issue #505):
# the server-variable read of `ConnectionImpl.loadServerVariables`, then the session
# settings. Run with `ab file tools/ab/workloads/connect/connector-j-8.sql`; no data needed.
-- ab:template connector-j-8
/* mysql-connector-java-8.0.33 (Revision: 7d6b0800528b6b25c68b52dc10d6c1c8429c100c) */SELECT  @@session.auto_increment_increment AS auto_increment_increment, @@character_set_client AS character_set_client, @@character_set_connection AS character_set_connection, @@character_set_results AS character_set_results, @@character_set_server AS character_set_server, @@collation_server AS collation_server, @@collation_connection AS collation_connection, @@init_connect AS init_connect, @@interactive_timeout AS interactive_timeout, @@license AS license, @@lower_case_table_names AS lower_case_table_names, @@max_allowed_packet AS max_allowed_packet, @@net_write_timeout AS net_write_timeout, @@performance_schema AS performance_schema, @@sql_mode AS sql_mode, @@system_time_zone AS system_time_zone, @@time_zone AS time_zone, @@transaction_isolation AS transaction_isolation, @@wait_timeout AS wait_timeout;
SET NAMES utf8mb4;
SET character_set_results = NULL;
SET autocommit=1;
SELECT @@session.transaction_read_only;
SELECT @@session.transaction_isolation;
SET SQL_SELECT_LIMIT=DEFAULT;
SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ;
SELECT @@session.auto_increment_increment;
SELECT @@character_set_client, @@character_set_connection, @@character_set_results;
