//! go-file: pkg/util/chunk/iterator.go
//!
//! Row iterators (N5a 3.1: L1). The iterators borrow what they iterate (`Row<'a>`).
//! `iterator4RowContainer` iterates a `RowContainer` (block D01.10) and waits for it. Go's
//! cache-line padding of `Iterator4Slice` (`cpu.CacheLinePad`) is `#[repr(align(64))]`.

use tidb_base::Error;

use crate::chunk::Chunk;
use crate::list::List;
use crate::list::RowPtr;
use crate::row::Row;

/// Iterator is used to iterate a number of rows.
///
/// ```text
/// for row := it.Begin(); row != it.End(); row = it.Next() {
///     ...
/// }
/// ```
pub trait Iterator<'a> {
    /// Begin resets the cursor of the iterator and returns the first Row.
    // glue: Go interface method
    fn begin(&mut self) -> Row<'a>;
    /// Next returns the next Row.
    // glue: Go interface method
    fn next(&mut self) -> Row<'a>;
    /// End returns the invalid end Row.
    // glue: Go interface method
    fn end(&self) -> Row<'a> {
        Row::default()
    }
    /// Len returns the length.
    // glue: Go interface method
    fn len(&self) -> usize;
    /// Current returns the current Row.
    // glue: Go interface method
    fn current(&self) -> Row<'a>;
    /// ReachEnd reaches the end of iterator.
    // glue: Go interface method
    fn reach_end(&mut self);
    /// Error returns none-nil error if anything wrong happens during the iteration.
    // glue: Go interface method
    fn error(&self) -> Option<&Error>;
}

/// NewIterator4Slice returns a *Iterator4Slice for Row slice.
// go: NewIterator4Slice L1
pub fn new_iterator4_slice<'a>(rows: Vec<Row<'a>>) -> Iterator4Slice<'a> {
    Iterator4Slice { rows, cursor: 0 }
}

/// Iterator4Slice is used to iterate rows inside a slice.
#[repr(align(64))]
#[derive(Debug)]
pub struct Iterator4Slice<'a> {
    rows: Vec<Row<'a>>,
    cursor: usize,
}

impl<'a> Iterator<'a> for Iterator4Slice<'a> {
    /// Begin implements the Iterator interface.
    // go: Iterator4Slice.Begin L1
    fn begin(&mut self) -> Row<'a> {
        if self.len() == 0 {
            return self.end();
        }
        self.cursor = 1;
        self.rows[0]
    }

    /// Next implements the Iterator interface.
    // go: Iterator4Slice.Next L1
    fn next(&mut self) -> Row<'a> {
        let l = self.len();
        if self.cursor >= l {
            self.cursor = l + 1;
            return self.end();
        }
        let row = self.rows[self.cursor];
        self.cursor += 1;
        row
    }

    /// End implements the Iterator interface.
    // go: Iterator4Slice.End L1
    fn end(&self) -> Row<'a> {
        Row::default()
    }

    /// Len implements the Iterator interface.
    // go: Iterator4Slice.Len L1
    fn len(&self) -> usize {
        self.rows.len()
    }

    /// Current implements the Iterator interface.
    // go: Iterator4Slice.Current L1
    fn current(&self) -> Row<'a> {
        if self.cursor == 0 || self.cursor > self.len() {
            return self.end();
        }
        self.rows[self.cursor - 1]
    }

    /// ReachEnd implements the Iterator interface.
    // go: Iterator4Slice.ReachEnd L1
    fn reach_end(&mut self) {
        self.cursor = self.len() + 1;
    }

    /// Error returns none-nil error if anything wrong happens during the iteration.
    // go: Iterator4Slice.Error L1
    fn error(&self) -> Option<&Error> {
        None
    }
}

impl<'a> Iterator4Slice<'a> {
    /// Reset iterator.rows and cursor.
    // go: Iterator4Slice.Reset L1
    pub fn reset(&mut self, rows: Vec<Row<'a>>) {
        self.rows = rows;
        self.cursor = 0;
    }
}

/// NewIterator4Chunk returns a iterator for Chunk.
// go: NewIterator4Chunk L1
pub fn new_iterator4_chunk(chk: &Chunk) -> Iterator4Chunk<'_> {
    Iterator4Chunk {
        chk,
        cursor: 0,
        num_rows: 0,
    }
}

/// Iterator4Chunk is used to iterate rows inside a chunk.
#[derive(Debug)]
pub struct Iterator4Chunk<'a> {
    chk: &'a Chunk,
    cursor: i32,
    num_rows: i32,
}

impl<'a> Iterator<'a> for Iterator4Chunk<'a> {
    /// Begin implements the Iterator interface.
    // go: Iterator4Chunk.Begin L1
    fn begin(&mut self) -> Row<'a> {
        self.num_rows = self.chk.num_rows() as i32;
        if self.num_rows == 0 {
            return self.end();
        }
        self.cursor = 1;
        self.chk.get_row(0)
    }

    /// Next implements the Iterator interface.
    // go: Iterator4Chunk.Next L1
    #[inline]
    fn next(&mut self) -> Row<'a> {
        if self.cursor >= self.num_rows {
            self.cursor = self.num_rows + 1;
            return self.end();
        }
        let row = self.chk.get_row(self.cursor as usize);
        self.cursor += 1;
        row
    }

    /// End implements the Iterator interface.
    // go: Iterator4Chunk.End L1
    fn end(&self) -> Row<'a> {
        Row::default()
    }

    /// Len implements the Iterator interface
    // go: Iterator4Chunk.Len L1
    fn len(&self) -> usize {
        self.chk.num_rows()
    }

    /// Current implements the Iterator interface.
    // go: Iterator4Chunk.Current L1
    fn current(&self) -> Row<'a> {
        if self.cursor == 0 || self.cursor as usize > self.len() {
            return self.end();
        }
        self.chk.get_row(self.cursor as usize - 1)
    }

    /// ReachEnd implements the Iterator interface.
    // go: Iterator4Chunk.ReachEnd L1
    fn reach_end(&mut self) {
        self.cursor = (self.len() + 1) as i32;
    }

    /// Error returns none-nil error if anything wrong happens during the iteration.
    // go: Iterator4Chunk.Error L1
    fn error(&self) -> Option<&Error> {
        None
    }
}

impl<'a> Iterator4Chunk<'a> {
    /// GetChunk returns the chunk stored in the Iterator4Chunk
    // go: Iterator4Chunk.GetChunk L1
    pub fn get_chunk(&self) -> &'a Chunk {
        self.chk
    }

    /// ResetChunk reset the chunk this iter holds
    // go: Iterator4Chunk.ResetChunk L1
    pub fn reset_chunk(&mut self, chk: &'a Chunk) {
        self.chk = chk;
    }
}

/// NewIterator4List returns a Iterator for List.
// go: NewIterator4List L1
pub fn new_iterator4_list(li: &List) -> Box<dyn Iterator<'_> + '_> {
    Box::new(Iterator4List {
        li,
        chk_cursor: 0,
        row_cursor: 0,
    })
}

struct Iterator4List<'a> {
    li: &'a List,
    chk_cursor: usize,
    row_cursor: usize,
}

impl<'a> Iterator<'a> for Iterator4List<'a> {
    /// Begin implements the Iterator interface.
    // go: iterator4List.Begin L1
    fn begin(&mut self) -> Row<'a> {
        if self.li.num_chunks() == 0 {
            return self.end();
        }
        let chk = self.li.get_chunk(0);
        let row = chk.get_row(0);
        if chk.num_rows() == 1 {
            self.chk_cursor = 1;
            self.row_cursor = 0;
        } else {
            self.chk_cursor = 0;
            self.row_cursor = 1;
        }
        row
    }

    /// Next implements the Iterator interface.
    // go: iterator4List.Next L1
    fn next(&mut self) -> Row<'a> {
        if self.chk_cursor >= self.li.num_chunks() {
            self.chk_cursor = self.li.num_chunks() + 1;
            return self.end();
        }
        let chk = self.li.get_chunk(self.chk_cursor);
        let row = chk.get_row(self.row_cursor);
        self.row_cursor += 1;
        if self.row_cursor == chk.num_rows() {
            self.row_cursor = 0;
            self.chk_cursor += 1;
        }
        row
    }

    /// End implements the Iterator interface.
    // go: iterator4List.End L1
    fn end(&self) -> Row<'a> {
        Row::default()
    }

    /// Len implements the Iterator interface.
    // go: iterator4List.Len L1
    fn len(&self) -> usize {
        self.li.len()
    }

    /// Current implements the Iterator interface.
    // go: iterator4List.Current L1
    fn current(&self) -> Row<'a> {
        if (self.chk_cursor == 0 && self.row_cursor == 0) || self.chk_cursor > self.li.num_chunks() {
            return self.end();
        }
        if self.row_cursor == 0 {
            let cur_chk = self.li.get_chunk(self.chk_cursor - 1);
            return cur_chk.get_row(cur_chk.num_rows() - 1);
        }
        let cur_chk = self.li.get_chunk(self.chk_cursor);
        cur_chk.get_row(self.row_cursor - 1)
    }

    /// ReachEnd implements the Iterator interface.
    // go: iterator4List.ReachEnd L1
    fn reach_end(&mut self) {
        self.chk_cursor = self.li.num_chunks() + 1;
    }

    /// Error returns none-nil error if anything wrong happens during the iteration.
    // go: iterator4List.Error L1
    fn error(&self) -> Option<&Error> {
        None
    }
}

/// NewIterator4RowPtr returns a Iterator for RowPtrs.
// go: NewIterator4RowPtr L1
pub fn new_iterator4_row_ptr<'a>(li: &'a List, ptrs: &'a [RowPtr]) -> Box<dyn Iterator<'a> + 'a> {
    Box::new(Iterator4RowPtr { li, ptrs, cursor: 0 })
}

struct Iterator4RowPtr<'a> {
    li: &'a List,
    ptrs: &'a [RowPtr],
    cursor: usize,
}

impl<'a> Iterator<'a> for Iterator4RowPtr<'a> {
    /// Begin implements the Iterator interface.
    // go: iterator4RowPtr.Begin L1
    fn begin(&mut self) -> Row<'a> {
        if self.len() == 0 {
            return self.end();
        }
        self.cursor = 1;
        self.li.get_row(self.ptrs[0])
    }

    /// Next implements the Iterator interface.
    // go: iterator4RowPtr.Next L1
    fn next(&mut self) -> Row<'a> {
        let l = self.len();
        if self.cursor >= l {
            self.cursor = l + 1;
            return self.end();
        }
        let row = self.li.get_row(self.ptrs[self.cursor]);
        self.cursor += 1;
        row
    }

    /// End implements the Iterator interface.
    // go: iterator4RowPtr.End L1
    fn end(&self) -> Row<'a> {
        Row::default()
    }

    /// Len implements the Iterator interface.
    // go: iterator4RowPtr.Len L1
    fn len(&self) -> usize {
        self.ptrs.len()
    }

    /// Current implements the Iterator interface.
    // go: iterator4RowPtr.Current L1
    fn current(&self) -> Row<'a> {
        if self.cursor == 0 || self.cursor > self.len() {
            return self.end();
        }
        self.li.get_row(self.ptrs[self.cursor - 1])
    }

    /// ReachEnd implements the Iterator interface.
    // go: iterator4RowPtr.ReachEnd L1
    fn reach_end(&mut self) {
        self.cursor = self.len() + 1;
    }

    /// Error returns none-nil error if anything wrong happens during the iteration.
    // go: iterator4RowPtr.Error L1
    fn error(&self) -> Option<&Error> {
        None
    }
}

/// multiIterator joins several iterators together to form a new iterator
struct MultiIterator<'a> {
    iters: Vec<Box<dyn Iterator<'a> + 'a>>,
    num_iter: usize,
    length: usize,
    cur_ptr: usize,
    /// Go's `curIter` (nil until `Begin`).
    cur_iter: Option<usize>,
    err: Option<Error>,
}

/// NewMultiIterator creates a new multiIterator
// go: NewMultiIterator L1
pub fn new_multi_iterator<'a>(iters: Vec<Box<dyn Iterator<'a> + 'a>>) -> Box<dyn Iterator<'a> + 'a> {
    let mut iter = MultiIterator {
        iters: Vec::new(),
        num_iter: 0,
        length: 0,
        cur_ptr: 0,
        cur_iter: None,
        err: None,
    };
    for it in iters {
        if it.len() > 0 {
            iter.length += it.len();
            iter.iters.push(it);
        }
    }
    iter.num_iter = iter.iters.len();
    Box::new(iter)
}

impl<'a> Iterator<'a> for MultiIterator<'a> {
    /// Len implements the Iterator interface.
    // go: multiIterator.Len L1
    fn len(&self) -> usize {
        self.length
    }

    /// Begin implements the Iterator interface.
    // go: multiIterator.Begin L1
    fn begin(&mut self) -> Row<'a> {
        self.cur_ptr = 0;
        if self.num_iter > 0 {
            self.cur_iter = Some(0);
            self.iters[0].begin();
        }
        self.current_inner()
    }

    /// Next implements the Iterator interface.
    // go: multiIterator.Next L1
    fn next(&mut self) -> Row<'a> {
        if self.cur_ptr == self.num_iter {
            return self.end();
        }
        let cur = &mut self.iters[self.cur_iter.expect("invalid memory address or nil pointer dereference")];
        let mut next = cur.next();
        if next == cur.end() {
            self.err = cur.error().cloned();
            if self.err.is_some() {
                self.reach_end();
                return self.end();
            }
            self.cur_ptr += 1;
            if self.cur_ptr == self.num_iter {
                return self.end();
            }
            self.cur_iter = Some(self.cur_ptr);
            next = self.iters[self.cur_ptr].begin();
        }
        next
    }

    /// Current implements the Iterator interface.
    ///
    /// Go records the current iterator's error here; `current` takes `&self`, so the error
    /// is recorded by `begin` / `next` (see [`MultiIterator::current_inner`]).
    // go: multiIterator.Current L2 (the error is recorded by the &mut callers)
    fn current(&self) -> Row<'a> {
        if self.cur_ptr == self.num_iter {
            return self.end();
        }
        self.iters[self.cur_iter.expect("invalid memory address or nil pointer dereference")].current()
    }

    /// End implements the Iterator interface.
    // go: multiIterator.End L1
    fn end(&self) -> Row<'a> {
        Row::default()
    }

    /// ReachEnd implements the Iterator interface.
    // go: multiIterator.ReachEnd L1
    fn reach_end(&mut self) {
        self.cur_ptr = self.num_iter;
    }

    /// Error returns none-nil error if anything wrong happens during the iteration.
    // go: multiIterator.Error L1
    fn error(&self) -> Option<&Error> {
        self.err.as_ref()
    }
}

impl<'a> MultiIterator<'a> {
    /// `Current` with Go's error recording.
    // glue: multiIterator.Current error recording
    fn current_inner(&mut self) -> Row<'a> {
        if self.cur_ptr == self.num_iter {
            return self.end();
        }
        let cur = &self.iters[self.cur_iter.expect("invalid memory address or nil pointer dereference")];
        let row = cur.current();
        if row == cur.end() {
            self.err = cur.error().cloned();
        }
        row
    }
}
