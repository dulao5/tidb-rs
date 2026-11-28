# What MySQL Connector/J 5.1 sends while it connects (issue #505): the server-variable read
# of `ConnectionImpl.loadServerVariables` (5.1 reads tx_isolation and the query cache), then
# the session settings. No data needed.
-- ab:template connector-j-5.1
/* mysql-connector-java-5.1.49 ( Revision: ad86f36e100e104cd926c6b81c8cab9565750116 ) */SELECT  @@session.auto_increment_increment AS auto_increment_increment, @@character_set_client AS character_set_client, @@character_set_connection AS character_set_connection, @@character_set_results AS character_set_results, @@character_set_server AS character_set_server, @@collation_server AS collation_server, @@collation_connection AS collation_connection, @@init_connect AS init_connect, @@interactive_timeout AS interactive_timeout, @@license AS license, @@lower_case_table_names AS lower_case_table_names, @@max_allowed_packet AS max_allowed_packet, @@net_buffer_length AS net_buffer_length, @@net_write_timeout AS net_write_timeout, @@query_cache_size AS query_cache_size, @@query_cache_type AS query_cache_type, @@sql_mode AS sql_mode, @@system_time_zone AS system_time_zone, @@time_zone AS time_zone, @@tx_isolation AS tx_isolation, @@wait_timeout AS wait_timeout;
SET NAMES utf8mb4;
SET character_set_results = NULL;
SET autocommit=1;
SELECT @@session.tx_read_only;
SELECT @@session.tx_isolation;
SET SQL_SELECT_LIMIT=DEFAULT;
SET net_write_timeout=600;
SET net_write_timeout=60;
