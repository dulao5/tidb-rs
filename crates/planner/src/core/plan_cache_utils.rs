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
//!   into them (`param.Datum = val`). The Rust markers hold no value: the values go to
//!   `SessionVars.PlanCacheParams` only, by the marker's order, which is where the expression
//!   rewriter reads them (D09.2). PREPARE writes each marker's order as Go's `SetOrder` does
//!   (`ParamMarkerExpr::set_order`, the only write into the statement). `params` keeps the
//!   markers' offsets in order.
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
use tidb_parser::types::FieldType;
use tidb_sessionctx::variable::session::SessionVars;
use tidb_types::field_name::NameSlice;

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
    /// The parameter markers of the LIMIT clauses' count and offset (Go reads them through
    /// `limits`, whose markers hold the values): their orders, in Go's order (each clause's
    /// count, then its offset).
    pub limit_params: Vec<usize>,
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

    /// The instance plan cache key of the last EXECUTE, reused as the buffer of the next
    /// one (the plan-cache exception A3 spec §3.4: the lookup does not allocate).
    pub cache_key: PlanCacheKey,
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
            limit_params: Vec::new(),
            has_subquery: false,
            normalized_sql: String::new(),
            sql_digest: String::new(),
            for_update_read: false,
            stmt_text: String::new(),
            db_name: Vec::new(),
            tbls: Vec::new(),
            cache_key: PlanCacheKey::default(),
        }
    }
}

/// A prepared statement shared by the session's statement map (`SessionVars.PreparedStmts`)
/// and the statements that execute it.
pub type PlanCacheStmtRef = Arc<parking_lot::Mutex<PlanCacheStmt>>;

/// `markers[i].SetOrder(i)` for the markers of `stmt`, `sorted_offsets` being their offsets
/// sorted by position (what the extractor collected, sorted).
///
/// Go's extractor holds the marker nodes themselves and writes each one's order through
/// its pointer. The Rust walk lends each node only for the call, so the extractor keeps the
/// offsets, and this second walk over the same statement gives each marker the index of its
/// offset among the sorted offsets. A marker has its own byte offset, so the index is the
/// marker's place in Go's sorted slice: the same numbering. Without it every marker kept the
/// parser's order 0 and read the first parameter (#352).
// go-part: GeneratePlanCacheStmtWithAST, ExtractAndSortParamMarkers (the SetOrder loop)
fn set_param_marker_orders(stmt: &Stmt, sorted_offsets: &[usize]) {
    struct SetOrder<'a>(&'a [usize]);
    impl Visit for SetOrder<'_> {
        // glue: ast.Visitor (the SetOrder loop)
        fn enter(&mut self, _: CNode<'_>) -> bool {
            false
        }
        // glue: ast.Visitor (the SetOrder loop)
        fn leave(&mut self, n: CNode<'_>) -> bool {
            if let CNode::ParamMarker(x) = n {
                let i = self.0.binary_search(&x.offset);
                debug_assert!(i.is_ok(), "a marker the extractor did not see");
                if let Ok(i) = i {
                    x.set_order(i);
                }
            }
            true
        }
        // glue: ast.Visitor (the extractor already walked the same statement without error)
        fn fail(&mut self, _: Error) {}
    }
    Walker(&mut SetOrder(sorted_offsets)).stmt(stmt);
}

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
    // for i := 0; i < paramCount; i++ { extractor.markers[i].SetOrder(i) }
    set_param_marker_orders(&stmt, &extractor.markers);

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
        limit_params: Vec::new(),
        has_subquery: false,
        cache_key: PlanCacheKey::default(),
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
        limit_params: Vec<usize>,
        has_subquery: bool,
    }
    impl Visit for P {
        // go: planCacheStmtProcessor.Enter L2 (the tables are for the plan cache key's statistics version, which is 0 without a statistics handle)
        fn enter(&mut self, n: CNode<'_>) -> bool {
            match n {
                CNode::Limit(l) => {
                    self.limits += 1;
                    // NewPlanCacheKey reads node.Count and node.Offset when they are
                    // parameter markers.
                    for val in std::iter::once(&l.count).chain(l.offset.as_ref()) {
                        if let tidb_parser::ast::Expr::ParamMarker(m) = val {
                            self.limit_params.push(m.order());
                        }
                    }
                }
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
    stmt.limit_params = p.limit_params;
    stmt.has_subquery = p.has_subquery;
}

/// The key the session's plan cache stand-in records for `cleanRetryInfo` (Go calls
/// `NewPlanCacheKey` there to delete the closed statement's plan from the session-level
/// cache). tidb-rs has no session-level cache (the plan-cache exception A3, §1.2), so the
/// deletion removes nothing; the key is the statement's database and text, always
/// cacheable, and Go's behaviour of computing every key from the first closed statement is
/// kept (D13.1 tasks). The instance cache's key is [`new_plan_cache_key`].
// glue: the session plan cache stand-in's key (no session-level cache, A3 spec §1.2)
pub fn session_plan_cache_key(
    stmt: &PlanCacheStmt,
) -> Result<(String, String, bool, String), Error> {
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
/// position and numbers them (`SetOrder`). Returns the markers' offsets; the Rust markers
/// carry no value, so Go's `Datum.SetNull` / `InExecute = false` have nothing to reset.
// go: ExtractAndSortParamMarkers L2 (offsets returned; the Rust markers hold no value to reset)
pub fn extract_and_sort_param_markers(stmt_node: &Stmt) -> Result<Vec<usize>, Error> {
    let mut extractor = ParamMarkerExtractor::default();
    Walker(&mut extractor).stmt(stmt_node);
    if let Some(e) = extractor.err {
        return Err(e);
    }
    extractor.markers.sort();
    set_param_marker_orders(stmt_node, &extractor.markers);
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

// ---------------------------------------------------------------------------------------
// The instance plan cache's key and value (the plan-cache exception A3 spec §2.1, §3.3, §3.4).
// ---------------------------------------------------------------------------------------

/// `ast.ReadCommitted`.
const READ_COMMITTED: &str = "READ-COMMITTED";

/// `MaxCacheableLimitCount`: the largest `LIMIT ?` value a cached plan may be keyed by.
pub const MAX_CACHEABLE_LIMIT_COUNT: u64 = 10000;

/// The process's random hash state of the plan cache keys (A3 spec §3.4: a seed drawn at
/// start, so that crafted statement texts cannot pile up in one shard or bucket).
static KEY_HASH_STATE: std::sync::LazyLock<foldhash::fast::RandomState> =
    std::sync::LazyLock::new(foldhash::fast::RandomState::default);

/// The instance plan cache key (Go's `string` of `NewPlanCacheKey`).
///
/// Go concatenates the user, host, database, statement text and the encoded environment into
/// one string. Here the statement text is kept apart (its hash is computed once per
/// statement text, not on every EXECUTE) and the rest is `env`, encoded in Go's order with a
/// length prefix in front of every string field (A3 decision 9: Go's concatenation lets
/// `'a127.0.0.'@'%'` and `'a'@'127.0.0.%'` produce the same key). The bytes are compared,
/// never shown, so integers are fixed-width little-endian rather than Go's `codec` encoding.
// glue: A3 spec §3.4 (the representation of NewPlanCacheKey's string)
#[derive(Clone, Default)]
pub struct PlanCacheKey {
    stmt_text: Arc<str>,
    text_hash: u64,
    env: smallvec::SmallVec<[u8; 128]>,
    hash: u64,
}

impl PlanCacheKey {
    /// The key's hash (shard and bucket selection).
    // glue: A3 spec §3.4
    pub fn hash_value(&self) -> u64 {
        self.hash
    }

    /// The bytes of the key, for tests and logs (`text` then the environment).
    // glue: A3 spec §3.4
    pub fn debug_bytes(&self) -> Vec<u8> {
        let mut b = self.stmt_text.as_bytes().to_vec();
        b.extend_from_slice(&self.env);
        b
    }

    /// Starts a key for `stmt_text`: the text and its hash are kept from the previous key when
    /// the text is the same (the usual case: the key buffer is the statement's own).
    // glue: A3 spec §3.4
    pub(crate) fn reset(&mut self, stmt_text: &str) {
        if &*self.stmt_text != stmt_text {
            self.stmt_text = Arc::from(stmt_text);
            self.text_hash = std::hash::BuildHasher::hash_one(&*KEY_HASH_STATE, stmt_text);
        }
        self.env.clear();
    }

    /// Appends a string field with its length prefix (A3 decision 9).
    // glue: A3 decision 9
    pub(crate) fn push_str(&mut self, v: &str) {
        self.env.extend_from_slice(&(v.len() as u32).to_le_bytes());
        self.env.extend_from_slice(v.as_bytes());
    }

    // glue: codec.EncodeInt / EncodeUint of NewPlanCacheKey (fixed width, see the type doc)
    fn push_u64(&mut self, v: u64) {
        self.env.extend_from_slice(&v.to_le_bytes());
    }

    // go: bool2Byte L2 (writes 0 / 1 into the key; also strconv.FormatBool of NewPlanCacheKey)
    fn push_bool(&mut self, v: bool) {
        self.env.push(u8::from(v));
    }

    /// Ends the key: its hash from the text's hash and the environment.
    // glue: A3 spec §3.4
    pub(crate) fn finish(&mut self) {
        self.hash =
            std::hash::BuildHasher::hash_one(&*KEY_HASH_STATE, (self.text_hash, &self.env[..]));
    }
}

impl PartialEq for PlanCacheKey {
    // glue: Go compares the key strings
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash
            && self.env == other.env
            && (Arc::ptr_eq(&self.stmt_text, &other.stmt_text) || self.stmt_text == other.stmt_text)
    }
}

impl Eq for PlanCacheKey {}

impl std::hash::Hash for PlanCacheKey {
    // glue: the map hashes the precomputed hash (PassThroughHasher)
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u64(self.hash);
    }
}

impl std::fmt::Debug for PlanCacheKey {
    // glue: Debug of the key
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PlanCacheKey({:?}, {} env bytes)",
            self.stmt_text,
            self.env.len()
        )
    }
}

/// `hashInt64Uint64Map`: the map's entries in key order.
// go: hashInt64Uint64Map L2 (writes into the key; fixed-width integers, see PlanCacheKey)
fn hash_int64_uint64_map(key: &mut PlanCacheKey, m: &HashMap<i64, u64>) {
    let mut keys: smallvec::SmallVec<[i64; 4]> = m.keys().copied().collect();
    keys.sort_unstable();
    for k in keys {
        key.push_u64(k as u64);
        key.push_u64(m[&k]);
    }
}

/// What `NewPlanCacheKey` reads besides the session variables and the statement:
/// `domain.GetDomain(sctx).InfoSchema().SchemaMetaVersion()` (read in RC and for
/// `FOR UPDATE` reads only).
// glue: the arguments of NewPlanCacheKey that Go reaches through sctx
pub struct PlanCacheKeyEnv<'a> {
    /// The domain's newest InfoSchema (`None`: no domain, version 0).
    pub domain_is: Option<&'a dyn InfoSchemaLookup>,
}

/// NewPlanCacheKey creates the plan cache key for this statement.
/// Note: lastUpdatedSchemaVersion will only be set in the case of rc or for update read in order to
/// differentiate the cache key. In other cases, it will be 0.
/// All information that might affect the plan should be considered in this function.
///
/// The key is written into `stmt.cache_key` (the statement's reusable buffer); returns
/// `(binding, cacheable, reason)`. Fields the new stack does not carry take the value Go has
/// in the same situation: no binding (bindinfo, D13.2: `MatchSQLBindingForPlanCache` finds
/// none), no user identity (the session has none: Go's nil `User` in tests, empty names),
/// the statistics version 0 of tables without a statistics handle (pseudo statistics), no
/// dirty table (`TblInfo2UnionScan`, the write path).
// go: NewPlanCacheKey L2 (writes into the statement's key buffer; bindings, user identity, statistics versions and dirty tables take Go's values when they are absent)
pub fn new_plan_cache_key(
    vars: &mut SessionVars,
    stmt: &mut PlanCacheStmt,
    env: &PlanCacheKeyEnv<'_>,
) -> Result<(String, bool, String), Error> {
    // binding, ignored := bindinfo.MatchSQLBindingForPlanCache(...): no bindings (D13.2).
    let binding = String::new();

    // In rc or for update read, we need the latest schema version to decide whether we need to
    // rebuild the plan. So we set this value in rc or for update read. In other cases, let it be 0.
    let mut latest_schema_version: i64 = 0;
    if vars.is_isolation(READ_COMMITTED) || stmt.for_update_read {
        // In Rc or ForUpdateRead, we should check if the information schema has been changed since
        // last time. If it changed, we should rebuild the plan. Here, we use a different and more
        // up-to-date schema version which can lead plan cache miss and thus, the plan will be rebuilt.
        latest_schema_version = env.domain_is.map_or(0, |is| is.schema_meta_version());
    }

    // rebuild key to exclude kv.TiFlash when stmt is not read only
    use tidb_base::kv::{TiDB, TiFlash, TiKV};
    let engines = &vars.state.isolation_read_engines;
    let mut has_tiflash = engines.contains(&TiFlash);
    if has_tiflash && !crate::optimize::ast_is_read_only(&stmt.prepared_ast.stmt)? {
        has_tiflash = false;
    }
    let engine_bits = u8::from(engines.contains(&TiDB))
        | u8::from(engines.contains(&TiKV)) << 1
        | u8::from(has_tiflash) << 2;

    if stmt.stmt_text.is_empty() {
        return Err(tidb_base::errors::new("no statement text"));
    }
    if stmt.schema_version == 0 && !cfg!(test) {
        return Err(tidb_base::errors::new("Schema version uninitialized"));
    }
    let stmt_db = if stmt.stmt_db.is_empty() {
        vars.current_db.as_str()
    } else {
        stmt.stmt_db.as_str()
    };
    let timezone_offset: i64 = match &vars.time_zone {
        Some(loc) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            i64::from(loc.lookup(now).offset)
        }
        None => 0,
    };
    let (conn_charset, conn_collation) = vars.get_charset_info();

    // not allow to share the same plan among different users for safety: the session has no
    // user identity (SessionVars.User is not carried), as Go's nil User. With one account this
    // is Go's key; once identity exists, different users must not share plans.
    // glue: blocked-by:D04.1 (the user and host of the key: different users must not share plans once identity exists)
    let (user_name, host_name) = ("", "");

    // the user might switch the prune mode dynamically
    let prune_mode = vars.state.partition_prune_mode.clone();

    let key = &mut stmt.cache_key;
    key.reset(&stmt.stmt_text);
    key.push_str(user_name);
    key.push_str(host_name);
    key.push_str(stmt_db);
    key.push_u64(stmt.schema_version as u64);
    hash_int64_uint64_map(key, &stmt.relate_version);
    key.push_str(&prune_mode);
    // Only be set in rc or for update read and leave it default otherwise.
    key.push_u64(latest_schema_version as u64);
    key.push_u64(vars.sql_mode as u64);
    key.push_u64(timezone_offset as u64);
    key.env.push(engine_bits);
    key.push_u64(vars.select_limit);
    key.push_str(&binding);
    key.push_str(&conn_charset);
    key.push_str(&conn_collation);
    key.push_bool(vars.in_restricted_sql);
    key.push_bool(
        tidb_sessionctx::variable::tidb_vars::RestrictedReadOnly
            .load(std::sync::atomic::Ordering::SeqCst),
    );
    key.push_bool(
        tidb_sessionctx::variable::tidb_vars::VarTiDBSuperReadOnly
            .load(std::sync::atomic::Ordering::SeqCst),
    );
    // expr-pushdown-blacklist can affect query optimization, so we need to consider it in plan cache.
    key.push_u64(
        tidb_expr_pushdown::infer_pushdown::ExprPushDownBlackListReloadTimeStamp
            .load(std::sync::atomic::Ordering::SeqCst) as u64,
    );

    // whether this query has sub-query
    if stmt.has_subquery {
        if !vars.enable_plan_cache_for_subquery {
            return Ok((
                binding,
                false,
                "the switch 'tidb_enable_plan_cache_for_subquery' is off".to_owned(),
            ));
        }
        key.push_bool(true);
    } else {
        key.push_bool(false);
    }

    // this variable might affect the plan
    key.push_bool(vars.foreign_key_checks);

    // "limit ?" can affect the cached plan: "limit 1" and "limit 10000" should use different plans.
    if stmt.limits > 0 {
        if !vars.enable_plan_cache_for_param_limit {
            return Ok((
                binding,
                false,
                "the switch 'tidb_enable_plan_cache_for_param_limit' is off".to_owned(),
            ));
        }
        key.env.push(b'|');
        let params = vars.state.plan_cache_params.lock();
        for &order in &stmt.limit_params {
            let (type_expected, val) =
                crate::core::logical_plan_builder::check_param_type_int64_or_uint64(
                    &params.get_param_value(order as i64),
                );
            if !type_expected {
                return Ok((binding, false, "unexpected value after LIMIT".to_owned()));
            }
            if val > MAX_CACHEABLE_LIMIT_COUNT {
                return Ok((binding, false, "limit count is too large".to_owned()));
            }
            key.push_u64(val);
        }
        key.env.push(b'|');
    }

    // stats ver can affect cached plan
    if vars.plan_cache_invalidation_on_fresh_stats {
        // getLatestVersionFromStatsTable of each table: 0 without a statistics handle
        // (pseudo statistics, the new stack has no statistics handle, D11.x).
        let stats_ver_hash: u64 = 0;
        key.push_u64(stats_ver_hash);
    }

    // handle dirty tables: TblInfo2UnionScan is not carried (the write path), none.

    // txn status
    key.env.push(b'|');
    key.push_bool(vars.in_txn());
    key.push_bool(vars.is_autocommit());
    key.push_bool(
        tidb_base::config::get_global_config()
            .pessimistic_txn
            .pessimistic_auto_commit
            .load(),
    );
    key.push_bool(vars.stmt_ctx().for_share_lock_enabled_by_noop);
    key.push_bool(vars.shared_lock_promotion);
    key.finish();

    Ok((binding, true, String::new()))
}

/// parseParamTypes get parameters' types in PREPARE statement
///
/// The parameters are the binary protocol's constants, whose type is inferred from the
/// value (`ExecBinaryParam`: `InferParamTypeFromUnderlyingValue` on a new field type); the
/// session hands the planner the values, so the type is inferred here the same way. The
/// text protocol's `GetVar` arm (`EXECUTE ... USING @v`) needs the expression rewriter
/// (D09.2), as `SetParameterValuesIntoSCtx`. `out` is the caller's buffer (A3 spec §3.4).
// go: parseParamTypes L2 (binary-protocol constants: the type is inferred from the value)
pub fn parse_param_types(params: &[tidb_types::datum::Datum], out: &mut Vec<FieldType>) {
    out.clear();
    for param in params {
        let mut tp = FieldType::default();
        tidb_types::field_type::infer_param_type_from_underlying_value(&param.get_value(), &mut tp);
        out.push(tp);
    }
}

/// CheckTypesCompatibility4PC compares FieldSlice with []*types.FieldType
/// Currently this is only used in plan cache to check whether the types of parameters are compatible.
/// If the types of parameters are compatible, we can use the cached plan.
/// tpsExpected is types from cached plan
// go: checkTypesCompatibility4PC L1
pub fn check_types_compatibility4_pc(expected: &[FieldType], actual: &[FieldType]) -> bool {
    use tidb_base::mysql::{TypeNewDecimal, TypeVarString, TypeVarchar, has_unsigned_flag};
    if expected.len() != actual.len() {
        return false;
    }
    for i in 0..actual.len() {
        // We only use part of logic of `func (ft *FieldType) Equal(other *FieldType)` here because (1) only numeric and
        // string types will show up here, and (2) we don't need flen and decimal to be matched exactly to use plan cache
        let tp_equal = (expected[i].get_type() == actual[i].get_type())
            || (expected[i].get_type() == TypeVarchar && actual[i].get_type() == TypeVarString)
            || (expected[i].get_type() == TypeVarString && actual[i].get_type() == TypeVarchar);
        if !tp_equal
            || expected[i].get_charset() != actual[i].get_charset()
            || expected[i].get_collate() != actual[i].get_collate()
            || (expected[i].eval_type() == tidb_parser::types::ETInt
                && has_unsigned_flag(expected[i].get_flag())
                    != has_unsigned_flag(actual[i].get_flag()))
        {
            return false;
        }
        // When the type is decimal, we should compare the Flen and Decimal.
        // We can only use the plan when both Flen and Decimal should less equal than the cached one.
        // We assume here that there is no correctness problem when the precision of the parameters is less than the precision of the parameters in the cache.
        if tp_equal
            && expected[i].get_type() == TypeNewDecimal
            && !(expected[i].get_flen() >= actual[i].get_flen()
                && expected[i].get_decimal() >= actual[i].get_decimal())
        {
            return false;
        }
    }
    true
}

/// PlanCacheValue stores the cached Statement and StmtNode.
///
/// Go's `Plan` is "not-read-only, session might update it before reusing", so the instance
/// cache clones it on every hit. Here the plan is the statement's own `Arc` tree, sealed
/// (A3 spec §3.1, §3.6) and never written: a hit rebuilds the ranges by copying the path
/// to the rewritten node (`plan_cache_rebuild`). `memoryUsage` is the bytes the entry
/// holds (A3 decision 4, [`crate::core::plan_cache_heap_size`]), not Go's estimate.
// glue: Go type PlanCacheValue (an immutable shared plan; memory is the held bytes, A3 decision 4)
// go-exception: EXC.A3 PlanCacheValue.CloneForInstancePlanCache (a hit is not cloned, A3 spec §1.2)
pub struct PlanCacheValue {
    pub plan: crate::PhysicalPlanRef,
    /// read-only
    pub output_columns: NameSlice,
    /// read-only, all parameters' types, different parameters may share same plan
    pub param_types: Vec<FieldType>,
    /// read-only, hints which set session variables
    pub stmt_hints: tidb_sessionctx::hint::StmtHints,
    /// read-only: the bytes the entry holds.
    mem_bytes: i64,
}

impl std::fmt::Debug for PlanCacheValue {
    // glue: Debug of the cached value
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PlanCacheValue({:?}, {} bytes)",
            self.plan, self.mem_bytes
        )
    }
}

impl PlanCacheValue {
    /// MemoryUsage return the memory usage of PlanCacheValue: the bytes it holds, computed
    /// once when it is built (A3 decision 4).
    // go: PlanCacheValue.MemoryUsage L2 (the held bytes, A3 decision 4)
    pub fn memory_usage(&self) -> i64 {
        self.mem_bytes
    }
}

/// NewPlanCacheValue creates a SQLCacheValue, for the instance plan cache: `None` when the
/// plan cannot go into the instance cache (Go's `CloneForPlanCache` returning `false`, a
/// silent "not cached", A3 spec §3.3). Otherwise the plan's nodes are sealed (their cost
/// memo is no longer written, A3 spec §3.6) and the held bytes are counted.
// go: NewPlanCacheValue L2 (plus the instance cache's CloneForPlanCache check and the seal, A3 spec §3.3)
pub fn new_plan_cache_value(
    plan: &crate::PhysicalPlanRef,
    names: &NameSlice,
    param_types: &[FieldType],
    stmt_hints: &tidb_sessionctx::hint::StmtHints,
) -> Option<PlanCacheValue> {
    if !plan.instance_cacheable() {
        return None;
    }
    crate::core::plan_cache_heap_size::seal_plan(plan.as_ref());
    let mut v = PlanCacheValue {
        plan: Arc::clone(plan),
        output_columns: names.clone(),
        param_types: param_types.to_vec(),
        stmt_hints: stmt_hints.clone_hints(),
        mem_bytes: 0,
    };
    v.mem_bytes = crate::core::plan_cache_heap_size::plan_cache_value_heap_size(&v);
    Some(v)
}

#[cfg(test)]
impl PlanCacheKey {
    /// A key of `text` with an empty environment (tests).
    pub(crate) fn for_test(text: &str) -> PlanCacheKey {
        let mut k = PlanCacheKey::default();
        k.reset(text);
        k.finish();
        k
    }
}

#[cfg(test)]
impl PlanCacheValue {
    /// A value of `plan` that holds `mem` bytes (Go's tests set `memoryUsage`).
    pub(crate) fn for_test(
        plan: crate::PhysicalPlanRef,
        mem: i64,
        param_types: Vec<FieldType>,
    ) -> PlanCacheValue {
        PlanCacheValue {
            plan,
            output_columns: NameSlice::new(),
            param_types,
            stmt_hints: Default::default(),
            mem_bytes: mem,
        }
    }
}
