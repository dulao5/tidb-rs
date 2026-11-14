//! The interned `FieldType` charset / collation names (spec N13 §8): the entry points that
//! bring names into a `FieldType` either validate and lower-case them (parser, sysvars), so
//! case variants and unknown names never reach `intern_name`, or keep them as stored (schema
//! JSON, as Go does), so the intern set grows once per distinct name and not per decode.
//!
//! One test in this binary: nothing else interns names concurrently.

use tidb_base::Error;
use tidb_parser::types::{FieldType, interned_name_count};
use tidb_parser::{ParseOptions, parse};
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};

struct Acc;

impl GlobalVarAccessor for Acc {
    fn get_global_sys_var(&self, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    fn set_global_sys_var_only(&mut self, _: &str, _: &str, _: bool) -> Result<(), Error> {
        Ok(())
    }
    fn get_tidb_table_value(&self, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    fn set_tidb_table_value(&mut self, _: &str, _: &str, _: &str) -> Result<(), Error> {
        Ok(())
    }
}

/// The quoted values of `charset: "..."` / `collate: "..."` in the parser's AST (the names a
/// plan builder copies into a `FieldType`).
fn ast_names(debug: &str) -> Vec<String> {
    let mut out = Vec::new();
    for key in ["charset: \"", "collate: \""] {
        let mut rest = debug;
        while let Some(i) = rest.find(key) {
            rest = &rest[i + key.len()..];
            let end = rest.find('"').unwrap();
            out.push(rest[..end].to_owned());
            rest = &rest[end..];
        }
    }
    out
}

/// Interns `cs` / `co` as a plan builder does (`SetCharset` / `SetCollate`).
fn intern(cs: &str, co: &str) {
    let mut ft = FieldType::new(tidb_base::mysql::TypeVarchar);
    ft.set_charset(cs);
    ft.set_collate(co);
}

#[test]
fn entry_points_do_not_grow_the_intern_set() {
    let base = interned_name_count();

    // 1. Parser: CHARACTER SET / COLLATE / introducers are validated and lower-cased.
    for sql in [
        "SELECT CAST(a AS CHAR CHARACTER SET UTF8MB4), a COLLATE UTF8MB4_BIN",
        "SELECT CAST(a AS CHAR(10) CHARSET Latin1) COLLATE Latin1_Bin",
        "SELECT CONVERT(a USING GBK), _UTF8MB4'x' COLLATE utf8mb4_General_CI",
    ] {
        let out = parse(sql, &ParseOptions::default()).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        let debug = format!("{:?}", out.stmts);
        let names = ast_names(&debug);
        assert!(!names.is_empty(), "{sql}: no names in {debug}");
        for n in &names {
            assert_eq!(n, &n.to_lowercase(), "{sql}: {n}");
            intern(n, n);
        }
    }
    // SET NAMES: the charset is a validated `CharsetName`; the collation is a plain
    // `StringName` kept as written (as in Go), applied through `collation_connection` (step 2).
    let sql = "SET NAMES 'UTF8MB4' COLLATE 'UTF8MB4_UNICODE_CI'";
    let debug = format!("{:?}", parse(sql, &ParseOptions::default()).unwrap().stmts);
    assert!(debug.contains("Str(\"utf8mb4\")"), "{debug}");
    for sql in [
        "SELECT CAST(a AS CHAR CHARACTER SET no_such_cs)",
        "SELECT a COLLATE no_such_collation",
    ] {
        assert!(parse(sql, &ParseOptions::default()).is_err(), "{sql}");
    }
    assert_eq!(interned_name_count(), base, "parser");

    // 2. Sysvars: the character_set_* / collation_* checks return the canonical name and
    //    reject unknown names before anything is stored.
    let mut vars = SessionVars::new(Box::new(Acc));
    for (name, val) in [
        ("character_set_client", "UTF8MB4"),
        ("character_set_connection", "Latin1"),
        ("character_set_results", "GBK"),
        ("collation_connection", "UTF8MB4_UNICODE_CI"),
        ("collation_connection", "UTF8MB4_BIN"),
        ("collation_database", "Utf8mb4_General_Ci"),
        ("collation_server", "LATIN1_BIN"),
    ] {
        vars.set_system_var(name, val).unwrap();
        let (stored, ok) = vars.get_system_var(name);
        assert!(ok, "{name}");
        assert_eq!(stored, val.to_lowercase(), "{name}");
        intern(&stored, &stored);
    }
    for (name, val) in [
        ("character_set_connection", "no_such_cs"),
        ("collation_connection", "no_such_collation"),
    ] {
        assert!(vars.set_system_var(name, val).is_err(), "{name}");
    }
    let (cs, co) = vars.get_charset_info();
    intern(&cs, &co);
    assert_eq!(interned_name_count(), base, "sysvars");

    // 3. Schema JSON: canonical names (what DDL stores) are not copied.
    let json = |cs: &str, co: &str| {
        format!(
            "{{\"Tp\":15,\"Flag\":0,\"Flen\":64,\"Decimal\":0,\"Charset\":\"{cs}\",\
             \"Collate\":\"{co}\",\"Elems\":null,\"ElemsIsBinaryLit\":null,\"Array\":false}}"
        )
    };
    let mut ft = FieldType::default();
    ft.unmarshal_json(json("utf8mb4", "utf8mb4_bin").as_bytes())
        .unwrap();
    assert_eq!(interned_name_count(), base, "canonical JSON");

    // A stored variant (a TableInfo older than TableInfoVersion3) or an unknown name is kept
    // as stored, as Go keeps it, so the JSON round-trips byte for byte: it is copied once,
    // and decoding it again does not copy it again.
    let odd = json("UTF8MB4", "no_such_collation");
    for _ in 0..1000 {
        let mut ft = FieldType::default();
        ft.unmarshal_json(odd.as_bytes()).unwrap();
        assert_eq!(ft.marshal_json(), odd);
    }
    assert_eq!(
        interned_name_count(),
        base + 2,
        "variant JSON: once per distinct name"
    );
}
