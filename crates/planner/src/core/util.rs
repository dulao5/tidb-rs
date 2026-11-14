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

use std::collections::BTreeSet;
use std::sync::OnceLock;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_expression::{Schema, new_schema};
use tidb_parser::model::CIStr;
use tidb_sessionctx::variable::session::SessionVars;
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
