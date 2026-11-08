import re
TSV='/Users/du-zhigang/.slock/agents/4e290aa1-0152-4fca-95a4-03ab60358460/projects/tracker-d052/data/sysvar_hooks.tsv'
OUT='/Users/du-zhigang/.slock/agents/4e290aa1-0152-4fca-95a4-03ab60358460/projects/tidb-rs-d052/crates/sessionctx/src/variable/session_hooks.rs'
gen=open('ss_gen.rs').read()
table=[l.strip() for l in open('ss_table.txt') if l.strip()]
# X-field hooks: write-path fields not in tidb-rs (D05.2 scoping): systems[] only
xhooks={'tidb_batch_insert':'BatchInsert','tidb_batch_delete':'BatchDelete','tidb_batch_commit':'BatchCommit',
 'tidb_allow_remove_auto_inc':'AllowRemoveAutoInc','tidb_dml_batch_size':'DMLBatchSize','tidb_retry_limit':'RetryLimit',
 'tidb_disable_txn_auto_retry':'DisableTxnAutoRetry','allow_auto_random_explicit_insert':'AllowAutoRandExplicitInsert',
 'tidb_shard_allocate_step':'ShardAllocateStep','tidb_ddl_reorg_priority':'DDLReorgPriority'}
pend_lines={l.split('\t')[0]:l.split('\t')[1] for l in open('ss_pending.txt')}
xfns=[]
for n,f in xhooks.items():
    fn='ss_'+re.sub(r'[^a-z0-9]+','_',n)
    xfns.append(f'// go-part: defaultSysVars ({pend_lines[n].replace(":",":L")} {n} SetSession closure) L2 (writes `SessionVars.{f}`, a write-path field: X in the D05.2 scoping, so only systems[] changes)\nfn {fn}(_s: &mut SessionVars, _val: &str) -> Result<(), Error> {{\n    Ok(())\n}}\n')
    table.append(f'"{n}" => {fn},')
manual=open('manual.rs').read()
for m in re.finditer(r'\(\w+\.go:L\d+ (\S+) SetSession closure\)[^\n]*\nfn (\w+)',manual): table.append(f'"{m.group(1)}" => {m.group(2)},')
gs_table={m.group(1):m.group(2) for m in re.finditer(r'\(\w+\.go:L\d+ (\S+) GetSession closure\)[^\n]*\nfn (\w+)',manual)}
gv_table={m.group(1):m.group(2) for m in re.finditer(r'\(\w+\.go:L\d+ (\S+) GetStateValue closure\)[^\n]*\nfn (\w+)',manual)}
implemented={re.match(r'"([^"]+)"',t).group(1) for t in table}
pend={'SetSession':[], 'SetGlobal':[], 'GetSession':[], 'GetGlobal':[], 'GetStateValue':[]}
for l in open(TSV):
    if l.startswith('#') or l.startswith('name\t'): continue
    f=l.rstrip('\n').split('\t')
    name=f[0]
    for k,i in [('SetSession',3),('SetGlobal',4),('GetSession',5),('GetGlobal',6),('GetStateValue',7)]:
        if f[i]=='1' and not (k=='SetSession' and name in implemented) and not (k=='GetSession' and name in gs_table) and not (k=='GetStateValue' and name in gv_table):
            pend[k].append(name)
def arr(k):
    items=''.join(f'    "{n}",\n' for n in sorted(pend[k]))
    return f'/// Variables whose Go entry has a `{k}` hook that is not ported yet.\nconst PENDING_{re.sub("([a-z])([A-Z])",r"\1_\2",k).upper()}: &[&str] = &[\n{items}];\n'
head='''//! go-file: pkg/sessionctx/variable/sysvar.go
//! go-file: pkg/sessionctx/variable/noop.go
//!
//! The session hooks of the system variable table (block D05.2): `SetSession`,
//! `SetGlobal`, `GetSession`, `GetGlobal` and `GetStateValue`. Go writes them as closures
//! inside the table literals (sysvar.go, noop.go); here they are functions named after the
//! variable (`ss_` SetSession, `sg_` SetGlobal, `gs_` GetSession, `gg_` GetGlobal, `gv_`
//! GetStateValue), each with the Go position of its closure. The checklist is tracker
//! `data/sysvar_hooks.tsv`; a Go hook that is not ported yet is listed in the `PENDING_*`
//! tables below, and the registry checks at construction (and in a unit test) that every
//! Go hook is either here or pending, and that no hook is here that Go does not have.

// Go names for constants (M-GONAME).
#![allow(non_upper_case_globals)]

use std::collections::HashSet;
use std::sync::LazyLock;

use tidb_base::Error;
use tidb_base::gostd;

use super::session::SessionVars;
use super::tidb_defs::*;
use super::tidb_vars::*;
use super::variable::{GetGlobalFn, GetSessionFn, GetStateValueFn, SetGlobalFn, SetSessionFn};
use super::varsutil::*;

/// The session hooks of one variable.
#[derive(Clone, Copy, Default)]
pub(crate) struct SessionHooks {
    pub(crate) set_session: Option<SetSessionFn>,
    pub(crate) set_global: Option<SetGlobalFn>,
    pub(crate) get_session: Option<GetSessionFn>,
    pub(crate) get_global: Option<GetGlobalFn>,
    pub(crate) get_state_value: Option<GetStateValueFn>,
}

/// The session hooks of a variable (the `SetSession`, `SetGlobal`, `GetSession`,
/// `GetGlobal` and `GetStateValue` fields of its Go table entry).
// glue: the hook fields of the Go table literals, keyed by variable name
pub(crate) fn hooks_of(name: &str) -> SessionHooks {
    SessionHooks {
        set_session: set_session(name),
        get_session: get_session(name),
        get_state_value: get_state_value(name),
        ..SessionHooks::default()
    }
}

// glue: the SetSession fields of the Go table literals, keyed by variable name
fn set_session(name: &str) -> Option<SetSessionFn> {
    Some(match name {
'''
mid='''        _ => return None,
    })
}

// glue: the GetSession fields of the Go table literals, keyed by variable name
fn get_session(name: &str) -> Option<GetSessionFn> {
    Some(match name {
GS_TABLE        _ => return None,
    })
}

// glue: the GetStateValue fields of the Go table literals, keyed by variable name
fn get_state_value(name: &str) -> Option<GetStateValueFn> {
    Some(match name {
GV_TABLE        _ => return None,
    })
}

/// Whether Go's `kind` hook of `name` is not ported yet.
// glue: the pending part of the D05.2 hook checklist
pub(crate) fn is_pending(name: &str, kind: &str) -> bool {
    static SETS: LazyLock<[(&str, HashSet<&str>); 5]> = LazyLock::new(|| {
        [
            ("SetSession", PENDING_SET_SESSION.iter().copied().collect()),
            ("SetGlobal", PENDING_SET_GLOBAL.iter().copied().collect()),
            ("GetSession", PENDING_GET_SESSION.iter().copied().collect()),
            ("GetGlobal", PENDING_GET_GLOBAL.iter().copied().collect()),
            ("GetStateValue", PENDING_GET_STATE_VALUE.iter().copied().collect()),
        ]
    });
    SETS.iter().any(|(k, set)| *k == kind && set.contains(name))
}

'''
mid=mid.replace('GS_TABLE',''.join(f'        "{k}" => {v},\n' for k,v in gs_table.items())).replace('GV_TABLE',''.join(f'        "{k}" => {v},\n' for k,v in gv_table.items()))
body=head+''.join('        '+t+'\n' for t in table)+mid+'\n'.join(arr(k) for k in pend)+'\n'+gen+'\n'+'\n'.join(xfns)+'\n'+manual
open(OUT,'w').write(body)
print({k:len(v) for k,v in pend.items()}, len(table))
