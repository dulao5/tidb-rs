# What MySQL Connector/Python sends while it connects (issue #505): the charset and
# collation, then its autocommit default (OFF). The read-only node answers autocommit=0 with
# its not-in-a-transaction 1235 (an `unsupported`), so the workload sets it back. No data
# needed.
-- ab:template connector-python
SET NAMES 'utf8mb4' COLLATE 'utf8mb4_general_ci';
SELECT @@character_set_client, @@character_set_connection, @@collation_connection;
SET @@session.time_zone = '+00:00';
SET @@session.sql_mode = 'STRICT_TRANS_TABLES,NO_ENGINE_SUBSTITUTION';
SELECT @@session.time_zone, @@session.sql_mode;
SET @@session.autocommit = OFF;
SET @@session.autocommit = ON;
SELECT @@session.autocommit;
SET @@session.sql_mode = DEFAULT;
SET @@session.time_zone = 'SYSTEM';
