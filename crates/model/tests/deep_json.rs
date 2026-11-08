//! A `json.RawMessage` holding a document nested to Go's limit, decoded and encoded on a
//! 2 MiB thread stack: the `Raw` tree of the whole job is dropped without recursion (review
//! of tidb-rs #25).

use tidb_model::Job;

#[test]
fn job_raw_args_at_max_depth() {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(|| {
            // The job object is level 1: the arguments may nest 9999 more levels.
            let args = format!("{}{}", "[".repeat(9999), "]".repeat(9999));
            let doc = format!(r#"{{"id":7,"raw_args":{args}}}"#);
            let mut job = Job::default();
            job.unmarshal_json(doc.as_bytes())
                .expect("valid at depth 10000");
            assert_eq!(job.id, 7);
            assert_eq!(job.raw_args.as_deref(), Some(args.as_bytes()));
            let out = job.marshal_json().unwrap();
            assert!(out.contains(&format!(r#""raw_args":{args}"#)));

            let too_deep = format!(r#"{{"raw_args":[{args}]}}"#);
            let mut job = Job::default();
            assert!(job.unmarshal_json(too_deep.as_bytes()).is_err());
        })
        .expect("spawn")
        .join()
        .unwrap_or_else(|p| std::panic::resume_unwind(p));
}
