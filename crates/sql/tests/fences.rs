//! Fences of the read-only node that SQL reaches today (`specs/exceptions/fences.md`): each
//! guard answers with a MySQL error code (1235) and TiDB's "not supported" text, never with a
//! panic.

use tidb_sql::parse_and_lower;

/// The error `sql` is fenced with; fails on success and on a panic.
fn fence(sql: &str) -> (u16, String) {
    match std::panic::catch_unwind(|| parse_and_lower(sql)) {
        Ok(Err(e)) => (e.code, e.message),
        Ok(Ok(_)) => panic!("{sql}: accepted"),
        Err(_) => panic!("{sql}: panicked"),
    }
}

fn not_supported(what: &str) -> String {
    format!("This version of TiDB doesn't yet support '{what}'")
}

/// Writes, DDL and the other statements Go TiDB runs for the cluster (not ported, X): the
/// read-only node answers `<node> statements (read-only prototype)`.
#[test]
fn writes_and_ddl_answer_1235() {
    for (sql, node) in [
        ("INSERT INTO t VALUES (1)", "InsertIntoStmt#0"),
        ("REPLACE INTO t VALUES (1)", "ReplaceIntoStmt#0"),
        ("UPDATE t SET a = 1", "UpdateStmtNoWith#0"),
        ("DELETE FROM t", "DeleteWithoutUsingStmt#0"),
        ("CREATE TABLE t (a INT)", "CreateTableStmt#0"),
        ("DROP TABLE t", "DropTableStmt#0"),
        ("ALTER TABLE t ADD COLUMN b INT", "AlterTableStmt#0"),
        ("CREATE INDEX i ON t (a)", "CreateIndexStmt#0"),
        ("TRUNCATE TABLE t", "TruncateTableStmt#0"),
        ("CREATE DATABASE d", "CreateDatabaseStmt#0"),
        ("RENAME TABLE t TO u", "RenameTableStmt#0"),
        ("ANALYZE TABLE t", "AnalyzeTableStmt#0"),
        ("LOAD DATA INFILE 'f' INTO TABLE t", "LoadDataStmt#0"),
        ("IMPORT INTO t FROM 'f'", "ImportIntoStmt#0"),
        ("BACKUP DATABASE * TO 'local:///tmp'", "BRIEStmt#0"),
        (
            "SPLIT TABLE t BETWEEN (0) AND (10) REGIONS 2",
            "SplitRegionStmt#0",
        ),
        ("ADMIN CHECK TABLE t", "AdminStmt#4"),
        ("GRANT SELECT ON *.* TO u", "GrantStmt#0"),
        ("CREATE USER u", "CreateUserStmt#0"),
        ("LOCK TABLES t READ", "LockTablesStmt#0"),
        ("SAVEPOINT s", "SavepointStmt#0"),
    ] {
        assert_eq!(
            fence(sql),
            (
                1235,
                not_supported(&format!("{node} statements (read-only prototype)"))
            ),
            "{sql}"
        );
    }
}

/// Savepoints are part of the write path (X): ROLLBACK TO SAVEPOINT answers 1235.
#[test]
fn rollback_to_savepoint_answers_1235() {
    assert_eq!(
        fence("ROLLBACK TO SAVEPOINT s"),
        (1235, not_supported("ROLLBACK TO SAVEPOINT"))
    );
}
