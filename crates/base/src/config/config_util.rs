//! go-file: pkg/config/config_util.go

use std::collections::BTreeMap;

use super::Config;
use super::reflect::{self, GoStruct, ValMut, ValRef};
use crate::errors::Error;
use crate::gostd::json;

/// CloneConf deeply clones this config. (Go: a JSON round trip; the port clones and applies
/// what the round trip changes, see [`reflect::json_round_trip`].)
// go: CloneConf L1
pub fn clone_conf(conf: &Config) -> Result<Config, Error> {
    // json.Marshal fails on NaN / ±Inf floats.
    reflect::to_json(conf, false)?;
    let mut cloned_conf = conf.clone();
    reflect::json_round_trip(&mut cloned_conf);
    Ok(cloned_conf)
}

/// dynamicConfigItems contains all config items that can be changed during runtime.
static dynamicConfigItems: &[&str] = &[
    "Performance.MaxProcs",
    "Performance.MaxMemory",
    "Performance.CrossJoin",
    "Performance.PseudoEstimateRatio",
    "Performance.StmtCountLimit",
    "Performance.TCPKeepAlive",
    "TiKVClient.StoreLimit",
    "Log.Level",
    "Log.ExpensiveThreshold",
    "Instance.SlowThreshold",
    "Instance.CheckMb4ValueInUTF8",
    "TxnLocalLatches.Capacity",
    "CompatibleKillQuery",
    "TreatOldVersionUTF8AsUTF8MB4",
    "OpenTracing.Enable",
];

/// MergeConfigItems overwrites the dynamic config items and leaves the other items unchanged.
// go: MergeConfigItems L1
pub fn merge_config_items(dst_conf: &mut Config, new_conf: &Config) -> (Vec<String>, Vec<String>) {
    merge_config_items_struct(dst_conf, new_conf, "")
}

/// `mergeConfigItems` on a struct value (Go recurses with `reflect.Value`s; leaf values are
/// handled in [`merge_leaf`]).
// go: mergeConfigItems L1
fn merge_config_items_struct(
    dst_conf: &mut dyn GoStruct,
    new_conf: &dyn GoStruct,
    field_path: &str,
) -> (Vec<String>, Vec<String>) {
    let (mut accepted_items, mut rejected_items) = (Vec::new(), Vec::new());
    for i in 0..dst_conf.fields().len() {
        let mut field_name = dst_conf.fields()[i].go_name.to_owned();
        if !field_path.is_empty() {
            field_name = format!("{field_path}.{field_name}");
        }
        let src = new_conf.field(i);
        let (as_, rs) = match (dst_conf.field_mut(i), &src) {
            (ValMut::Struct(d), ValRef::Struct(s)) => merge_config_items_struct(d, *s, &field_name),
            // nullableBool is a struct: Go recurses into IsValid and IsTrue.
            (ValMut::NullableBool(d), ValRef::NullableBool(s)) => {
                let mut acc = Vec::new();
                let mut rej = Vec::new();
                let (a, r) = merge_leaf(
                    ValMut::Bool(&mut d.is_valid),
                    &ValRef::Bool(&s.is_valid),
                    &format!("{field_name}.IsValid"),
                );
                acc.extend(a);
                rej.extend(r);
                let (a, r) = merge_leaf(
                    ValMut::Bool(&mut d.is_true),
                    &ValRef::Bool(&s.is_true),
                    &format!("{field_name}.IsTrue"),
                );
                acc.extend(a);
                rej.extend(r);
                (acc, rej)
            }
            (d, s) => merge_leaf(d, s, &field_name),
        };
        accepted_items.extend(as_);
        rejected_items.extend(rs);
    }
    (accepted_items, rejected_items)
}

/// The non-struct cases of `mergeConfigItems` (including `AtomicBool`, compared and set as
/// a whole).
// glue: leaf case of mergeConfigItems
fn merge_leaf(dst: ValMut<'_>, src: &ValRef<'_>, field_path: &str) -> (Vec<String>, Vec<String>) {
    let equal = {
        let d = val_ref(&dst);
        reflect::leaf_equal(&d, src)
    };
    if equal {
        return (Vec::new(), Vec::new());
    }
    if dynamicConfigItems.contains(&field_path) {
        reflect::leaf_set(dst, src);
        return (vec![field_path.to_owned()], Vec::new());
    }
    (Vec::new(), vec![field_path.to_owned()])
}

/// A shared view of a mutable field.
// glue: reborrow of ValMut as ValRef
fn val_ref<'a>(v: &'a ValMut<'_>) -> ValRef<'a> {
    match v {
        ValMut::Bool(x) => ValRef::Bool(x),
        ValMut::Int(x) => ValRef::Int(x),
        ValMut::I32(x) => ValRef::I32(x),
        ValMut::I64(x) => ValRef::I64(x),
        ValMut::Uint(x) => ValRef::Uint(x),
        ValMut::U32(x) => ValRef::U32(x),
        ValMut::U64(x) => ValRef::U64(x),
        ValMut::F64(x) => ValRef::F64(x),
        ValMut::Str(x) => ValRef::Str(x),
        ValMut::Duration(x) => ValRef::Duration(x),
        ValMut::StrSlice(x) => ValRef::StrSlice(x),
        ValMut::StrMap(x) => ValRef::StrMap(x),
        ValMut::NullableBool(x) => ValRef::NullableBool(x),
        ValMut::AtomicBool(x) => ValRef::AtomicBool(x),
        ValMut::Struct(x) => ValRef::Struct(&**x),
    }
}

/// ConfReloadFunc is used to reload the config to make it work.
pub type ConfReloadFunc = Box<dyn Fn(&Config, &Config) + Send + Sync>;

/// FlattenConfigItems flatten this config, see more cases in the test.
// go: FlattenConfigItems L1
pub fn flatten_config_items(
    nested_config: &BTreeMap<String, json::Value>,
) -> BTreeMap<String, json::Value> {
    let mut flat_map = BTreeMap::new();
    flatten(
        &mut flat_map,
        &json::Value::Object(nested_config.clone()),
        "",
    );
    flat_map
}

// go: flatten L1
fn flatten(flat_map: &mut BTreeMap<String, json::Value>, nested: &json::Value, prefix: &str) {
    match nested {
        json::Value::Object(nested) => {
            for (k, v) in nested {
                let mut path = k.clone();
                if !prefix.is_empty() {
                    path = format!("{prefix}.{k}");
                }
                flatten(flat_map, v, &path);
            }
        }
        _ => {
            // don't flatten arrays
            flat_map.insert(prefix.to_owned(), nested.clone());
        }
    }
}

/// GetTxnScopeFromConfig extracts @@txn_scope value from the config.
// go: GetTxnScopeFromConfig L1
pub fn get_txn_scope_from_config() -> String {
    // tikvcfg.GetTxnScopeFromConfig (the "injectTxnScope" failpoint is not ported).
    let txn_scope = super::tikv_get_global_config().txn_scope.clone();
    if !txn_scope.is_empty() {
        return txn_scope;
    }
    // oracle.GlobalTxnScope
    "global".to_owned()
}
