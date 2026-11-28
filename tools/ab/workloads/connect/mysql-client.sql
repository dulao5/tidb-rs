# What the `mysql` command-line client sends while it connects (issue #505), and the session
# settings scripts run before a load or a migration. No data needed.
-- ab:template mysql-client
select @@version_comment limit 1;
SELECT DATABASE();
SET NAMES utf8mb4;
SET foreign_key_checks = 0;
SET unique_checks = 0;
SET sql_safe_updates = 0;
SET innodb_lock_wait_timeout = 100;
SELECT @@foreign_key_checks, @@unique_checks, @@sql_safe_updates, @@innodb_lock_wait_timeout;
SET foreign_key_checks = 1;
SET unique_checks = 1;
SET innodb_lock_wait_timeout = DEFAULT;
