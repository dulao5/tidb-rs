//! go-file: pkg/planner/core/resolve/resolve.go
//!
//! Package resolve is used for semantic resolve of the AST tree.
//! semantic resolve is mostly done by 'core.preprocessor', in tableListExtractor
//! and updatableTableListResolver we also do some resolve for aliases.
//!
//! Mapping: Go keys the table names by the `*ast.TableName` pointer; here by the address of
//! the AST node (the nodes are boxed and stay in place while the statement is planned). The
//! `*Context` shared by the `NodeW`s of one statement is an `Arc<Mutex<Context>>` (`M-LOCK`;
//! only the planning task touches it).

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use tidb_model::{DBInfo, TableInfo};
use tidb_parser::ast::{Stmt, TableName};

/// The identity of an AST table name (Go: the `*ast.TableName` pointer).
pub type TableNameKey = usize;

/// The key of `tn`.
// glue: pointer identity of an AST node
pub fn table_name_key(tn: &TableName) -> TableNameKey {
    tn as *const TableName as usize
}

/// TableNameW is a wrapper around ast.TableName to store more information.
#[derive(Clone, Debug)]
pub struct TableNameW {
    /// The embedded `*ast.TableName`, by identity.
    pub table_name: TableNameKey,
    /// DBInfo.
    pub db_info: Arc<DBInfo>,
    /// TableInfo.
    pub table_info: Arc<TableInfo>,
}

/// The shared resolve context of a statement.
pub type ContextRef = Arc<Mutex<Context>>;

/// NodeW is a wrapper around ast.Node to store resolve context.
pub struct NodeW<'a> {
    /// Node.
    pub node: &'a mut Stmt,
    resolve_ctx: ContextRef,
}

/// NewNodeW creates a NodeW.
// go: NewNodeW L1
pub fn new_node_w(node: &mut Stmt) -> NodeW<'_> {
    NodeW {
        node,
        resolve_ctx: Arc::new(Mutex::new(new_context())),
    }
}

/// NewNodeWWithCtx creates a NodeW with the given Context.
// go: NewNodeWWithCtx L1
pub fn new_node_w_with_ctx(node: &mut Stmt, resolve_ctx: ContextRef) -> NodeW<'_> {
    NodeW { node, resolve_ctx }
}

impl NodeW<'_> {
    /// CloneWithNewNode creates a new NodeW with the given ast.Node.
    // go: NodeW.CloneWithNewNode L1
    pub fn clone_with_new_node<'b>(&self, new_node: &'b mut Stmt) -> NodeW<'b> {
        NodeW {
            node: new_node,
            resolve_ctx: Arc::clone(&self.resolve_ctx),
        }
    }

    /// GetResolveContext returns the Context of the NodeW.
    // go: NodeW.GetResolveContext L1
    pub fn get_resolve_context(&self) -> ContextRef {
        Arc::clone(&self.resolve_ctx)
    }
}

/// Context is used to store the context and result of resolving AST tree.
#[derive(Debug, Default)]
pub struct Context {
    table_names: HashMap<TableNameKey, TableNameW>,
}

/// NewContext creates a Context.
// go: NewContext L1
pub fn new_context() -> Context {
    Context {
        table_names: HashMap::new(),
    }
}

impl Context {
    /// AddTableName adds the AST table name and its corresponding TableNameW to the Context.
    // go: Context.AddTableName L1
    pub fn add_table_name(&mut self, table_name_w: TableNameW) {
        self.table_names
            .insert(table_name_w.table_name, table_name_w);
    }

    /// GetTableName returns the TableNameW of the AST table name.
    /// the TableNameW should have been added to the Context in pre-process phase before
    /// calling this function, if it doesn't exist, pre-process should have returned error,
    /// so we don't check nil-ness in most cases.
    // go: Context.GetTableName L1
    pub fn get_table_name(&self, table_name: &TableName) -> Option<&TableNameW> {
        self.table_names.get(&table_name_key(table_name))
    }

    /// GetTableNames returns the inner map contain all TableNameW in the Context in pre-process phase.
    // go: Context.GetTableNames L1
    pub fn get_table_names(&self) -> &HashMap<TableNameKey, TableNameW> {
        &self.table_names
    }
}
