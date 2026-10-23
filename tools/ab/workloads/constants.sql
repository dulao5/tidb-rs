# Constant SELECTs and session statements: the first things a client sends, answerable without
# reading data. Values that legitimately differ between two servers (version, connection id,
# user host) are left out.
SELECT 1;
SELECT -42;
SELECT 1, 2, 3;
SELECT 9223372036854775807;
SELECT 'abc';
SELECT '';
SELECT 1 LIMIT 1;
SELECT 1 + 1;
SELECT 1.5;
SELECT NULL;
SELECT 'a' AS x;
SELECT @@max_allowed_packet;
SELECT @@session.wait_timeout;
SELECT @@autocommit;
SELECT @@transaction_isolation;
SELECT @@lower_case_table_names;
SELECT @@character_set_client, @@collation_connection;
SELECT DATABASE();
SET NAMES utf8mb4;
SET autocommit = 1;
USE sbtest;
SELECT DATABASE();
SELECT SCHEMA();
SELECT CURRENT_USER();
SELECT @@no_such_variable;
SHOW DATABASES;
