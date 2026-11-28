# What Go-MySQL-Driver sends while it connects (issue #505): `@@max_allowed_packet` when the
# DSN does not set it, the DSN's charset, then its system-variable parameters (here
# `sql_mode`, `time_zone`, `group_concat_max_len`, as ORMs set them). No data needed.
-- ab:template go-mysql-driver
SELECT @@max_allowed_packet;
SET NAMES utf8mb4;
SET NAMES utf8mb4 COLLATE utf8mb4_bin;
SET sql_mode='STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO';
SET time_zone='+00:00';
SET group_concat_max_len=1048576;
SET max_execution_time=30000;
SELECT @@sql_mode, @@time_zone, @@group_concat_max_len, @@max_execution_time;
SET sql_mode=DEFAULT;
SET time_zone=DEFAULT;
