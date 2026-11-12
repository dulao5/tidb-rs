//! Every ported class of Go's `funcs` map is registered in `builtin_registry::function_class`
//! (Lucky, review of #95): each instance of a type that implements `FunctionClass` in
//! `src/` (Go: each `funcs` entry, e.g. the seven `compareFunctionClass` values) has an
//! entry in `PORTED_FUNCTION_CLASSES`, and each entry names a function of `funcs`.

use tidb_expression::builtin_registry::{PORTED_FUNCTION_CLASSES, function_class, lookup};

fn sources(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            sources(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push((
                p.display().to_string(),
                std::fs::read_to_string(&p).unwrap(),
            ));
        }
    }
}

#[test]
fn every_ported_function_class_is_registered() {
    let mut src = Vec::new();
    sources(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut src,
    );
    // The types that implement FunctionClass.
    let mut types = Vec::new();
    for (_, text) in &src {
        for line in text.lines() {
            let l = line.trim_start();
            if l.starts_with("impl")
                && let Some(rest) = l.split("FunctionClass for ").nth(1)
            {
                types.push(rest.trim_end_matches(" {").trim().to_owned());
            }
        }
    }
    // Their instances: `pub static NAME: Type = ..` (directly or through a macro that
    // declares `pub static $name: Type`, whose invocations are `macro!(NAME, ..)`).
    let mut instances = Vec::new();
    for (path, text) in &src {
        for t in &types {
            for line in text.lines() {
                let l = line.trim_start();
                if let Some(rest) = l.strip_prefix("pub static ")
                    && let Some((name, ty)) = rest.split_once(':')
                    && ty.trim().trim_end_matches(" = {").starts_with(t.as_str())
                {
                    if name.trim().starts_with('$') {
                        // A macro: its invocations are `<macro>!(NAME, ..)`.
                        let mac = text[..text.find(l).unwrap()]
                            .rsplit("macro_rules! ")
                            .next()
                            .unwrap()
                            .split_whitespace()
                            .next()
                            .unwrap()
                            .to_owned();
                        for inv in text.lines() {
                            if let Some(args) = inv.trim_start().strip_prefix(&format!("{mac}!(")) {
                                instances.push((
                                    path.clone(),
                                    args.split(',').next().unwrap().trim().to_owned(),
                                ));
                            }
                        }
                    } else {
                        instances.push((path.clone(), name.trim().to_owned()));
                    }
                }
            }
        }
    }
    assert!(!types.is_empty() || PORTED_FUNCTION_CLASSES.is_empty());
    let registry = src
        .iter()
        .find(|(p, _)| p.ends_with("builtin_registry.rs"))
        .unwrap()
        .1
        .clone();
    let table = registry
        .split("pub static PORTED_FUNCTION_CLASSES")
        .nth(1)
        .unwrap();
    for (path, name) in &instances {
        assert!(
            table.contains(&format!("::{name})"))
                || table.contains(&format!(" {name})"))
                // rustfmt breaks a long entry, leaving a trailing comma
                || table.contains(&format!("::{name},")),
            "{path}: {name} is not in PORTED_FUNCTION_CLASSES"
        );
    }
    assert_eq!(
        instances.len(),
        PORTED_FUNCTION_CLASSES.len(),
        "FunctionClass instances {instances:?} vs PORTED_FUNCTION_CLASSES entries"
    );
    for (name, _) in PORTED_FUNCTION_CLASSES {
        assert!(lookup(name).is_some(), "{name} is not in funcs");
        assert!(function_class(name).is_some(), "{name} is not dispatched");
    }
}
