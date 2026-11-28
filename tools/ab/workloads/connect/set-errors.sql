# The errors of connect-time SETs (issue #505): both servers must fail with the same code,
# SQLSTATE and message (`both_error`). The two last are not supported by the read-only node
# yet (`unsupported`): a SET value that is not a literal waits for the expression rewriter's
# `rewriteSystemVariable`, a user variable for `getvar`/`setvar`. No data needed.
-- ab:template set-errors
SET max_execution_time = 1.5;
SET max_execution_time = 99999999999999999999;
SET max_execution_time = 'abc';
SET NAMES nosuch;
SET CHARACTER SET nosuch;
SET NAMES utf8mb4 COLLATE nosuch;
SET NAMES utf8mb4 COLLATE latin1_bin;
SET no_such_variable = 1;
SET autocommit = 2;
SET group_concat_max_len = 'abc';
SET sql_mode = CONCAT(@@sql_mode, ',STRICT_TRANS_TABLES');
SET @x = 1;
