//! go-file: pkg/planner/core/plan_cache_utils.go
//!
//! The prepared statement object of the plan cache (block D13.1): `PlanCacheStmt` and
//! `GeneratePlanCacheStmtWithAST`, which PREPARE builds (prepared-statement planning, VS1-15).
//! The cache itself (`NewPlanCacheKey`, `PlanCacheValue`, the LRU and instance caches) is the
//! plan-cache exception A3, whose spec is not confirmed: nothing here caches a plan.
//!
//! Mapping:
//! - The AST is not shared and not changed by the plan cache: the statement is owned by the
//!   `PlanCacheStmt` (`Prepared::stmt`, boxed so the resolve context's keys, which are node
//!   addresses, stay valid), and the `PlanCacheStmt` sits behind a mutex shared by the
//!   session's statement map and the statements that execute it ([`PlanCacheStmtRef`]).
//! - Go's `Params []ast.ParamMarkerExpr` point at the markers, and EXECUTE writes the values
//!   into them (`param.Datum = val`). The Rust markers are not written: the values go to
//!   `SessionVars.PlanCacheParams` only, by the marker's order, which is where the expression
//!   rewriter reads them (D09.2). `params` keeps the markers' offsets in order.
//! - `infoschema.InfoSchema` is the narrow [`InfoSchemaLookup`] (the full `InfoSchema`
//!   interface is D06.3 part 2, task T9). Its table lookups are synchronous and read no
//!   TiKV: a table not in the InfoSchema's table cache is a boundary stub, and the session
//!   loads the statement's tables before planning (D06.3 spec §11.2).

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::dbterror::plannererrors::{
    ErrPrepareDDL, ErrPsManyParam, ErrStmtNotFound, ErrUnsupportedPs,
};
use tidb_infoschema::InfoSchemaV2;
use tidb_model::TableInfo;
use tidb_parser::ast::{ExecuteStmt, Stmt};
use tidb_parser::model::CIStr;
use tidb_sessionctx::variable::session::SessionVars;

use crate::core::plan_cacheable_checker::{CNode, Visit, Walker, is_ast_cacheable};
use crate::core::planbuilder::new_plan_builder;
use crate::core::preprocess::{
    InfoSchemaHandle, PreprocessOpt, PreprocessorReturn, in_prepare, preprocess,
    with_domain_info_schema, with_preprocessor_return,
};
use crate::core::resolve::{ContextRef, new_node_w};
use crate::planctx::PlanCtx;
use crate::util::fixcontrol;

/// The `infoschema.InfoSchema` methods the plan cache calls (see the module doc).
pub trait InfoSchemaLookup: Send + Sync {
    /// `TableByName(ctx, schema, table)`: the table's meta (Go's `table.Table.Meta()`).
    // glue: interface method infoschema.InfoSchema.TableByName
    fn table_by_name(&self, schema: &CIStr, table: &CIStr) -> Result<Arc<TableInfo>, Error>;
    /// `TableByID(ctx, id)`.
    // glue: interface method infoschema.InfoSchema.TableByID
    fn table_by_id(&self, id: i64) -> Result<Option<Arc<TableInfo>>, Error>;
    /// `SchemaByID(id)`: the database's name.
    // glue: interface method infoschema.InfoSchema.SchemaByID
    fn schema_name_by_id(&self, id: i64) -> Option<CIStr>;
    /// `SchemaByID(id)`: the database (`infoschema.SchemaByTable` looks the table's `DBID`
    /// up with it). None by default: a lookup without databases.
    // glue: interface method infoschema.InfoSchema.SchemaByID
    fn schema_by_id(&self, id: i64) -> Option<Arc<tidb_model::DBInfo>> {
        let _ = id;
        None
    }
    /// `SchemaMetaVersion()`.
    // glue: interface method infoschema.InfoSchema.SchemaMetaVersion
    fn schema_meta_version(&self) -> i64;
    /// The InfoSchema as a shared value (Go copies the interface value into
    /// `PreprocessorReturn.InfoSchema`).
    // glue: Go interface value copy
    fn snapshot(&self) -> Arc<dyn InfoSchemaLookup>;
}

/// The error of a table that is in the InfoSchema but not in its table cache: loading it
/// reads TiKV, which the synchronous planner does not do (D06.3 spec §11.2).
// glue: the cache miss of a synchronous table lookup
fn not_resident() -> Error {
    // glue: blocked-by:D06.3 (a table the session did not load before planning)
    blocked(
        "D06.3",
        "a table not loaded into the InfoSchema cache before planning",
    )
}

/// The new session's InfoSchema (D06.3): the lookups without TiKV reads (see the module doc).
impl InfoSchemaLookup for InfoSchemaV2 {
    // glue: interface method infoschema.InfoSchema.TableByName (infoschemaV2.TableByName without the TiKV load)
    fn table_by_name(&self, schema: &CIStr, table: &CIStr) -> Result<Arc<TableInfo>, Error> {
        match self.table_by_name_resident(schema, table)? {
            Some(t) => Ok(Arc::clone(t.meta())),
            None => Err(not_resident()),
        }
    }
    // glue: interface method infoschema.InfoSchema.TableByID (infoschemaV2.TableByID without the TiKV load)
    fn table_by_id(&self, id: i64) -> Result<Option<Arc<TableInfo>>, Error> {
        match self.table_by_id_resident(id) {
            Ok(t) => Ok(t.map(|t| Arc::clone(t.meta()))),
            Err(()) => Err(not_resident()),
        }
    }
    // glue: interface method infoschema.InfoSchema.SchemaByID
    fn schema_name_by_id(&self, id: i64) -> Option<CIStr> {
        InfoSchemaV2::schema_by_id(self, id).map(|db| db.name.clone())
    }
    // glue: interface method infoschema.InfoSchema.SchemaByID
    fn schema_by_id(&self, id: i64) -> Option<Arc<tidb_model::DBInfo>> {
        InfoSchemaV2::schema_by_id(self, id)
    }
    // glue: interface method infoschema.InfoSchema.SchemaMetaVersion
    fn schema_meta_version(&self) -> i64 {
        InfoSchemaV2::schema_meta_version(self)
    }
    // glue: Go interface value copy (the version's Arcs are shared, N13)
    fn snapshot(&self) -> Arc<dyn InfoSchemaLookup> {
        Arc::new(self.clone())
    }
}

/// `ast.Prepared`: the prepared statement and its label.
#[derive(Debug)]
pub struct Prepared {
    pub stmt: Box<Stmt>,
    /// `StmtType`: `stmtctx.GetStmtLabel`.
    pub stmt_type: String,
}

/// PointGetExecutorCache caches the PointGetExecutor to further improve its performance.
/// Don't forget to reset this executor when the prior plan is invalid.
///
/// The executor is the executor crate's (above this one), kept as `Any`.
#[derive(Default)]
pub struct PointGetExecutorCache {
    /// `ColumnInfos any`: the column infos of the cached executor.
    pub column_infos: Option<Box<dyn Any + Send>>,
    /// Executor is only used for point get scene.
    /// Notice that we should only cache the PointGetExecutor that have a snapshot with
    /// MaxTS in it. If the current plan is not PointGet or does not use MaxTS optimization,
    /// this value should be nil here.
    pub executor: Option<Box<dyn Any + Send>>,
}

/// PlanCacheStmt store prepared ast from PrepareExec and other related fields
///
/// Not ported (with the reason): `VisitInfos` (the privilege check, D04.1),
/// `BindingInfo` (bindings, D13.2), `SnapshotTSEvaluator` (stale read, D05.4: the
/// preprocessor returns none), `tables` (the table statistics of the plan cache key, A3),
/// `NormalizedPlan` / `PlanDigest` (written when a plan is put into the cache, A3).
pub struct PlanCacheStmt {
    pub prepared_ast: Prepared,
    pub resolve_ctx: ContextRef,
    /// which DB the statement will be processed over
    pub stmt_db: String,
    /// The offsets of the parameter markers, in order (see the module doc).
    pub params: Vec<usize>,

    pub point_get: PointGetExecutorCache,

    /// below fields are for PointGet short path
    pub schema_version: i64,

    /// RelateVersion stores the true cache plan table schema version, since each table
    /// schema can be updated separately in transaction.
    pub relate_version: HashMap<i64, u64>,

    /// Whether this stmt is cacheable.
    pub stmt_cacheable: bool,
    /// Why this stmt is uncacheable.
    pub uncacheable_reason: String,

    /// `len(limits)`: the number of LIMIT clauses (planCacheStmtProcessor).
    pub limits: usize,
    pub has_subquery: bool,

    pub normalized_sql: String,
    /// `SQLDigest`.
    pub sql_digest: String,
    pub for_update_read: bool,

    /// the different between NormalizedSQL, NormalizedSQL4PC and StmtText: see Go.
    pub stmt_text: String,

    /// dbName and tbls are used to add metadata lock.
    pub db_name: Vec<CIStr>,
    pub tbls: Vec<Arc<TableInfo>>,
}

impl Default for PlanCacheStmt {
    /// Go's zero `PlanCacheStmt{}` (tests build one to hold a cached executor): an empty
    /// statement.
    // glue: Go zero value
    fn default() -> Self {
        PlanCacheStmt {
            prepared_ast: Prepared {
                stmt: Box::new(Stmt::Unported(Box::new(tidb_parser::Node {
                    kind: "",
                    span: tidb_parser::Span::default(),
                    children: Vec::new(),
                }))),
                stmt_type: String::new(),
            },
            resolve_ctx: Arc::new(parking_lot::Mutex::new(crate::core::resolve::new_context())),
            stmt_db: String::new(),
            params: Vec::new(),
            point_get: PointGetExecutorCache::default(),
            schema_version: 0,
            relate_version: HashMap::new(),
            stmt_cacheable: false,
            uncacheable_reason: String::new(),
            limits: 0,
            has_subquery: false,
            normalized_sql: String::new(),
            sql_digest: String::new(),
            for_update_read: false,
            stmt_text: String::new(),
            db_name: Vec::new(),
            tbls: Vec::new(),
        }
    }
}

/// A prepared statement shared by the session's statement map (`SessionVars.PreparedStmts`)
/// and the statements that execute it.
pub type PlanCacheStmtRef = Arc<parking_lot::Mutex<PlanCacheStmt>>;

/// paramMarkerExtractor extracts all the param markers in the AST (by `Leave` order).
#[derive(Default)]
struct ParamMarkerExtractor {
    /// The markers' offsets.
    markers: Vec<usize>,
    err: Option<Error>,
}

impl Visit for ParamMarkerExtractor {
    // go: paramMarkerExtractor.Enter L1
    fn enter(&mut self, _: CNode<'_>) -> bool {
        false
    }
    // go: paramMarkerExtractor.Leave L1
    fn leave(&mut self, n: CNode<'_>) -> bool {
        if let CNode::ParamMarker(x) = n {
            self.markers.push(x.offset);
        }
        true
    }
    // glue: a node not in the Rust AST may hold markers
    fn fail(&mut self, e: Error) {
        self.err.get_or_insert(e);
    }
}

/// Collects the table names of a statement (CTE names included: their lookup simply fails).
#[derive(Default)]
struct TableNameCollector {
    names: Vec<(String, String)>,
}

impl Visit for TableNameCollector {
    // glue: ast.Visitor (statement_table_names)
    fn enter(&mut self, n: CNode<'_>) -> bool {
        if let CNode::TableName(t) = n {
            self.names.push((t.schema.o.clone(), t.name.o.clone()));
        }
        false
    }
    // glue: ast.Visitor (statement_table_names)
    fn leave(&mut self, _: CNode<'_>) -> bool {
        true
    }
    // glue: ast.Visitor (statement_table_names: the rest of the walk is skipped)
    fn fail(&mut self, _: Error) {}
}

/// The `(schema, table)` names a statement refers to, the schema empty where the statement
/// gives none. The session loads these tables into the InfoSchema's table cache before the
/// synchronous planner looks them up (D06.3 spec §11.2).
// glue: the tables to load before planning (D06.3 spec §11.2)
pub fn statement_table_names(stmt: &Stmt) -> Vec<(String, String)> {
    let mut c = TableNameCollector::default();
    Walker(&mut c).stmt(stmt);
    c.names
}

/// GeneratePlanCacheStmtWithAST generates the PlanCacheStmt structure for this AST.
/// paramSQL is the corresponding parameterized sql like 'select * from t where a<? and b>?'.
/// paramStmt is the Node of paramSQL.
///
/// Returns the statement, what PREPARE reads of the plan `PlanBuilder.Build` returns (Go's
/// `base.Plan`: its schema and output names, [`BuiltPlanInfo`]) and the parameter count.
/// The plan is built in the statement's planning context `ctx`, built by the caller from the
/// session's parts like `planner.Optimize`'s ([`crate::planctx::SessionPlanCtx`]).
///
/// Differences (all boundary stubs, see the spec): the statement label is `"Select"` for a
/// SELECT and `blocked-by:D12.0` otherwise; `parser.NormalizeDigest` is not ported (D02.2)
/// and leaves the normalized SQL and digest empty (they feed Top SQL, the statement summary
/// and the plan cache key, none of which the new stack has); `planCacheStmtProcessor` is
/// ported except the table list it keeps for the plan cache key (A3); `checkPreparedPriv`
/// has no privilege manager (D04.1) and no table locks (write path).
// go: GeneratePlanCacheStmtWithAST L2 (label / NormalizeDigest / privileges: see the doc comment)
pub fn generate_plan_cache_stmt_with_ast(
    ctx: &mut PlanCtx<'_>,
    is_prep_stmt: bool,
    param_sql: &str,
    param_stmt: Stmt,
    is: &dyn InfoSchemaLookup,
    domain_is: Option<&dyn InfoSchemaLookup>,
) -> Result<(PlanCacheStmt, Option<BuiltPlanInfo>, usize), Error> {
    let vars = &mut *ctx.vars;
    let mut extractor = ParamMarkerExtractor::default();
    Walker(&mut extractor).stmt(&param_stmt);
    if let Some(e) = extractor.err {
        return Err(e);
    }

    // DDL Statements can not accept parameters: no DDL in the Rust AST.
    let _ = &ErrPrepareDDL;

    match &param_stmt {
        Stmt::Select(stmt) => {
            if stmt.select_into_opt.is_some() {
                return Err(ErrUnsupportedPs.clone());
            }
        }
        Stmt::SetOpr(_) => {}
        // ImportInto, LoadData, Prepare, Execute, Deallocate, NonTransactionalDML (all
        // ErrUnsupportedPs) and the DDL check above need statements the Rust AST does not
        // have.
        // glue: blocked-by:D02.2 (statements other than SELECT in the Rust AST)
        _ => return Err(blocked("D02.2", "preparing statements other than SELECT")),
    }

    // Prepare parameters should NOT over 2 bytes(MaxUint16)
    // https://dev.mysql.com/doc/internals/en/com-stmt-prepare-response.html#packet-COM_STMT_PREPARE_OK.
    if extractor.markers.len() > u16::MAX as usize {
        return Err(ErrPsManyParam.clone());
    }

    let mut stmt = Box::new(param_stmt);
    let ret = PreprocessorReturn {
        info_schema: Some(InfoSchemaHandle(is.snapshot())),
        ..Default::default()
    };
    let resolve_ctx = {
        let mut node_w = new_node_w(&mut stmt);
        let mut opts: Vec<PreprocessOpt> =
            vec![Box::new(in_prepare), with_preprocessor_return(ret)];
        // domain.GetDomain(sctx), for the metadata lock (preprocess module doc).
        if let Some(d) = domain_is {
            opts.push(with_domain_info_schema(d.snapshot()));
        }
        let (_, res) = preprocess(vars, &mut node_w, opts);
        res?;
        node_w.get_resolve_context()
    };

    // The parameter markers are appended in visiting order, which may not
    // be the same as the position order in the query string. We need to
    // sort it by position.
    extractor.markers.sort();
    let param_count = extractor.markers.len();
    // extractor.markers[i].SetOrder(i): the Rust parser numbers the markers by position
    // already (`ParamMarkerExpr.order`).

    let prepared = Prepared {
        stmt_type: get_stmt_label(&stmt)?,
        stmt,
    };
    // parser.NormalizeDigest(prepared.Stmt.Text())
    // glue: stub, blocked-by:D02.2 (parser.NormalizeDigest: the normalized SQL feeds Top SQL, the statement summary and the plan cache key, none on the new stack)
    let (normalized_sql, digest) = (String::new(), String::new());

    let mut cacheable;
    let mut reason;
    if is_prep_stmt && !vars.enable_prepared_plan_cache
        || !is_prep_stmt && !vars.enable_non_prepared_plan_cache
    {
        cacheable = false;
        reason = "plan cache is disabled".to_owned();
    } else {
        if is_prep_stmt {
            (cacheable, reason) = is_ast_cacheable(vars, &prepared.stmt, Some(is))?;
        } else {
            cacheable = true; // it is already checked here
            reason = String::new();
        }

        if !cacheable
            && fixcontrol::get_bool_with_default(
                &vars.state.optimizer_fix_control,
                fixcontrol::Fix49736,
                false,
            )
        {
            vars.stmt_ctx().append_warning(Error::plain(format!(
                "force plan-cache: may use risky cached plan: {reason}"
            )));
            cacheable = true;
            reason = String::new();
        }

        if !cacheable {
            vars.stmt_ctx()
                .append_warning(Error::plain(format!("skip prepared plan-cache: {reason}")));
        }
    }

    // For prepared statements like `prepare st from 'select * from t where a<?'`,
    // parameters are unknown here, so regard them all as NULL: the Rust markers carry no
    // value (see the module doc), and the parameter list is reset by EXECUTE.

    let mut dest_builder = new_plan_builder(&[]);
    let _ = dest_builder.init(vars, Some(InfoSchemaHandle(is.snapshot())), None);
    // The Rust AST keeps byte spans into the text it was parsed from (Go: ast.Node.Text).
    dest_builder.sql_text = param_sql;
    let built = dest_builder.build(ctx, &prepared.stmt)?;
    let p = Some(BuiltPlanInfo {
        schema: ctx.arena.get(built).schema(&ctx.arena).clone(),
        output_names: ctx.arena.get(built).output_names(&ctx.arena).clone(),
    });
    let opt_flag = dest_builder.get_opt_flag();
    let for_update_read = dest_builder.get_is_for_update_read();
    drop(dest_builder);
    let vars = &mut *ctx.vars;

    if cacheable && opt_flag & FLAG_PARTITION_PROCESSOR > 0 {
        // dynamic prune mode is not used, could be that global statistics not yet available!
        cacheable = false;
        reason = "static partition prune mode used".to_owned();
        vars.stmt_ctx()
            .append_warning(Error::plain(format!("skip prepared plan-cache: {reason}")));
    }

    // Collect information for metadata lock.
    let related: Vec<i64> = vars.stmt_ctx().related_table_ids.iter().copied().collect();
    let mut db_name = Vec::with_capacity(related.len());
    let mut tbls = Vec::with_capacity(related.len());
    let mut relate_version = HashMap::with_capacity(related.len());
    for id in related {
        let Some(tbl) = is.table_by_id(id)? else {
            log::error!("table not found in info schema, tableID: {id}");
            return Err(tidb_base::errors::new("table not found in info schema"));
        };
        let Some(db) = is.schema_name_by_id(tbl.dbid) else {
            log::error!("database not found in info schema, dbID: {}", tbl.dbid);
            return Err(tidb_base::errors::new("database not found in info schema"));
        };
        db_name.push(db);
        relate_version.insert(id, tbl.revision);
        tbls.push(tbl);
    }

    let mut prepared_obj = PlanCacheStmt {
        prepared_ast: prepared,
        resolve_ctx,
        stmt_db: vars.current_db.clone(),
        stmt_text: param_sql.to_owned(),
        normalized_sql,
        sql_digest: digest,
        for_update_read,
        stmt_cacheable: cacheable,
        uncacheable_reason: reason,
        db_name,
        tbls,
        schema_version: is.schema_meta_version(),
        relate_version,
        params: extractor.markers,
        point_get: PointGetExecutorCache::default(),
        limits: 0,
        has_subquery: false,
    };

    plan_cache_stmt_process(&mut prepared_obj);

    crate::core::plan_cache::check_prepared_priv(&prepared_obj)?;
    Ok((prepared_obj, p, param_count))
}

/// What PREPARE reads of the plan `PlanBuilder.Build` returns (Go's `base.Plan`): the
/// schema and output names, for the column definitions of the prepare response. The
/// logical plan itself lives in the statement's arena, which ends with PREPARE.
// glue: the parts of base.Plan that PrepareExec reads (the logical plan stays in its arena, N1)
#[derive(Clone, Debug, Default)]
pub struct BuiltPlanInfo {
    /// `p.Schema()`.
    pub schema: tidb_expression::Schema,
    /// `p.OutputNames()`.
    pub output_names: tidb_types::field_name::NameSlice,
}

/// `rule.FlagPartitionProcessor` (the partition pruning rule, D09.12): not set by any rule
/// the new stack runs.
// glue: rule.FlagPartitionProcessor (D09.12)
const FLAG_PARTITION_PROCESSOR: u64 = 1 << 14;

/// `stmtctx.GetStmtLabel(ctx, node)` for the statements the new stack prepares.
// glue: stub, blocked-by:D12.0 (stmtctx.GetStmtLabel: the labels of statements other than SELECT)
fn get_stmt_label(node: &Stmt) -> Result<String, Error> {
    match node {
        Stmt::Select(_) => Ok("Select".to_owned()),
        _ => Err(blocked("D12.0", "stmtctx.GetStmtLabel")),
    }
}

/// `planCacheStmtProcessor` (`paramStmt.Accept(stmtProcessor)`): records the LIMIT clauses
/// (all of them, as Go) and whether there is a subquery, and the tables (for the plan cache key's
/// statistics, A3: not kept).
// glue: paramStmt.Accept(stmtProcessor)
fn plan_cache_stmt_process(stmt: &mut PlanCacheStmt) {
    #[derive(Default)]
    struct P {
        limits: usize,
        has_subquery: bool,
    }
    impl Visit for P {
        // go: planCacheStmtProcessor.Enter L2 (the tables are for the plan cache key, A3)
        fn enter(&mut self, n: CNode<'_>) -> bool {
            match n {
                CNode::Limit(_) => self.limits += 1,
                CNode::Subquery => self.has_subquery = true,
                _ => {}
            }
            false
        }
        // go: planCacheStmtProcessor.Leave L1
        fn leave(&mut self, _: CNode<'_>) -> bool {
            true
        }
        // glue: ast.Visitor
        fn fail(&mut self, _: Error) {}
    }
    let mut p = P::default();
    Walker(&mut p).stmt(&stmt.prepared_ast.stmt);
    stmt.limits = p.limits;
    stmt.has_subquery = p.has_subquery;
}

/// NewPlanCacheKey creates the plan cache key for this statement: the key itself is the plan
/// cache (the plan-cache exception A3). The stand-in returns the statement's database and text
/// as the key, always cacheable: the only user is the deletion of a closed statement's plan
/// from the session's cache, which holds no plans (`cleanRetryInfo`, whose Go behaviour of
/// computing every key from the first closed statement is kept, D13.1 tasks).
// glue: stub of NewPlanCacheKey, blocked-by:D13.1 (the plan cache key is the plan-cache exception A3)
pub fn new_plan_cache_key(stmt: &PlanCacheStmt) -> Result<(String, String, bool, String), Error> {
    Ok((
        format!("{}\u{0}{}", stmt.stmt_db, stmt.stmt_text),
        String::new(),
        true,
        String::new(),
    ))
}

impl PlanCacheStmt {
    /// DBName returns the database names of the tables (for the metadata lock).
    // go: PlanCacheStmt.DBName L1
    pub fn db_name(&self) -> &[CIStr] {
        &self.db_name
    }

    /// Tbls returns the tables (for the metadata lock).
    // go: PlanCacheStmt.Tbls L1
    pub fn tbls(&self) -> &[Arc<TableInfo>] {
        &self.tbls
    }

    /// SetDBNameAndTbls sets the dbName and tbls fields, cloning the input slices.
    // go: PlanCacheStmt.SetDBNameAndTbls L1
    pub fn set_db_name_and_tbls(&mut self, db_name: &[CIStr], tbls: &[Arc<TableInfo>]) {
        self.db_name = db_name.to_vec();
        self.tbls = tbls.to_vec();
    }
}

/// ExtractAndSortParamMarkers extracts ParamMarkerExpr nodes from the AST, sorts them by
/// position and numbers them (the markers' offsets here; the Rust markers carry their order
/// and no value, see the module doc).
// go: ExtractAndSortParamMarkers L2 (offsets: the Rust markers are not written)
pub fn extract_and_sort_param_markers(stmt_node: &Stmt) -> Result<Vec<usize>, Error> {
    let mut extractor = ParamMarkerExtractor::default();
    Walker(&mut extractor).stmt(stmt_node);
    if let Some(e) = extractor.err {
        return Err(e);
    }
    extractor.markers.sort();
    Ok(extractor.markers)
}

/// CollectPlanCacheStmtInfo walks the AST to populate the limits and hasSubquery of the
/// statement (the tables are for the plan cache key, A3).
// go: CollectPlanCacheStmtInfo L2 (the tables are for the plan cache key, A3)
pub fn collect_plan_cache_stmt_info(stmt: &mut PlanCacheStmt) {
    plan_cache_stmt_process(stmt);
}

/// GetPreparedStmt extract the prepared statement from the execute statement.
///
/// Go caches the statement found by name in the node (`stmt.PrepStmt = prepStmt`); the
/// statement node is shared here (`Arc<Stmt>`, read-only), so a later call looks the name up
/// again and finds the same statement (nothing can deallocate it inside one statement). Go's
/// type assertion panics on a value that is not a `PlanCacheStmt`; that is an error here.
// go: GetPreparedStmt L2 (the statement found by name is not written back into the shared node)
pub fn get_prepared_stmt(
    stmt: &ExecuteStmt,
    vars: &SessionVars,
) -> Result<PlanCacheStmtRef, Error> {
    if let Some(prep_stmt) = &stmt.prep_stmt {
        return as_plan_cache_stmt(Arc::clone(prep_stmt));
    }
    if !stmt.name.is_empty() {
        let prep_stmt = vars.get_prepared_stmt_by_name(&stmt.name)?;
        // stmt.PrepStmt = prepStmt: see the doc comment.
        return as_plan_cache_stmt(prep_stmt);
    }
    Err(ErrStmtNotFound.clone())
}

/// `prepStmt.(*PlanCacheStmt)`: the session keeps its statements as `Any`.
// glue: Go type assertion (an error instead of a panic, see get_prepared_stmt)
fn as_plan_cache_stmt(prep_stmt: Arc<dyn Any + Send + Sync>) -> Result<PlanCacheStmtRef, Error> {
    prep_stmt
        .downcast::<parking_lot::Mutex<PlanCacheStmt>>()
        .map_err(|_| tidb_base::errors::new("invalid PlanCacheStmt type".to_owned()))
}

/// `parseParamTypes`: the parameter types of the plan cache key.
// glue: stub of parseParamTypes, blocked-by:D13.1 (the plan cache key is the plan-cache exception A3)
pub fn parse_param_types() -> Result<(), Error> {
    Err(blocked("D13.1", "parseParamTypes (the plan cache key)"))
}

/// `isSafePointGetPath4PlanCache`.
// go: isSafePointGetPath4PlanCache L1 [M-CTX]
pub fn is_safe_point_get_path4_plan_cache(
    sctx: &crate::PlanCtx<'_>,
    path: &crate::util::AccessPath,
) -> bool {
    // PointGet might contain some over-optimized assumptions, like `a>=1 and a<=1` --> `a=1`, but
    // these assumptions may be broken after parameters change.

    if is_safe_point_get_path4_plan_cache_scenario1(path) {
        return true;
    }

    // TODO: enable this fix control switch by default after more test cases are added.
    let fix_control_ok = crate::util::fixcontrol::get_bool_with_default(
        sctx.vars.get_optimizer_fix_control_map(),
        crate::util::fixcontrol::Fix44830,
        false,
    );
    if fix_control_ok
        && (is_safe_point_get_path4_plan_cache_scenario2(path)
            || is_safe_point_get_path4_plan_cache_scenario3(path))
    {
        return true;
    }

    false
}

// go: isSafePointGetPath4PlanCacheScenario1 L1
fn is_safe_point_get_path4_plan_cache_scenario1(path: &crate::util::AccessPath) -> bool {
    // safe scenario 1: each column corresponds to a single EQ, `a=1 and b=2 and c=3` --> `[1, 2, 3]`
    if path.ranges.is_empty() || path.ranges[0].width() != path.access_conds.len() {
        return false;
    }
    for access_cond in &path.access_conds {
        match &**access_cond {
            tidb_expression::Expr::ScalarFunction(f)
                if f.func_name.l == tidb_expression::scalar_function::ast_func::EQ => {}
            _ => return false, // column = constant
        }
    }
    true
}

// go: isSafePointGetPath4PlanCacheScenario2 L1
fn is_safe_point_get_path4_plan_cache_scenario2(path: &crate::util::AccessPath) -> bool {
    // safe scenario 2: this Batch or PointGet is simply from a single IN predicate, `key in (...)`
    if path.ranges.is_empty() || path.access_conds.len() != 1 {
        return false;
    }
    let tidb_expression::Expr::ScalarFunction(f) = &*path.access_conds[0] else {
        return false;
    };
    if f.func_name.l != tidb_expression::scalar_function::ast_func::In {
        return false;
    }
    path.ranges.len() == f.get_args().len() - 1 // no duplicated values in this in-list for safety.
}

// go: isSafePointGetPath4PlanCacheScenario3 L1
fn is_safe_point_get_path4_plan_cache_scenario3(path: &crate::util::AccessPath) -> bool {
    use tidb_expression::Expr;
    // safe scenario 3: this Batch or PointGet is simply from a simple DNF like `key=? or key=? or key=?`
    if path.ranges.is_empty() || path.access_conds.len() != 1 {
        return false;
    }
    let Expr::ScalarFunction(f) = &*path.access_conds[0] else {
        return false;
    };
    if f.func_name.l != tidb_expression::scalar_function::ast_func::LogicOr {
        return false;
    }

    let dnf_exprs = tidb_expression::expression::flatten_dnf_conditions(f);
    if path.ranges.len() != dnf_exprs.len() {
        // no duplicated values in this in-list for safety.
        // e.g. `k=1 or k=2 or k=1` --> [[1, 1], [2, 2]]
        return false;
    }

    for expr in &dnf_exprs {
        let Expr::ScalarFunction(f) = &**expr else {
            return false;
        };
        if f.func_name.l == tidb_expression::scalar_function::ast_func::EQ {
            // (k=1 or k=2) --> [k=1, k=2]
        } else if f.func_name.l == tidb_expression::scalar_function::ast_func::LogicAnd {
            // ((k1=1 and k2=1) or (k1=2 and k2=2)) --> [k1=1 and k2=1, k2=2 and k2=2]
            let cnf_exprs = tidb_expression::expression::flatten_cnf_conditions(f);
            if path.ranges[0].width() != cnf_exprs.len() {
                // not all key columns are specified
                return false;
            }
            for expr in &cnf_exprs {
                // k1=1 and k2=1
                match &**expr {
                    Expr::ScalarFunction(f)
                        if f.func_name.l == tidb_expression::scalar_function::ast_func::EQ => {}
                    _ => return false,
                }
            }
        } else {
            return false;
        }
    }
    true
}
