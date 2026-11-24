//! Unit tests of the access objects (`access_object.go`); the operators' access objects are
//! compared with Go v8.5.8 by the govec domain `postprocess`.

use super::*;

fn dyn_obj(table: &str, all: bool, parts: &[&str], err: &str) -> DynamicPartitionAccessObject {
    DynamicPartitionAccessObject {
        database: "test".to_owned(),
        table: table.to_owned(),
        all_partitions: all,
        partitions: parts.iter().map(|s| s.to_string()).collect(),
        err: err.to_owned(),
    }
}

#[test]
fn dynamic_partition_strings() {
    assert_eq!(dyn_obj("t", false, &[], "boom").to_string(), "boom");
    assert_eq!(dyn_obj("t", true, &["p0"], "").to_string(), "partition:all");
    assert_eq!(dyn_obj("t", false, &[], "").to_string(), "partition:dual");
    assert_eq!(
        dyn_obj("t", false, &["p0", "p1"], "").to_string(),
        "partition:p0,p1"
    );

    let none = DynamicPartitionAccessObjects(Vec::new());
    assert_eq!(AccessObject::to_string(&none), "");
    let one = DynamicPartitionAccessObjects(vec![dyn_obj("t", false, &["p1"], "")]);
    assert_eq!(AccessObject::to_string(&one), "partition:p1");
    let two = DynamicPartitionAccessObjects(vec![
        dyn_obj("t1", true, &[], ""),
        dyn_obj("t2", false, &[], "e"),
    ]);
    assert_eq!(
        AccessObject::to_string(&two),
        "partition:all of t1, e of t2"
    );
    assert_eq!(two.normalized_string(), AccessObject::to_string(&two));
}

#[test]
fn dynamic_partition_set_into_pb() {
    let mut pb = ExplainOperator::default();
    DynamicPartitionAccessObjects(Vec::new()).set_into_pb(Some(&mut pb));
    assert!(pb.get_access_objects().is_empty());
    let objs = DynamicPartitionAccessObjects(vec![
        dyn_obj("t1", false, &["p0"], ""),
        dyn_obj("t2", false, &[], "e"),
    ]);
    objs.set_into_pb(None);
    objs.set_into_pb(Some(&mut pb));
    assert_eq!(pb.get_access_objects().len(), 1);
    let list = pb.get_access_objects()[0]
        .get_dynamic_partition_objects()
        .get_objects();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].get_table(), "t1");
    assert_eq!(list[0].get_partitions(), ["p0".to_owned()]);
    // An object with an error is left empty, as Go leaves its slot zero.
    assert_eq!(list[1], tipb::DynamicPartitionAccessObject::default());
}

#[test]
fn scan_access_object() {
    let s = ScanAccessObject {
        database: "test".to_owned(),
        table: "t".to_owned(),
        indexes: vec![
            IndexAccess {
                name: "PRIMARY".to_owned(),
                cols: vec!["a".to_owned(), "b".to_owned()],
                is_clustered_index: true,
            },
            IndexAccess {
                name: "idx".to_owned(),
                cols: vec!["c".to_owned()],
                is_clustered_index: false,
            },
        ],
        partitions: vec!["p0".to_owned(), "p1".to_owned()],
    };
    assert_eq!(
        AccessObject::to_string(&s),
        "table:t, partition:p0,p1, clustered index:PRIMARY(a, b), index:idx(c)"
    );
    assert_eq!(
        s.normalized_string(),
        "table:t, partition:?, clustered index:PRIMARY(a, b), index:idx(c)"
    );
    let empty = ScanAccessObject::default();
    assert_eq!(AccessObject::to_string(&empty), "");

    let mut pb = ExplainOperator::default();
    s.set_into_pb(None);
    s.set_into_pb(Some(&mut pb));
    let scan = pb.get_access_objects()[0].get_scan_object();
    assert_eq!(scan.get_database(), "test");
    assert_eq!(scan.get_table(), "t");
    assert_eq!(scan.get_partitions().len(), 2);
    assert_eq!(scan.get_indexes().len(), 2);
    assert!(scan.get_indexes()[0].get_is_clustered_index());
    assert_eq!(scan.get_indexes()[1].get_cols(), ["c".to_owned()]);
}

#[test]
fn other_access_object() {
    let o = OtherAccessObject("CTE:c".to_owned());
    assert_eq!(AccessObject::to_string(&o), "CTE:c");
    assert_eq!(o.normalized_string(), "CTE:c");
    let mut pb = ExplainOperator::default();
    OtherAccessObject(String::new()).set_into_pb(Some(&mut pb));
    assert!(pb.get_access_objects().is_empty());
    o.set_into_pb(Some(&mut pb));
    assert_eq!(pb.get_access_objects()[0].get_other_object(), "CTE:c");
}
