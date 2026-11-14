//! The N3 executor interface as a trait object: Open / Next / Close through
//! `Box<dyn Executor>`, a child driven by its parent, and the context handed to both.

use std::sync::Arc;

use async_trait::async_trait;
use tidb_base::Error;
use tidb_base::mysql::TypeLonglong;
use tidb_chunk::chunk::{Chunk, new_chunk_with_capacity};
use tidb_executor::{ExecCtx, Executor};
use tidb_sessionctx::stmtctx::new_stmt_ctx;
use tidb_types::field_type::new_field_type;

/// Produces 0..n, at most `batch` rows per Next.
struct Numbers {
    next: i64,
    n: i64,
    batch: usize,
    opened: bool,
}

#[async_trait]
impl Executor for Numbers {
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
    child: Box<dyn Executor>,
    buf: Chunk,
}

#[async_trait]
impl Executor for Even {
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
        child: Box::new(Numbers {
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
