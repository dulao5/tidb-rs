//! The N3 executor interface as a trait object: Open / Next / Close through
//! `Box<dyn Executor>`, a child driven by its parent, and the context handed to both.

use std::sync::Arc;

use async_trait::async_trait;
use tidb_base::Error;
use tidb_base::mysql::TypeLonglong;
use tidb_chunk::chunk::{Chunk, new_chunk_with_capacity};
use tidb_executor::internal::exec::executor::{BaseExecutorV2, new_base_executor_v2};
use tidb_executor::{ExecCtx, Executor};
use tidb_memory::sqlkiller::SqlKiller;
use tidb_sessionctx::stmtctx::new_stmt_ctx;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_types::field_type::new_field_type;

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

/// The base executor of a test executor (no schema, no children).
fn base() -> BaseExecutorV2 {
    new_base_executor_v2(
        &SessionVars::new(Box::new(Acc)),
        &SqlKiller::new(),
        None,
        0,
        Vec::new(),
    )
}

/// Produces 0..n, at most `batch` rows per Next.
struct Numbers {
    base: BaseExecutorV2,
    next: i64,
    n: i64,
    batch: usize,
    opened: bool,
}

#[async_trait]
impl Executor for Numbers {
    fn base(&self) -> &BaseExecutorV2 {
        &self.base
    }
    fn base_mut(&mut self) -> &mut BaseExecutorV2 {
        &mut self.base
    }
    async fn open(&mut self, _ctx: &ExecCtx) -> Result<(), Error> {
        self.opened = true;
        Ok(())
    }
    async fn next(&mut self, _ctx: &ExecCtx, chk: &mut Chunk) -> Result<(), Error> {
        chk.reset();
        while self.next < self.n && chk.num_rows() < self.batch {
            chk.append_int64(0, self.next);
            self.next += 1;
        }
        Ok(())
    }
    async fn close(&mut self) -> Result<(), Error> {
        self.opened = false;
        Ok(())
    }
}

/// Keeps the even numbers of its child.
struct Even {
    base: BaseExecutorV2,
    child: Box<dyn Executor>,
    buf: Chunk,
}

#[async_trait]
impl Executor for Even {
    fn base(&self) -> &BaseExecutorV2 {
        &self.base
    }
    fn base_mut(&mut self) -> &mut BaseExecutorV2 {
        &mut self.base
    }
    async fn open(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        self.child.open(ctx).await
    }
    async fn next(&mut self, ctx: &ExecCtx, chk: &mut Chunk) -> Result<(), Error> {
        chk.reset();
        loop {
            self.child.next(ctx, &mut self.buf).await?;
            if self.buf.num_rows() == 0 {
                return Ok(());
            }
            for i in 0..self.buf.num_rows() {
                let v = self.buf.get_row(i).get_int64(0);
                if v % 2 == 0 {
                    chk.append_int64(0, v);
                }
            }
            if chk.num_rows() > 0 {
                return Ok(());
            }
        }
    }
    async fn close(&mut self) -> Result<(), Error> {
        self.child.close().await
    }
}

fn int_chunk() -> Chunk {
    new_chunk_with_capacity(&[new_field_type(TypeLonglong)], 32)
}

#[test]
fn volcano_over_trait_objects() {
    let ctx = ExecCtx::new(Arc::new(new_stmt_ctx()));
    let mut root: Box<dyn Executor> = Box::new(Even {
        base: base(),
        child: Box::new(Numbers {
            base: base(),
            next: 0,
            n: 10,
            batch: 3,
            opened: false,
        }),
        buf: int_chunk(),
    });
    let got = futures::executor::block_on(async {
        root.open(&ctx).await?;
        let mut chk = int_chunk();
        let mut got = Vec::new();
        loop {
            root.next(&ctx, &mut chk).await?;
            if chk.num_rows() == 0 {
                break;
            }
            got.extend((0..chk.num_rows()).map(|i| chk.get_row(i).get_int64(0)));
        }
        root.close().await?;
        Ok::<_, Error>(got)
    })
    .unwrap();
    assert_eq!(got, [0, 2, 4, 6, 8]);
}

/// A worker task gets a clone of the context, which shares the statement context.
#[test]
fn exec_ctx_is_shared_with_workers() {
    let ctx = ExecCtx::new(Arc::new(new_stmt_ctx()));
    let worker = ctx.clone();
    std::thread::spawn(move || assert!(Arc::strong_count(&worker.stmt) >= 2))
        .join()
        .unwrap();
    assert_eq!(Arc::strong_count(&ctx.stmt), 1);
}
