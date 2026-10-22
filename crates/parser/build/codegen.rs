//! Table and action code generator (included by `build.rs`).
//!
//! Reads the typed grmtools grammar with cfgrammar, builds the LR(1) table with lrtable
//! (Pager's algorithm) and emits:
//!
//! * a binary blob of comb-packed action/goto tables with default reductions, loaded at runtime
//!   via `include_bytes!` (no giant array literal for rustc to chew on);
//! * Rust code: a value enum `V` (one variant per distinct Rust value type plus `Tok`), one
//!   action fn `a_<prod>` per production (the grammar's action text with `$n`/`$span`/`$lexer`
//!   substituted), one reducer fn `r_<prod>` that pops the arguments and pushes the result, and
//!   a static reducer table. Same-type passthroughs (`$$ = $1`) map to the no-op reducer.
//!
//! Table blob layout (all little-endian u32):
//! `[n_states, action_len, goto_len, start_state]`, then `default[n_states]`,
//! `action_base[n_states]`, `action_check[action_len]`, `action_value[action_len]`,
//! `goto_base[n_states]`, `goto_check[goto_len]`, `goto_value[goto_len]`.
//! Action values: `s + 1` = shift to state `s`; `-(p + 1)` = reduce production `p`;
//! `i32::MIN` = accept; `0` (only as a default) = error.
use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
};

use cfgrammar::{
    RIdx, Symbol, TIdx,
    yacc::{YaccGrammar, YaccKind},
};
use lrtable::{Action, Minimiser, StIdx, from_yacc};

pub struct Generated {
    pub tables: Vec<u8>,
    pub code: String,
    /// Terminal name -> token id, sorted by name.
    pub token_ids: Vec<(String, u32)>,
    pub stats: String,
    /// Textual automaton dump for `tools/parser-port/compare_tables.py` (if requested).
    pub dump: Option<String>,
}

pub const ACCEPT: i32 = i32::MIN;

/// Classic yacc "comb" packing: row `r`'s entry for column `c` lives at `base[r] + c` if
/// `check[base[r] + c] == r`. Rows are placed densest first, at the first base that fits.
pub fn comb_pack(rows: &[Vec<(u32, i32)>], width: usize) -> (Vec<i32>, Vec<u32>, Vec<i32>) {
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by_key(|&r| std::cmp::Reverse(rows[r].len()));
    let mut base = vec![0i32; rows.len()];
    let mut check: Vec<u32> = Vec::new();
    let mut val: Vec<i32> = Vec::new();
    let mut used_base: HashSet<i32> = HashSet::new();
    let mut first_free = 0usize;
    for &r in &order {
        let row = &rows[r];
        if row.is_empty() {
            // Out of range for every column: lookups always fall back to the default.
            base[r] = -(width as i32) - 1;
            continue;
        }
        let min_col = i64::from(row[0].0);
        let mut b: i64 = first_free as i64 - min_col;
        loop {
            if !used_base.contains(&(b as i32))
                && row.iter().all(|&(c, _)| {
                    let i = (b + i64::from(c)) as usize;
                    i >= check.len() || check[i] == u32::MAX
                })
            {
                break;
            }
            b += 1;
        }
        used_base.insert(b as i32);
        base[r] = b as i32;
        for &(c, v) in row {
            let i = (b + i64::from(c)) as usize;
            if i >= check.len() {
                check.resize(i + 1, u32::MAX);
                val.resize(i + 1, 0);
            }
            check[i] = r as u32;
            val[i] = v;
        }
        while first_free < check.len() && check[first_free] != u32::MAX {
            first_free += 1;
        }
    }
    (base, check, val)
}

/// Substitute grmtools action syntax: `$n` -> `an`, `$span` -> `span`, `$lexer` -> `lexer`.
fn subst_action(code: &str) -> String {
    let mut out = String::with_capacity(code.len());
    let b = code.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'$' {
            if code[i..].starts_with("$lexer") {
                out.push_str("lexer");
                i += 6;
                continue;
            }
            if code[i..].starts_with("$span") {
                out.push_str("span");
                i += 5;
                continue;
            }
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 {
                out.push('a');
                out.push_str(&code[i + 1..j]);
                i = j;
                continue;
            }
        }
        // Copy whole UTF-8 sequences (the action text may contain non-ASCII in strings).
        let ch = code[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

pub fn generate(grammar_src: &str, want_dump: bool) -> Generated {
    // cfgrammar's YaccGrammar::new does not understand the `%grmtools` header line.
    let body: String = grammar_src
        .lines()
        .map(|l| if l.starts_with("%grmtools") { "" } else { l })
        .collect::<Vec<_>>()
        .join("\n");
    let grm = match YaccGrammar::<u32>::new(YaccKind::Grmtools, &body) {
        Ok(g) => g,
        Err(errs) => {
            let msgs: Vec<String> = errs.iter().take(20).map(|e| e.to_string()).collect();
            panic!("grammar errors:\n{}", msgs.join("\n"));
        }
    };
    let (sg, st) = from_yacc(&grm, Minimiser::Pager).expect("LR table construction failed");
    if let Some(c) = st.conflicts() {
        // goyacc resolves the same conflicts by precedence; anything left over would be a
        // porting bug, so make it loud.
        assert!(
            c.sr_len() == 0 && c.rr_len() == 0,
            "unresolved conflicts: {} shift/reduce, {} reduce/reduce",
            c.sr_len(),
            c.rr_len()
        );
    }
    let n_states = usize::from(sg.all_states_len());
    let n_toks = usize::from(grm.tokens_len());
    let n_rules = usize::from(grm.rules_len());

    // ---- action rows with default reductions -----------------------------------------------
    let mut defaults = vec![0i32; n_states];
    let mut arows: Vec<Vec<(u32, i32)>> = Vec::with_capacity(n_states);
    let mut grows: Vec<Vec<(u32, i32)>> = Vec::with_capacity(n_states);
    let (mut total_entries, mut kept) = (0usize, 0usize);
    for (s, default) in defaults.iter_mut().enumerate() {
        let sidx: StIdx<u32> = StIdx(s as u32);
        let mut entries = Vec::new();
        let mut red_count: HashMap<i32, usize> = HashMap::new();
        for t in 0..n_toks {
            let v = match st.action(sidx, TIdx(t as u32)) {
                Action::Shift(ns) => usize::from(ns) as i32 + 1,
                Action::Reduce(p) => {
                    let v = -(usize::from(p) as i32) - 1;
                    *red_count.entry(v).or_default() += 1;
                    v
                }
                Action::Accept => ACCEPT,
                Action::Error => continue,
            };
            entries.push((t as u32, v));
        }
        total_entries += entries.len();
        // The most frequent reduction becomes the row default (ties: lowest production).
        let def = red_count
            .into_iter()
            .max_by_key(|&(v, c)| (c, -v))
            .map(|(v, _)| v)
            .unwrap_or(0);
        *default = def;
        let row: Vec<(u32, i32)> = entries.into_iter().filter(|&(_, v)| v != def).collect();
        kept += row.len();
        arows.push(row);
        let mut g = Vec::new();
        for r in 0..n_rules {
            if let Some(ns) = st.goto(sidx, RIdx(r as u32)) {
                g.push((r as u32, usize::from(ns) as i32));
            }
        }
        grows.push(g);
    }
    let (abase, acheck, aval) = comb_pack(&arows, n_toks);
    let (gbase, gcheck, gval) = comb_pack(&grows, n_rules);

    let mut tables = Vec::new();
    let mut w32 = |v: u32| tables.extend_from_slice(&v.to_le_bytes());
    for v in [
        n_states as u32,
        acheck.len() as u32,
        gcheck.len() as u32,
        usize::from(st.start_state()) as u32,
    ] {
        w32(v);
    }
    defaults.iter().for_each(|v| w32(*v as u32));
    abase.iter().for_each(|v| w32(*v as u32));
    acheck.iter().for_each(|v| w32(*v));
    aval.iter().for_each(|v| w32(*v as u32));
    gbase.iter().for_each(|v| w32(*v as u32));
    gcheck.iter().for_each(|v| w32(*v));
    gval.iter().for_each(|v| w32(*v as u32));

    // ---- code -------------------------------------------------------------------------------
    let mut types: Vec<String> = Vec::new();
    let mut tymap: HashMap<String, usize> = HashMap::new();
    let mut rule_var = vec![usize::MAX; n_rules];
    for r in grm.iter_rules() {
        if let Some(t) = grm.actiontype(r) {
            let n = types.len();
            let k = *tymap.entry(t.clone()).or_insert_with(|| {
                types.push(t.clone());
                n
            });
            rule_var[usize::from(r)] = k;
        }
    }
    let mut c = String::new();
    c.push_str(
        "// GENERATED by build.rs (build/codegen.rs) from grammar/tidb_typed.y. Do not edit.\n",
    );
    c.push_str("#[allow(unused_imports)] use crate::ast::*;\n");
    c.push_str("#[allow(unused_imports)] use crate::gen_types::*;\n");
    c.push_str("#[allow(unused_imports)] use crate::actions_rt::*;\n");
    c.push_str("use crate::driver::Src;\nuse crate::ctx::Ctx;\n");
    // User code section of the grammar (helper fns used by the hand-ported actions).
    if let Some(p) = grm.programs() {
        c.push_str(p);
        c.push('\n');
    }
    c.push_str("pub enum V {\n    Tok(Tok),\n");
    for (i, t) in types.iter().enumerate() {
        writeln!(c, "    T{i}({t}),").unwrap();
    }
    c.push_str("}\n");
    // The LR automaton guarantees the stack shape: the value below the top for production p's
    // k-th symbol always has that symbol's type. A mismatch would be a generator bug.
    c.push_str(
        "macro_rules! pop { ($st:ident, $v:ident) => { match $st.pop() { Some(V::$v(x)) => x, \
         _ => if cfg!(debug_assertions) { unreachable!(\"value stack type mismatch\") } \
         else { unsafe { std::hint::unreachable_unchecked() } } } }; }\n",
    );
    let span_ty = "crate::span::Span";
    let mut red_names = Vec::new();
    let (mut nops, mut fns) = (0, 0);
    let pparam = grm
        .parse_param()
        .as_ref()
        .map(|(n, _)| n.clone())
        .unwrap_or("_p".into());
    let mut prod_lhs = Vec::new();
    let mut prod_len = Vec::new();
    for p in grm.iter_pidxs() {
        let r = grm.prod_to_rule(p);
        prod_lhs.push(usize::from(r));
        prod_len.push(grm.prod(p).len());
        if p == grm.start_prod() {
            red_names.push("r_nop".to_string());
            continue;
        }
        let pi = usize::from(p);
        let prod = grm.prod(p);
        let lhs_var = rule_var[usize::from(r)];
        let code = grm.action(p).as_ref().expect("action").trim().to_string();
        // Same-type passthrough: the value already on the stack is the result.
        if prod.len() == 1
            && code == "$1"
            && let Symbol::Rule(r1) = prod[0]
            && rule_var[usize::from(r1)] == lhs_var
        {
            red_names.push("r_nop".to_string());
            nops += 1;
            continue;
        }
        fns += 1;
        let mut args = String::new();
        let mut pops = String::new();
        let mut call = String::new();
        for (k, s) in prod.iter().enumerate() {
            let (ty, var) = match s {
                Symbol::Rule(r1) => {
                    let v = rule_var[usize::from(*r1)];
                    (types[v].clone(), format!("T{v}"))
                }
                Symbol::Token(_) => ("Tok".to_string(), "Tok".to_string()),
            };
            write!(args, ", a{}: {}", k + 1, ty).unwrap();
            write!(call, ", a{}", k + 1).unwrap();
            pops = format!("    let a{} = pop!(st, {});\n", k + 1, var) + &pops;
        }
        let ret = &types[lhs_var];
        writeln!(
            c,
            "#[allow(unused_variables, unused_mut)]\nfn a_{pi}(lexer: &Src<'_>, span: {span_ty}, \
             {pparam}: &Ctx{args}) -> {ret} {{ {} }}",
            subst_action(&code)
        )
        .unwrap();
        writeln!(
            c,
            "fn r_{pi}(st: &mut Vec<V>, lexer: &Src<'_>, span: {span_ty}, {pparam}: &Ctx) {{\n\
             {pops}    st.push(V::T{lhs_var}(a_{pi}(lexer, span, {pparam}{call})));\n}}"
        )
        .unwrap();
        red_names.push(format!("r_{pi}"));
    }
    writeln!(
        c,
        "fn r_nop(_: &mut Vec<V>, _: &Src<'_>, _: {span_ty}, _: &Ctx) {{}}"
    )
    .unwrap();
    writeln!(
        c,
        "pub type Reducer = fn(&mut Vec<V>, &Src<'_>, {span_ty}, &Ctx);"
    )
    .unwrap();
    writeln!(
        c,
        "pub static REDUCERS: [Reducer; {}] = [{}];",
        red_names.len(),
        red_names.join(", ")
    )
    .unwrap();
    writeln!(
        c,
        "pub static PROD_LHS: [u16; {}] = {:?};",
        prod_lhs.len(),
        prod_lhs
    )
    .unwrap();
    writeln!(
        c,
        "pub static PROD_LEN: [u8; {}] = {:?};",
        prod_len.len(),
        prod_len
    )
    .unwrap();
    writeln!(
        c,
        "pub const EOF: u32 = {};",
        usize::from(grm.eof_token_idx())
    )
    .unwrap();
    let start_rule = match grm.prod(grm.start_prod())[0] {
        Symbol::Rule(r) => r,
        Symbol::Token(_) => unreachable!("start production must derive the start rule"),
    };
    let sv = rule_var[usize::from(start_rule)];
    writeln!(c, "pub type StartT = {};", types[sv]).unwrap();
    writeln!(
        c,
        "pub fn take_start(v: V) -> StartT {{ match v {{ V::T{sv}(x) => x, _ => unreachable!() }} }}"
    )
    .unwrap();

    let mut token_ids: Vec<(String, u32)> = grm
        .iter_tidxs()
        .filter_map(|t| {
            grm.token_name(t)
                .map(|n| (n.to_string(), usize::from(t) as u32))
        })
        .collect();
    token_ids.sort();

    let stats = format!(
        "states={n_states} tokens={n_toks} rules={n_rules} prods={} terminal_entries={total_entries} \
         after_default_reductions={kept} action_comb_len={} goto_comb_len={} table_bytes={} \
         value_variants={} reducer_fns={fns} passthrough_nops={nops}",
        usize::from(grm.prods_len()),
        acheck.len(),
        gcheck.len(),
        tables.len(),
        types.len() + 1
    );
    let dump = want_dump.then(|| dump_automaton(&grm, n_states, &st));
    Generated {
        tables,
        code: c,
        token_ids,
        stats,
        dump,
    }
}

/// Text dump of the automaton in the format `compare_tables.py` reads.
fn dump_automaton(
    grm: &YaccGrammar<u32>,
    n_states: usize,
    st: &lrtable::StateTable<u32>,
) -> String {
    let prod_str = |p| {
        let r = grm.prod_to_rule(p);
        let rhs: Vec<&str> = grm
            .prod(p)
            .iter()
            .map(|s| match s {
                Symbol::Rule(r) => grm.rule_name_str(*r),
                Symbol::Token(t) => grm.token_name(*t).unwrap_or("$end"),
            })
            .collect();
        format!("{}: {}", grm.rule_name_str(r), rhs.join(" "))
    };
    let mut txt = format!("START {}\n", usize::from(st.start_state()));
    for s in 0..n_states {
        let stidx = StIdx::<u32>(s as u32);
        writeln!(txt, "S {s}").unwrap();
        for t in grm.iter_tidxs() {
            let name = grm.token_name(t).unwrap_or("$end");
            match st.action(stidx, t) {
                Action::Shift(n) => writeln!(txt, "T {name}\tshift {}", usize::from(n)).unwrap(),
                Action::Reduce(p) => writeln!(txt, "T {name}\treduce {}", prod_str(p)).unwrap(),
                Action::Accept => writeln!(txt, "T {name}\taccept").unwrap(),
                Action::Error => {}
            }
        }
        for r in grm.iter_rules() {
            if let Some(n) = st.goto(stidx, r) {
                writeln!(txt, "G {}\t{}", grm.rule_name_str(r), usize::from(n)).unwrap();
            }
        }
    }
    txt
}
