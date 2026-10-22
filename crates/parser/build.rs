//! Build script: turns `grammar/tidb_typed.y` into the parser's tables and action code.
//!
//! cfgrammar + lrtable compute the LR(1) automaton (Pager's algorithm, the same table grmtools'
//! lrpar would use); `build/codegen.rs` then emits, into `OUT_DIR`:
//!
//! * `tables.bin`  comb-packed action/goto tables with default reductions (`include_bytes!`ed
//!   by `src/driver.rs`),
//! * `actions.rs`  the value enum, one action fn + one reducer fn per production, the reducer
//!   table and production metadata,
//! * `token_map.rs` the terminal name -> token id map used by the lexer.
//!
//! Set `TIDB_PARSER_DUMP_TABLE=<path>` to also write a textual dump of the automaton, used by
//! `tools/parser-port/compare_tables.py` to check it against goyacc's `y.output`.
use std::{env, fmt::Write as _, fs, path::PathBuf, time::Instant};

#[path = "build/codegen.rs"]
mod codegen;

const GRAMMAR: &str = "grammar/tidb_typed.y";

fn main() {
    println!("cargo::rerun-if-changed={GRAMMAR}");
    println!("cargo::rerun-if-changed=build/codegen.rs");
    println!("cargo::rerun-if-env-changed=TIDB_PARSER_DUMP_TABLE");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let src = fs::read_to_string(GRAMMAR).expect("read grammar");

    let dump_path = env::var("TIDB_PARSER_DUMP_TABLE").ok();

    let t0 = Instant::now();
    let out = codegen::generate(&src, dump_path.is_some());
    fs::write(out_dir.join("tables.bin"), &out.tables).expect("write tables");
    fs::write(out_dir.join("actions.rs"), &out.code).expect("write actions");

    let mut tm = String::from("pub static TOKEN_IDS: &[(&str, u32)] = &[\n");
    for (name, id) in &out.token_ids {
        writeln!(tm, "    ({name:?}, {id}),").unwrap();
    }
    tm.push_str("];\n");
    writeln!(
        tm,
        "/// Table generator statistics (informational).\npub const BUILD_REPORT: &str = {:?};",
        format!(
            "generated in {:.2}s: {}",
            t0.elapsed().as_secs_f64(),
            out.stats
        )
    )
    .unwrap();
    fs::write(out_dir.join("token_map.rs"), tm).expect("write token map");

    if let (Some(path), Some(dump)) = (dump_path, &out.dump) {
        fs::write(path, dump).expect("write table dump");
    }
}
