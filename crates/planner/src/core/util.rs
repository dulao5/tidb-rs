//! go-file: pkg/planner/core/util.go
//!
//! The VS1-10 part: the embedded schema producers of the non-logical plans
//! (`baseSchemaProducer` for the common plans, `physicalSchemaProducer` for the physical
//! operators that own a schema) and small helpers.
//!
//! - `physicalSchemaProducer.Schema()` initialises a nil schema from the only child on first
//!   read (a getter with a side effect): a `OnceLock` keeps the read path `&self` and the node
//!   `Sync` (N1 §3.7). `SetSchema(nil)` resets it.
//! - The context arguments of the clone helpers are dropped (`M-CTX`).
//! - `EncodeUniqueIndexKey` / `EncodeUniqueIndexValuesForKey` (VS1-11, by decision: the
//!   unique-index key of a point get, D12.1 spec §5) take the statement context, SQL mode and
//!   connection ID instead of the session (`M-CTX`).

use std::collections::BTreeSet;
use std::sync::OnceLock;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::mysql::{SQLMode, TypeEnum, TypeString, TypeVarString, TypeVarchar};
use tidb_codec::encode_key;
use tidb_expression::{Schema, new_schema};
use tidb_infoschema::table::column::cast_value_with_stmt_ctx;
use tidb_kv::error::ErrNotExist;
use tidb_model::{IndexInfo, TableInfo};
use tidb_parser::model::CIStr;
use tidb_sessionctx::stmtctx::StatementContext;
use tidb_sessionctx::variable::session::SessionVars;
use tidb_tablecodec::tablecodec;
use tidb_types::datum::{Datum, KindBinaryLiteral, KindBytes, KindString};
use tidb_types::enum_type::parse_enum_name;
use tidb_types::errors as terr;
use tidb_types::field_name::NameSlice;

use crate::baseimpl::Plan;
use crate::physicalop::{BasePhysicalPlan, PhysicalPlanRef};

/// `size.SizeOfPointer`.
const SIZE_OF_POINTER: i64 = 8;
/// `size.SizeOfSlice`.
const SIZE_OF_SLICE: i64 = 24;

/// physicalSchemaProducer stores the schema for the physical plans who can produce schema directly.
#[derive(Clone, Debug, Default)]
pub struct PhysicalSchemaProducer {
    schema: OnceLock<Schema>,
    /// The embedded `physicalop.BasePhysicalPlan`.
    pub base: BasePhysicalPlan,
}

impl PhysicalSchemaProducer {
    /// A producer embedding `base`, with Go's nil schema.
    // glue: N1 (struct literal)
    pub fn new(base: BasePhysicalPlan) -> PhysicalSchemaProducer {
        PhysicalSchemaProducer {
            schema: OnceLock::new(),
            base,
        }
    }

    // go: physicalSchemaProducer.cloneForPlanCacheWithSelf L1 [M-SELF, M-CTX]
    pub fn clone_for_plan_cache_with_self(&self) -> Option<PhysicalSchemaProducer> {
        let schema = self.schema().clone_schema();
        let base = self.base.clone_for_plan_cache_with_self()?;
        Some(PhysicalSchemaProducer {
            schema: OnceLock::from(schema),
            base,
        })
    }

    // go: physicalSchemaProducer.cloneWithSelf L1 [M-SELF, M-CTX]
    pub fn clone_with_self(&self) -> Result<PhysicalSchemaProducer, Error> {
        let base = self.base.clone_with_self()?;
        Ok(PhysicalSchemaProducer {
            base,
            schema: OnceLock::from(self.schema().clone_schema()),
        })
    }

    /// Schema implements the Plan.Schema interface.
    // go: physicalSchemaProducer.Schema L1
    pub fn schema(&self) -> &Schema {
        self.schema.get_or_init(|| {
            if self.base.children().len() == 1 {
                // default implementation for plans has only one child: proprgate child schema.
                // multi-children plans are likely to have particular implementation.
                self.base.children()[0].schema().clone_schema()
            } else {
                new_schema(Vec::new())
            }
        })
    }

    /// Go's writes through `p.schema` (`p.schema.Columns[i] = ...`, `column.Index = i`):
    /// the schema for writing, initialized as [`PhysicalSchemaProducer::schema`] first.
    // glue: M-SELF (the mutable side of the lazily initialized schema)
    pub fn schema_mut(&mut self) -> &mut Schema {
        self.schema();
        self.schema.get_mut().expect("initialized by schema()")
    }

    /// SetSchema implements the Plan.SetSchema interface. `None` is Go's nil.
    // go: physicalSchemaProducer.SetSchema L1
    pub fn set_schema(&mut self, schema: Option<Schema>) {
        self.schema = match schema {
            Some(s) => OnceLock::from(s),
            None => OnceLock::new(),
        };
    }

    /// MemoryUsage return the memory usage of physicalSchemaProducer
    // go: physicalSchemaProducer.MemoryUsage L1
    pub fn memory_usage(&self) -> i64 {
        self.base.memory_usage() + SIZE_OF_POINTER
    }
}

/// baseSchemaProducer stores the schema for the base plans who can produce schema directly.
#[derive(Clone, Debug, Default)]
pub struct BaseSchemaProducer {
    schema: Option<Schema>,
    names: NameSlice,
    /// The embedded `baseimpl.Plan`.
    pub plan: Plan,
}

impl BaseSchemaProducer {
    /// A producer embedding `plan`, with Go's nil schema and names.
    // glue: N1 (struct literal)
    pub fn new(plan: Plan) -> BaseSchemaProducer {
        BaseSchemaProducer {
            plan,
            ..Default::default()
        }
    }

    /// CloneWithNewCtx clones the baseSchemaProducer with new context.
    // go: baseSchemaProducer.CloneWithNewCtx L1 [M-CTX]
    pub fn clone_with_new_ctx(&self) -> BaseSchemaProducer {
        BaseSchemaProducer {
            plan: self.plan.clone_with_new_ctx(),
            schema: self.schema.as_ref().map(Schema::clone_schema),
            names: self.names.clone(),
        }
    }

    /// OutputNames returns the outputting names of each column.
    // go: baseSchemaProducer.OutputNames L1
    pub fn output_names(&self) -> &NameSlice {
        &self.names
    }

    /// `SetOutputNames`.
    // go: baseSchemaProducer.SetOutputNames L1
    pub fn set_output_names(&mut self, names: NameSlice) {
        self.names = names;
    }

    /// Schema implements the Plan.Schema interface. Go initialises a nil schema on read;
    /// the read takes `&mut self` here.
    // go: baseSchemaProducer.Schema L1
    pub fn schema(&mut self) -> &Schema {
        self.schema.get_or_insert_with(|| new_schema(Vec::new()))
    }

    /// SetSchema implements the Plan.SetSchema interface. `None` is Go's nil.
    // go: baseSchemaProducer.SetSchema L1
    pub fn set_schema(&mut self, schema: Option<Schema>) {
        self.schema = schema;
    }

    // go: baseSchemaProducer.setSchemaAndNames L1
    pub fn set_schema_and_names(&mut self, schema: Option<Schema>, names: NameSlice) {
        self.schema = schema;
        self.names = names;
    }

    /// MemoryUsage return the memory usage of baseSchemaProducer
    // go: baseSchemaProducer.MemoryUsage L1
    pub fn memory_usage(&self) -> i64 {
        let mut sum = SIZE_OF_POINTER
            + SIZE_OF_SLICE
            + self.names.capacity() as i64 * SIZE_OF_POINTER
            + self.plan.memory_usage();
        if let Some(schema) = &self.schema {
            sum += schema.memory_usage();
        }
        for name in &self.names {
            sum += name.memory_usage();
        }
        sum
    }
}

/// GetStatsInfo gets the statistics info from a physical plan tree.
/// Deprecated: FlattenPhysicalPlan() + GetStatsInfoFromFlatPlan() is preferred.
///
/// It reads `Insert` / `Update` / `Delete` plans and `CollectPlanStatsVersion` (D10.9).
// glue: stub of GetStatsInfo, blocked-by:D10.9 (CollectPlanStatsVersion, the DML plans)
pub fn get_stats_info() -> Result<(), Error> {
    Err(blocked("D10.9", "GetStatsInfo (CollectPlanStatsVersion)"))
}

/// extractStringFromStringSet helps extract string info from set.StringSet.
// go: extractStringFromStringSet L1
pub fn extract_string_from_string_set(set: &BTreeSet<String>) -> String {
    if set.is_empty() {
        return String::new();
    }
    let mut l: Vec<String> = set.iter().map(|k| format!("\"{k}\"")).collect();
    l.sort();
    l.join(",")
}

/// extractStringFromStringSlice helps extract string info from []string.
///
/// Go sorts the caller's slice in place.
// go: extractStringFromStringSlice L1
pub fn extract_string_from_string_slice(ss: &mut [String]) -> String {
    if ss.is_empty() {
        return String::new();
    }
    ss.sort();
    ss.join(",")
}

/// extractStringFromUint64Slice helps extract string info from uint64 slice.
// go: extractStringFromUint64Slice L1
pub fn extract_string_from_uint64_slice(slice: &[u64]) -> String {
    if slice.is_empty() {
        return String::new();
    }
    let mut l: Vec<String> = slice.iter().map(|k| k.to_string()).collect();
    l.sort();
    l.join(",")
}

/// extractStringFromBoolSlice helps extract string info from bool slice.
// go: extractStringFromBoolSlice L1
pub fn extract_string_from_bool_slice(slice: &[bool]) -> String {
    if slice.is_empty() {
        return String::new();
    }
    let mut l: Vec<String> = slice.iter().map(|k| k.to_string()).collect();
    l.sort();
    l.join(",")
}

// go: clonePhysicalPlan L1 [M-CTX]
pub fn clone_physical_plan(plans: &[PhysicalPlanRef]) -> Result<Vec<PhysicalPlanRef>, Error> {
    let mut cloned = Vec::with_capacity(plans.len());
    for p in plans {
        let c = p.clone_plan()?;
        cloned.push(c);
    }
    Ok(cloned)
}

// go: getLowerDB L1
pub fn get_lower_db(db_name: &CIStr, vars: &SessionVars) -> String {
    let mut db_name_l = db_name.l.clone();
    if db_name_l.is_empty() {
        db_name_l = tidb_base::gostd::to_lower(&vars.current_db);
    }
    db_name_l
}

/// EncodeUniqueIndexKey encodes a unique index key.
///
/// Go takes the session (`sessionctx.Context`) and reads its statement context, SQL mode and
/// connection ID (`table.CastValue`); the point get executor calls it from `Next`, where it
/// holds those rather than the session (N2), so they are the arguments (`M-CTX`). Go casts
/// `idxVals` in place (the executor's slice is the plan's); `idx_vals` is changed in place
/// too.
// go: EncodeUniqueIndexKey L1 [M-CTX]
pub fn encode_unique_index_key(
    sc: &StatementContext,
    sql_mode: SQLMode,
    connection_id: u64,
    tbl_info: &TableInfo,
    idx_info: &IndexInfo,
    idx_vals: &mut [Datum],
    t_id: i64,
) -> Result<Vec<u8>, Error> {
    let encoded_idx_vals = encode_unique_index_values_for_key(
        sc,
        sql_mode,
        connection_id,
        tbl_info,
        idx_info,
        idx_vals,
    )?;
    Ok(tablecodec::encode_index_seek_key(t_id, idx_info.id, &encoded_idx_vals).0)
}

/// EncodeUniqueIndexValuesForKey encodes unique index values for a key.
///
/// The context arguments are those of [`encode_unique_index_key`].
// go: EncodeUniqueIndexValuesForKey L1 [M-CTX, M-ERRVAL]
pub fn encode_unique_index_values_for_key(
    sc: &StatementContext,
    sql_mode: SQLMode,
    connection_id: u64,
    tbl_info: &TableInfo,
    idx_info: &IndexInfo,
    idx_vals: &mut [Datum],
) -> Result<Vec<u8>, Error> {
    let columns = tbl_info.columns.as_deref().unwrap_or(&[]);
    let idx_columns = idx_info.columns.as_deref().unwrap_or(&[]);
    for i in 0..idx_vals.len() {
        let col_info = &columns[idx_columns[i].offset as usize];
        let tp = col_info.get_type();
        // table.CastValue will append 0x0 if the string value's length is smaller than the BINARY column's length.
        // So we don't use CastValue for string value for now.
        // TODO: The first if branch should have been removed, because the functionality of set the collation of the datum
        // have been moved to util/ranger (normal path) and getNameValuePairs/getPointGetValue (fast path). But this change
        // will be cherry-picked to a hotfix, so we choose to be a bit conservative and keep this for now.
        let err: Result<(), Error> = if tp == TypeString || tp == TypeVarString || tp == TypeVarchar
        {
            // Go sets the string ToString returned (empty on an error) before checking it.
            let (s, e) = match idx_vals[i].to_string() {
                Ok(s) => (s, Ok(())),
                Err(e) => (Vec::new(), Err(e)),
            };
            let collation = idx_vals[i].collation().to_owned();
            idx_vals[i].set_string(s, collation);
            e
        } else if tp == TypeEnum
            && (idx_vals[i].kind() == KindString
                || idx_vals[i].kind() == KindBytes
                || idx_vals[i].kind() == KindBinaryLiteral)
        {
            let Ok(s) = idx_vals[i].to_string() else {
                return Err(ErrNotExist.clone());
            };
            let Ok(e) = parse_enum_name(
                col_info.field_type.get_elems().unwrap_or(&[]),
                &s,
                col_info.field_type.get_collate(),
            ) else {
                return Err(ErrNotExist.clone());
            };
            idx_vals[i].set_mysql_enum(e, col_info.field_type.get_collate());
            Ok(())
        } else {
            // If a truncated error or an overflow error is thrown when converting the type of `idxVal[i]` to
            // the type of `colInfo`, the `idxVal` does not exist in the `idxInfo` for sure.
            let (casted, e) = cast_value_with_stmt_ctx(
                sc,
                sql_mode,
                connection_id,
                std::mem::take(&mut idx_vals[i]),
                col_info,
                true,
                false,
            );
            idx_vals[i] = casted;
            if let Err(e) = &e
                && (terr::ErrOverflow.equal(e)
                    || terr::ErrDataTooLong.equal(e)
                    || terr::ErrTruncated.equal(e)
                    || terr::ErrTruncatedWrongVal.equal(e))
            {
                return Err(ErrNotExist.clone());
            }
            e
        };
        err?;
    }

    let mut encoded_idx_vals = Vec::new();
    if let Err(err) = encode_key(&sc.time_zone(), &mut encoded_idx_vals, idx_vals) {
        sc.handle_error(err)?;
    }
    Ok(encoded_idx_vals)
}

/// tableHasDirtyContent: whether the transaction wrote the table (or a partition of it).
/// The session's dirty-table check is handed over through the planning context; without
/// it this is a boundary stub (`blocked-by:D09.13`, the optimizer driver that sets up the
/// planning context, VS1-24).
// go: tableHasDirtyContent L2 (the session's dirty check comes through PlanCtx)
pub fn table_has_dirty_content(
    ctx: &crate::planctx::PlanCtx<'_>,
    table_info: &tidb_model::TableInfo,
) -> Result<bool, Error> {
    let Some(has_dirty_content) = &ctx.has_dirty_content else {
        // glue: blocked-by:D09.13 (the session's HasDirtyContent, handed over with the planning context)
        return Err(blocked(
            "D09.13",
            "the transaction's dirty tables of the planning context (HasDirtyContent)",
        ));
    };
    let Some(pi) = table_info.get_partition_info() else {
        return Ok(has_dirty_content(table_info.id));
    };
    // Currently, we add UnionScan on every partition even though only one partition's data is changed.
    // This is limited by current implementation of Partition Prune. It'll be updated once we modify that part.
    for partition in pi.definitions.iter().flatten() {
        if has_dirty_content(partition.id) {
            return Ok(true);
        }
    }
    Ok(false)
}
