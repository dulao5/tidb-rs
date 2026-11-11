//! Every ported class of Go's `funcs` map is registered in `builtin_registry::function_class`
//! (Lucky, review of #95): each `impl FunctionClass for` in `src/` has an entry in
//! `PORTED_FUNCTION_CLASSES`, and each entry names a function of `funcs`.

use tidb_expression::builtin_registry::{PORTED_FUNCTION_CLASSES, function_class, lookup};

fn impls(dir: &std::path::Path, out: &mut Vec<String>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            impls(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            let text = std::fs::read_to_string(&p).unwrap();
            for line in text.lines() {
                let l = line.trim_start();
                if l.starts_with("impl") && l.contains("FunctionClass for ") && !l.starts_with("//")
                {
                    out.push(format!("{}: {l}", p.display()));
                }
            }
        }
    }
}

#[test]
fn every_ported_function_class_is_registered() {
    let mut found = Vec::new();
    impls(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut found,
    );
    assert_eq!(
        found.len(),
        PORTED_FUNCTION_CLASSES.len(),
        "impl FunctionClass in src/ vs PORTED_FUNCTION_CLASSES entries:\n{}",
        found.join("\n")
    );
    for (name, _) in PORTED_FUNCTION_CLASSES {
        assert!(lookup(name).is_some(), "{name} is not in funcs");
        assert!(function_class(name).is_some(), "{name} is not dispatched");
    }
}
