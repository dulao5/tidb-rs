use super::*;

fn facts() -> ServerFacts {
    ServerFacts {
        version: "v8.5.8-tidb-rs-test".to_owned(),
        git_hash: "0123456789abcdef".to_owned(),
        maxprocs: 12,
    }
}

/// Every family tidb-rs records itself is exposed with Go's type, label names and buckets
/// once each recording path ran.
#[test]
fn exposition_matches_go_definitions() {
    init(&facts());
    command_started(Duration::from_millis(3));
    packet_in(10);
    packet_out(20);
    statement(StmtType::Select);
    parse_duration(Duration::from_micros(30));
    compile_duration(Duration::from_micros(40));
    execute_duration(Duration::from_micros(500));
    transaction(Duration::from_millis(1), true);
    slow_query(Duration::from_millis(400));
    plan_cache_miss_prepared();
    expensive(ExpensiveExec::Sort);
    distsql_query(Duration::from_millis(2), true);
    multi_query(2);
    command_finished(0x17, StmtType::Select, Duration::from_millis(1), None);
    command_finished(
        0x03,
        StmtType::General,
        Duration::from_millis(1),
        Some("planner:1146"),
    );
    let text = gather_text();
    let problems = golden::check(&text, golden::TIDB);
    assert!(problems.is_empty(), "{problems:#?}\n{text}");
    // Label values Go uses for the same events.
    for want in [
        r#"tidb_server_query_total{resource_group="default",result="OK",type="StmtExecute"} 1"#,
        r#"tidb_server_query_total{resource_group="default",result="Error",type="Query"} 1"#,
        r#"tidb_server_execute_error_total{db="",resource_group="default",type="planner:1146"} 1"#,
        r#"tidb_executor_statement_total{db="",resource_group="default",type="Select"} 1"#,
        r#"tidb_session_transaction_duration_seconds_count{scope="general",txn_mode="optimistic",type="commit"} 1"#,
        r#"tidb_distsql_handle_query_duration_seconds_count{copr_type="paging",sql_type="general",type="dag"} 1"#,
        r#"tidb_server_conn_idle_duration_seconds_count{in_txn="0"} 1"#,
        r#"tidb_server_get_token_duration_seconds_bucket{le="1"} 1"#,
        r#"tidb_server_info{hash="0123456789abcdef",version="v8.5.8-tidb-rs-test"}"#,
        "tidb_server_maxprocs 12",
        r#"tidb_server_connections{resource_group="default"} 0"#,
        r#"tidb_server_plan_cache_miss_total{type="prepared"} 1"#,
        r#"tidb_executor_expensive_total{type="SortExec"} 1"#,
    ] {
        assert!(text.contains(want), "missing {want}\n{text}");
    }
}

#[test]
fn command_labels_follow_go() {
    assert_eq!(command_label(0x03), "Query");
    assert_eq!(command_label(0x17), "StmtExecute");
    assert_eq!(command_label(0x01), "Quit");
    // Go: strconv.Itoa for unnamed commands.
    assert_eq!(command_label(0x09), "9");
    assert_eq!(command_label(0x1f), "31");
}
