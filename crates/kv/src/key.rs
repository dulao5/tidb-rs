//! go-file: pkg/kv/key.go
//!
//! `Key`, `KeyRange`, the row handles and the handle maps.
//!
//! Mapping:
//! - `Key` is a byte vector with Go's methods. Go slices carry a capacity that
//!   `KeyRangeSliceMemUsage` and `CommonHandle.ExtraMemSize` read; the port uses the
//!   vectors' capacities and gives every buffer Go builds with `make` exactly Go's capacity
//!   (`append` uses Go's growth rule, [`tidb_base::gostd::runtime::growslice_cap`]).
//! - `Handle` is an interface with three implementations in v8.5.8 (`IntHandle`,
//!   `*CommonHandle`, `PartitionHandle`); the port is a closed enum (M-IFACE). A
//!   `PartitionHandle` embeds its handle, so the methods it does not override are the
//!   embedded handle's, as in Go (`PartitionHandle.Next()` returns the embedded handle's
//!   `Next()`, which is not a `PartitionHandle`). `*CommonHandle` is shared, so it is an
//!   `Arc`.
//! - `HandleMap` stores `any` in Go; the port is generic over the value type.
//! - Go's maps iterate in random order; `Range` here iterates in `HashMap` order
//!   (M-MAPORDER): callers that stop early see an unspecified subset in both languages.
//! - Go panics (`IntValue` on a `CommonHandle`, `Compare` across kinds) are Rust panics with
//!   the same message (M-PANIC).

// Go identifiers for the size constants (M-GONAME).
#![allow(non_upper_case_globals)]

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::gostd::runtime::growslice_cap;
use tidb_types::datum::Datum;
use tidb_types::datum::new_int_datum;

use crate::hack::GoLayout;
use crate::hack::MemAwareMap;

/// `bytes.Compare` as Go's `-1 / 0 / +1`.
// go-std: bytes.Compare L1
pub(crate) fn bytes_compare(a: &[u8], b: &[u8]) -> isize {
    match a.cmp(b) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

/// `make([]byte, n)` followed by `copy`: a vector of capacity exactly `n` (Go's `make`).
// glue: Go make + copy with Go's capacity
fn make_copy(src: &[u8], n: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(n);
    buf.extend_from_slice(&src[..src.len().min(n)]);
    buf.resize(n, 0);
    buf
}

/// `append(buf, b)` with Go's capacity growth.
// glue: Go append of one byte (runtime.growslice capacity)
fn go_append_byte(buf: &mut Vec<u8>, b: u8) {
    if buf.len() == buf.capacity() {
        let cap = growslice_cap(buf.len() + 1, buf.capacity(), 1);
        buf.reserve_exact(cap - buf.len());
    }
    buf.push(b);
}

/// Key represents high-level Key type.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(pub Vec<u8>);

impl std::ops::Deref for Key {
    type Target = [u8];
    // glue: Go's Key is a []byte
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for Key {
    // glue: Go conversion Key(b)
    fn from(v: Vec<u8>) -> Key {
        Key(v)
    }
}

impl From<&[u8]> for Key {
    // glue: Go conversion Key(b) (the port copies)
    fn from(v: &[u8]) -> Key {
        Key(v.to_vec())
    }
}

impl Key {
    /// Next returns the next key in byte-order.
    // go: Key.Next L1
    pub fn next(&self) -> Key {
        // add 0x0 to the end of key
        Key(make_copy(&self.0, self.0.len() + 1))
    }

    /// PrefixNext returns the next prefix key.
    ///
    /// Assume there are keys like:
    ///
    /// ```text
    /// rowkey1
    /// rowkey1_column1
    /// rowkey1_column2
    /// rowKey2
    /// ```
    ///
    /// If we seek 'rowkey1' Next, we will get 'rowkey1_column1'.
    /// If we seek 'rowkey1' PrefixNext, we will get 'rowkey2'.
    // go: Key.PrefixNext L1
    pub fn prefix_next(&self) -> Key {
        let k = &self.0;
        let mut buf = make_copy(k, k.len());
        let mut i: isize = k.len() as isize - 1;
        while i >= 0 {
            buf[i as usize] = buf[i as usize].wrapping_add(1);
            if buf[i as usize] != 0 {
                break;
            }
            i -= 1;
        }
        if i == -1 {
            buf.copy_from_slice(k);
            go_append_byte(&mut buf, 0); // nozero
        }
        Key(buf)
    }

    /// Cmp returns the comparison result of two key.
    /// The result will be 0 if a==b, -1 if a < b, and +1 if a > b.
    // go: Key.Cmp L1
    #[allow(clippy::should_implement_trait)] // Go name (M-NAME): Cmp returns -1/0/+1
    pub fn cmp(&self, another: &[u8]) -> isize {
        bytes_compare(&self.0, another)
    }

    /// HasPrefix tests whether the Key begins with prefix.
    // go: Key.HasPrefix L1
    pub fn has_prefix(&self, prefix: &[u8]) -> bool {
        self.0.starts_with(prefix)
    }

    /// Clone returns a deep copy of the Key.
    // go: Key.Clone L1
    pub fn clone_key(&self) -> Key {
        Key(make_copy(&self.0, self.0.len()))
    }

    /// String implements fmt.Stringer interface.
    // go: Key.String L1
    pub fn string(&self) -> String {
        let mut s = String::with_capacity(self.0.len() * 2);
        for b in &self.0 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}

impl std::fmt::Display for Key {
    // glue: fmt.Stringer -> Display (M-NAME)
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.string())
    }
}

/// KeyRange represents a range where StartKey <= key < EndKey.
///
/// Go makes the layout exactly the same as kvproto's `coprocessor.KeyRange` (three
/// protobuf bookkeeping fields) to avoid an allocation when converting; the port has no
/// such fields. `XXXunrecognized` is always nil in TiDB, so `KeyRangeSliceMemUsage` is
/// unchanged; its `unsafe.Sizeof(KeyRange{})` is Go's 80.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct KeyRange {
    pub start_key: Key,
    pub end_key: Key,
}

/// Go `unsafe.Sizeof(KeyRange{})` (two slices, a zero-size struct, a slice and an int32).
pub const SizeofKeyRange: i64 = 80;

/// KeyRangeSliceMemUsage return the memory usage of []KeyRange
// go: KeyRangeSliceMemUsage L1
pub fn key_range_slice_mem_usage(k: &Vec<KeyRange>) -> i64 {
    let mut res = SizeofKeyRange * k.capacity() as i64;
    for m in k {
        res += m.start_key.0.capacity() as i64 + m.end_key.0.capacity() as i64;
    }
    res
}

/// Entry is the entry for key and value
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    pub key: Key,
    pub value: Vec<u8>,
}

/// Handle is the ID of a row.
///
/// Go: an interface implemented by `IntHandle`, `*CommonHandle` and `PartitionHandle`.
#[derive(Clone, Debug)]
pub enum Handle {
    /// IntHandle implement the Handle interface for int64 type handle.
    Int(IntHandle),
    /// CommonHandle implements the Handle interface for non-int64 type handle.
    Common(Arc<CommonHandle>),
    /// PartitionHandle combines a handle and a PartitionID, used to location a row in
    /// partitioned table.
    Partition(PartitionHandle),
}

/// IntHandle implement the Handle interface for int64 type handle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct IntHandle(pub i64);

/// CommonHandle implements the Handle interface for non-int64 type handle.
#[derive(Debug, Default)]
pub struct CommonHandle {
    encoded: Vec<u8>,
    /// Shared with the handle `Next` derives from it, as Go shares the slice.
    col_end_offsets: Arc<Vec<u16>>,
}

/// PartitionHandle combines a handle and a PartitionID, used to location a row in
/// partitioned table.
#[derive(Clone, Debug)]
pub struct PartitionHandle {
    pub handle: Box<Handle>,
    pub partition_id: i64,
}

/// Go `unsafe.Sizeof(CommonHandle{})`: two slices.
const SizeofCommonHandle: u64 = 48;
/// Go `unsafe.Sizeof(PartitionHandle{})`: an interface and an int64.
const SizeofPartitionHandle: u64 = 24;
/// Go `unsafe.Sizeof(IntHandle(0))`.
const SizeofIntHandle: u64 = 8;

impl From<IntHandle> for Handle {
    // glue: IntHandle as a Handle value
    fn from(h: IntHandle) -> Handle {
        Handle::Int(h)
    }
}

impl From<PartitionHandle> for Handle {
    // glue: PartitionHandle as a Handle value
    fn from(h: PartitionHandle) -> Handle {
        Handle::Partition(h)
    }
}

impl From<CommonHandle> for Handle {
    // glue: *CommonHandle as a Handle value
    fn from(h: CommonHandle) -> Handle {
        Handle::Common(Arc::new(h))
    }
}

#[allow(clippy::len_without_is_empty)] // Go has Len and no IsEmpty
impl IntHandle {
    /// Copy implements the Handle interface.
    // go: IntHandle.Copy L1
    pub fn copy(self) -> Handle {
        Handle::Int(self)
    }

    /// IsInt implements the Handle interface.
    // go: IntHandle.IsInt L1
    pub fn is_int(self) -> bool {
        true
    }

    /// IntValue implements the Handle interface.
    // go: IntHandle.IntValue L1
    pub fn int_value(self) -> i64 {
        self.0
    }

    /// Next implements the Handle interface.
    // go: IntHandle.Next L1 [M-INT]
    pub fn next(self) -> Handle {
        Handle::Int(IntHandle(self.0.wrapping_add(1)))
    }

    /// Equal implements the Handle interface.
    // go: IntHandle.Equal L1
    pub fn equal(self, h: &Handle) -> bool {
        h.is_int() && self.0 == h.int_value()
    }

    /// Compare implements the Handle interface.
    // go: IntHandle.Compare L1 [M-PANIC]
    pub fn compare(self, h: &Handle) -> isize {
        if !h.is_int() {
            panic!("IntHandle compares to CommonHandle");
        }
        let ih_val = self.int_value();
        let h_val = h.int_value();
        if ih_val > h_val {
            return 1;
        }
        if ih_val < h_val {
            return -1;
        }
        0
    }

    /// Encoded implements the Handle interface.
    // go: IntHandle.Encoded L1
    pub fn encoded(self) -> Vec<u8> {
        let mut b = Vec::new();
        tidb_codec::encode_int(&mut b, self.0);
        b
    }

    /// Len implements the Handle interface.
    // go: IntHandle.Len L1
    pub fn len(self) -> isize {
        8
    }

    /// NumCols implements the Handle interface, not supported for IntHandle type.
    // go: IntHandle.NumCols L1 [M-PANIC]
    pub fn num_cols(self) -> isize {
        panic!("not supported in IntHandle");
    }

    /// EncodedCol implements the Handle interface., not supported for IntHandle type.
    // go: IntHandle.EncodedCol L1 [M-PANIC]
    pub fn encoded_col(self, _idx: isize) -> Vec<u8> {
        panic!("not supported in IntHandle");
    }

    /// Data implements the Handle interface.
    // go: IntHandle.Data L1
    pub fn data(self) -> Result<Vec<Datum>, Error> {
        Ok(vec![new_int_datum(self.0)])
    }

    /// String implements the Handle interface.
    // go: IntHandle.String L1
    pub fn string(self) -> String {
        self.0.to_string()
    }

    /// MemUsage implements the Handle interface.
    // go: IntHandle.MemUsage L1
    pub fn mem_usage(self) -> u64 {
        SizeofIntHandle
    }

    /// ExtraMemSize implements the Handle interface.
    // go: IntHandle.ExtraMemSize L1
    pub fn extra_mem_size(self) -> u64 {
        0
    }
}

/// NewCommonHandle creates a CommonHandle from a encoded bytes which is encoded by code.EncodeKey.
// go: NewCommonHandle L1
pub fn new_common_handle(encoded: Vec<u8>) -> Result<CommonHandle, Error> {
    let mut col_end_offsets: Vec<u16> = Vec::new();
    {
        let mut remain: &[u8] = &encoded;
        let mut end_off = 0u16;
        while !remain.is_empty() {
            if remain[0] == 0 {
                // padded data
                break;
            }
            let (col, rest) = tidb_codec::cut_one(remain)?;
            remain = rest;
            end_off = end_off.wrapping_add(col.len() as u16);
            // Go `append`: runtime.growslice's capacity (CommonHandle.ExtraMemSize reads it).
            if col_end_offsets.len() == col_end_offsets.capacity() {
                let cap = growslice_cap(col_end_offsets.len() + 1, col_end_offsets.capacity(), 2);
                col_end_offsets.reserve_exact(cap - col_end_offsets.len());
            }
            col_end_offsets.push(end_off);
        }
    }
    let encoded = if encoded.len() < 9 {
        make_copy(&encoded, 9)
    } else {
        encoded
    };
    Ok(CommonHandle {
        encoded,
        col_end_offsets: Arc::new(col_end_offsets),
    })
}

#[allow(clippy::len_without_is_empty)] // Go has Len and no IsEmpty
impl CommonHandle {
    /// Copy implements the Handle interface.
    // go: CommonHandle.Copy L1
    pub fn copy(&self) -> Handle {
        let encoded = make_copy(&self.encoded, self.encoded.len());
        let mut col_end_offsets = Vec::with_capacity(self.col_end_offsets.len());
        col_end_offsets.extend_from_slice(&self.col_end_offsets);
        Handle::Common(Arc::new(CommonHandle {
            encoded,
            col_end_offsets: Arc::new(col_end_offsets),
        }))
    }

    /// IsInt implements the Handle interface.
    // go: CommonHandle.IsInt L1
    pub fn is_int(&self) -> bool {
        false
    }

    /// IntValue implements the Handle interface, not supported for CommonHandle type.
    // go: CommonHandle.IntValue L1 [M-PANIC]
    pub fn int_value(&self) -> i64 {
        panic!("not supported in CommonHandle");
    }

    /// Next implements the Handle interface.
    /// Note that the returned encoded field is not guaranteed to be able to decode.
    // go: CommonHandle.Next L1
    pub fn next(&self) -> Handle {
        Handle::Common(Arc::new(CommonHandle {
            encoded: Key(self.encoded.clone()).prefix_next().0,
            col_end_offsets: Arc::clone(&self.col_end_offsets),
        }))
    }

    /// Equal implements the Handle interface.
    // go: CommonHandle.Equal L1
    pub fn equal(&self, h: &Handle) -> bool {
        !h.is_int() && self.encoded[..] == h.encoded()[..]
    }

    /// Compare implements the Handle interface.
    // go: CommonHandle.Compare L1 [M-PANIC]
    pub fn compare(&self, h: &Handle) -> isize {
        if h.is_int() {
            panic!("CommonHandle compares to IntHandle");
        }
        bytes_compare(&self.encoded, &h.encoded())
    }

    /// Encoded implements the Handle interface.
    // go: CommonHandle.Encoded L1
    pub fn encoded(&self) -> &[u8] {
        &self.encoded
    }

    /// Len implements the Handle interface.
    // go: CommonHandle.Len L1
    pub fn len(&self) -> isize {
        self.encoded.len() as isize
    }

    /// NumCols implements the Handle interface.
    // go: CommonHandle.NumCols L1
    pub fn num_cols(&self) -> isize {
        self.col_end_offsets.len() as isize
    }

    /// EncodedCol implements the Handle interface.
    // go: CommonHandle.EncodedCol L1 [M-PANIC]
    pub fn encoded_col(&self, idx: isize) -> &[u8] {
        let mut col_start_offset = 0u16;
        if idx > 0 {
            col_start_offset = self.col_end_offsets[(idx - 1) as usize];
        }
        &self.encoded[col_start_offset as usize..self.col_end_offsets[idx as usize] as usize]
    }

    /// Data implements the Handle interface.
    // go: CommonHandle.Data L1
    pub fn data(&self) -> Result<Vec<Datum>, Error> {
        let mut data = Vec::with_capacity(self.num_cols() as usize);
        for i in 0..self.num_cols() {
            let encoded_col = self.encoded_col(i);
            let (_, d, err) = tidb_codec::decode_one(encoded_col);
            err?;
            data.push(d);
        }
        Ok(data)
    }

    /// String implements the Handle interface.
    // go: CommonHandle.String L1
    pub fn string(&self) -> String {
        let data = match self.data() {
            Ok(data) => data,
            Err(err) => return String::from_utf8_lossy(&err.error()).into_owned(),
        };
        let mut strs: Vec<String> = Vec::with_capacity(self.num_cols() as usize);
        for datum in &data {
            match datum.to_string() {
                Ok(s) => strs.push(String::from_utf8_lossy(&s).into_owned()),
                Err(err) => return String::from_utf8_lossy(&err.error()).into_owned(),
            }
        }
        format!("{{{}}}", strs.join(", "))
    }

    /// MemUsage implements the Handle interface.
    // go: CommonHandle.MemUsage L1
    pub fn mem_usage(&self) -> u64 {
        // 48 is used by the 2 slice fields.
        SizeofCommonHandle + self.extra_mem_size()
    }

    /// ExtraMemSize implements the Handle interface.
    // go: CommonHandle.ExtraMemSize L1
    pub fn extra_mem_size(&self) -> u64 {
        // colEndOffsets is a slice of uint16.
        self.encoded.capacity() as u64 + self.col_end_offsets.capacity() as u64 * 2
    }
}

impl PartitionHandle {
    /// Copy implements the Handle interface.
    // go: PartitionHandle.Copy L1
    pub fn copy(&self) -> Handle {
        Handle::Partition(PartitionHandle {
            handle: Box::new(self.handle.copy()),
            partition_id: self.partition_id,
        })
    }

    /// Equal implements the Handle interface.
    // go: PartitionHandle.Equal L1
    pub fn equal(&self, h: &Handle) -> bool {
        // Compare pid and handle if both sides are `PartitionHandle`.
        if let Handle::Partition(ph2) = h {
            return self.partition_id == ph2.partition_id && self.handle.equal(&ph2.handle);
        }

        // Otherwise, use underlying handle to do comparation.
        self.handle.equal(h)
    }

    /// Compare implements the Handle interface.
    // go: PartitionHandle.Compare L1 [M-PANIC]
    pub fn compare(&self, h: &Handle) -> isize {
        if let Handle::Partition(ph2) = h {
            if self.partition_id < ph2.partition_id {
                return -1;
            }
            if self.partition_id > ph2.partition_id {
                return 1;
            }
            return self.handle.compare(&ph2.handle);
        }
        panic!("PartitonHandle compares to non-parition Handle");
    }

    /// MemUsage implements the Handle interface.
    // go: PartitionHandle.MemUsage L1
    pub fn mem_usage(&self) -> u64 {
        self.handle.mem_usage() + SizeofPartitionHandle
    }

    /// ExtraMemSize implements the Handle interface.
    // go: PartitionHandle.ExtraMemSize L1
    pub fn extra_mem_size(&self) -> u64 {
        self.handle.extra_mem_size()
    }
}

/// NewPartitionHandle creates a PartitionHandle from a normal handle and a pid.
// go: NewPartitionHandle L1
pub fn new_partition_handle(pid: i64, h: Handle) -> PartitionHandle {
    PartitionHandle {
        handle: Box::new(h),
        partition_id: pid,
    }
}

#[allow(clippy::len_without_is_empty)] // Go has Len and no IsEmpty
impl Handle {
    /// IsInt returns if the handle type is int64.
    // glue: Handle interface dispatch
    pub fn is_int(&self) -> bool {
        match self {
            Handle::Int(h) => h.is_int(),
            Handle::Common(h) => h.is_int(),
            Handle::Partition(h) => h.handle.is_int(),
        }
    }

    /// IntValue returns the int64 value if IsInt is true, it panics if IsInt returns false.
    // glue: Handle interface dispatch
    pub fn int_value(&self) -> i64 {
        match self {
            Handle::Int(h) => h.int_value(),
            Handle::Common(h) => h.int_value(),
            Handle::Partition(h) => h.handle.int_value(),
        }
    }

    /// Next returns the minimum handle that is greater than this handle.
    /// The returned handle is not guaranteed to be able to decode.
    // glue: Handle interface dispatch (PartitionHandle: the embedded handle's Next)
    pub fn next(&self) -> Handle {
        match self {
            Handle::Int(h) => h.next(),
            Handle::Common(h) => h.next(),
            Handle::Partition(h) => h.handle.next(),
        }
    }

    /// Equal returns if the handle equals to another handle, it panics if the types are
    /// different.
    // glue: Handle interface dispatch
    pub fn equal(&self, h: &Handle) -> bool {
        match self {
            Handle::Int(x) => x.equal(h),
            Handle::Common(x) => x.equal(h),
            Handle::Partition(x) => x.equal(h),
        }
    }

    /// Compare returns the comparison result of the two handles, it panics if the types are
    /// different.
    // glue: Handle interface dispatch
    pub fn compare(&self, h: &Handle) -> isize {
        match self {
            Handle::Int(x) => x.compare(h),
            Handle::Common(x) => x.compare(h),
            Handle::Partition(x) => x.compare(h),
        }
    }

    /// Encoded returns the encoded bytes.
    // glue: Handle interface dispatch
    pub fn encoded(&self) -> Cow<'_, [u8]> {
        match self {
            Handle::Int(h) => Cow::Owned(h.encoded()),
            Handle::Common(h) => Cow::Borrowed(h.encoded()),
            Handle::Partition(h) => h.handle.encoded(),
        }
    }

    /// Len returns the length of the encoded bytes.
    // glue: Handle interface dispatch
    pub fn len(&self) -> isize {
        match self {
            Handle::Int(h) => h.len(),
            Handle::Common(h) => h.len(),
            Handle::Partition(h) => h.handle.len(),
        }
    }

    /// NumCols returns the number of columns of the handle,
    // glue: Handle interface dispatch
    pub fn num_cols(&self) -> isize {
        match self {
            Handle::Int(h) => h.num_cols(),
            Handle::Common(h) => h.num_cols(),
            Handle::Partition(h) => h.handle.num_cols(),
        }
    }

    /// EncodedCol returns the encoded column value at the given column index.
    // glue: Handle interface dispatch
    pub fn encoded_col(&self, idx: isize) -> Cow<'_, [u8]> {
        match self {
            Handle::Int(h) => Cow::Owned(h.encoded_col(idx)),
            Handle::Common(h) => Cow::Borrowed(h.encoded_col(idx)),
            Handle::Partition(h) => h.handle.encoded_col(idx),
        }
    }

    /// Data returns the data of all columns of a handle.
    // glue: Handle interface dispatch
    pub fn data(&self) -> Result<Vec<Datum>, Error> {
        match self {
            Handle::Int(h) => h.data(),
            Handle::Common(h) => h.data(),
            Handle::Partition(h) => h.handle.data(),
        }
    }

    /// String implements the fmt.Stringer interface.
    // glue: Handle interface dispatch
    pub fn string(&self) -> String {
        match self {
            Handle::Int(h) => h.string(),
            Handle::Common(h) => h.string(),
            Handle::Partition(h) => h.handle.string(),
        }
    }

    /// MemUsage returns the memory usage of a handle.
    // glue: Handle interface dispatch
    pub fn mem_usage(&self) -> u64 {
        match self {
            Handle::Int(h) => h.mem_usage(),
            Handle::Common(h) => h.mem_usage(),
            Handle::Partition(h) => h.mem_usage(),
        }
    }

    /// ExtraMemSize returns the memory usage of objects that are pointed to by the Handle.
    // glue: Handle interface dispatch
    pub fn extra_mem_size(&self) -> u64 {
        match self {
            Handle::Int(h) => h.extra_mem_size(),
            Handle::Common(h) => h.extra_mem_size(),
            Handle::Partition(h) => h.extra_mem_size(),
        }
    }

    /// Copy returns a deep copy of the Handle.
    // glue: Handle interface dispatch
    pub fn copy(&self) -> Handle {
        match self {
            Handle::Int(h) => h.copy(),
            Handle::Common(h) => h.copy(),
            Handle::Partition(h) => h.copy(),
        }
    }
}

impl std::fmt::Display for Handle {
    // glue: fmt.Stringer -> Display (M-NAME)
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.string())
    }
}

/// Go sizes from `pkg/util/size` (64-bit).
const SizeOfString: i64 = 16;
const SizeOfInt64: i64 = 8;
const SizeOfInterface: i64 = 16;
const SizeOfMap: i64 = 8;

/// HandleMap is the map for Handle.
#[derive(Debug)]
pub struct HandleMap<V> {
    ints: HashMap<i64, V>,
    strs: HashMap<Vec<u8>, StrHandleVal<V>>,

    // Use two two-dimensional map to fit partitionHandle.
    // The first int64 is for partitionID.
    partition_ints: HashMap<i64, HashMap<i64, V>>,
    partition_strs: HashMap<i64, HashMap<Vec<u8>, StrHandleVal<V>>>,
}

#[derive(Debug)]
struct StrHandleVal<V> {
    h: Handle,
    val: V,
}

/// SizeofHandleMap presents the memory size of struct HandleMap (Go: four map pointers).
pub const SizeofHandleMap: i64 = 32;

/// SizeofStrHandleVal presents the memory size of struct strHandleVal (Go: two interfaces).
pub const SizeofStrHandleVal: i64 = 32;

/// NewHandleMap creates a new map for handle.
// go: NewHandleMap L1
pub fn new_handle_map<V>() -> HandleMap<V> {
    HandleMap {
        ints: HashMap::new(),
        strs: HashMap::new(),

        partition_ints: HashMap::new(),
        partition_strs: HashMap::new(),
    }
}

// go: calcStrsMemUsage L1
fn calc_strs_mem_usage<V>(strs: &HashMap<Vec<u8>, StrHandleVal<V>>) -> i64 {
    let mut res = 0i64;
    for key in strs.keys() {
        res += SizeOfString + key.len() as i64 + SizeofStrHandleVal;
    }
    res
}

// go: calcIntsMemUsage L1
fn calc_ints_mem_usage<V>(ints: &HashMap<i64, V>) -> i64 {
    ints.len() as i64 * (SizeOfInt64 + SizeOfInterface)
}

#[allow(clippy::len_without_is_empty)] // Go has Len and no IsEmpty
impl<V> HandleMap<V> {
    /// Get gets a value by a Handle.
    // go: HandleMap.Get L1
    pub fn get(&self, h: &Handle) -> Option<&V> {
        let (mut ints, mut strs) = (&self.ints, &self.strs);
        if let Handle::Partition(ph) = h {
            let idx = ph.partition_id;
            let pi = self.partition_ints.get(&idx);
            let ps = self.partition_strs.get(&idx);
            if (h.is_int() && pi.is_none()) || (!h.is_int() && ps.is_none()) {
                return None;
            }
            if h.is_int() {
                ints = pi?;
            } else {
                strs = ps?;
            }
        }
        if h.is_int() {
            ints.get(&h.int_value())
        } else {
            strs.get(&h.encoded()[..]).map(|s| &s.val)
        }
    }

    /// MemUsage gets the memory usage.
    // go: HandleMap.MemUsage L1
    pub fn mem_usage(&self) -> i64 {
        let mut res = SizeofHandleMap;
        res += self.partition_ints.len() as i64 * (SizeOfInt64 + SizeOfMap);
        for v in self.partition_ints.values() {
            res += calc_ints_mem_usage(v);
        }
        res += self.partition_strs.len() as i64 * (SizeOfInt64 + SizeOfMap);
        for v in self.partition_strs.values() {
            res += calc_strs_mem_usage(v);
        }
        res += calc_ints_mem_usage(&self.ints);
        res += calc_strs_mem_usage(&self.strs);
        res
    }

    /// Set sets a value with a Handle.
    // go: HandleMap.Set L1
    pub fn set(&mut self, h: &Handle, val: V) {
        let (ints, strs) = match h {
            Handle::Partition(ph) => {
                let idx = ph.partition_id;
                if h.is_int() {
                    (Some(self.partition_ints.entry(idx).or_default()), None)
                } else {
                    (None, Some(self.partition_strs.entry(idx).or_default()))
                }
            }
            _ => {
                if h.is_int() {
                    (Some(&mut self.ints), None)
                } else {
                    (None, Some(&mut self.strs))
                }
            }
        };
        if h.is_int() {
            ints.expect("int map").insert(h.int_value(), val);
        } else {
            strs.expect("str map")
                .insert(h.encoded().into_owned(), StrHandleVal { h: h.clone(), val });
        }
    }

    /// Delete deletes a entry from the map.
    // go: HandleMap.Delete L1
    pub fn delete(&mut self, h: &Handle) {
        let (mut ints, mut strs) = (Some(&mut self.ints), Some(&mut self.strs));
        if let Handle::Partition(ph) = h {
            let idx = ph.partition_id;
            if (h.is_int() && !self.partition_ints.contains_key(&idx))
                || (!h.is_int() && !self.partition_strs.contains_key(&idx))
            {
                return;
            }
            ints = self.partition_ints.get_mut(&idx);
            strs = self.partition_strs.get_mut(&idx);
        }
        if h.is_int() {
            if let Some(ints) = ints {
                ints.remove(&h.int_value());
            }
        } else if let Some(strs) = strs {
            strs.remove(&h.encoded()[..]);
        }
    }

    /// Len returns the length of the map.
    // go: HandleMap.Len L1
    pub fn len(&self) -> isize {
        let mut l = self.ints.len() + self.strs.len();
        for v in self.partition_ints.values() {
            l += v.len();
        }
        for v in self.partition_strs.values() {
            l += v.len();
        }
        l as isize
    }

    /// Range iterates the HandleMap with fn, the fn returns true to continue, returns false
    /// to stop.
    // go: HandleMap.Range L1 [M-MAPORDER]
    pub fn range(&self, mut f: impl FnMut(&Handle, &V) -> bool) {
        for (h, val) in &self.ints {
            if !f(&Handle::Int(IntHandle(*h)), val) {
                return;
            }
        }
        for str_val in self.strs.values() {
            if !f(&str_val.h, &str_val.val) {
                return;
            }
        }
        for (pid, v) in &self.partition_ints {
            for (h, val) in v {
                let ph = Handle::Partition(new_partition_handle(*pid, Handle::Int(IntHandle(*h))));
                if !f(&ph, val) {
                    return;
                }
            }
        }
        for v in self.partition_strs.values() {
            for str_val in v.values() {
                if !f(&str_val.h, &str_val.val) {
                    return;
                }
            }
        }
    }
}

/// MemAwareHandleMap is similar to HandleMap, but it's aware of its memory usage and doesn't
/// support delete. It only tracks the actual sizes. Objects that are pointed to by the key
/// or value are not tracked. Those should be tracked by the caller.
///
/// The sizes are Go's (`hack.MemAwareMap`, see [`crate::hack`]): `V` gives its Go layout.
#[derive(Debug)]
pub struct MemAwareHandleMap<V: GoLayout> {
    ints: MemAwareMap<i64, V>,
    strs: MemAwareMap<Vec<u8>, StrHandleValue<V>>,

    partition_ints: HashMap<i64, MemAwareMap<i64, V>>,
    partition_strs: HashMap<i64, MemAwareMap<Vec<u8>, StrHandleValue<V>>>,
}

#[derive(Debug)]
struct StrHandleValue<V> {
    h: Handle,
    val: V,
}

impl<V: GoLayout> GoLayout for StrHandleValue<V> {
    // Go: struct { h Handle; val V } (an interface, then V).
    const GO_ALIGN: u64 = if V::GO_ALIGN > 8 { V::GO_ALIGN } else { 8 };
    const GO_SIZE: u64 = {
        let off = 16u64.div_ceil(V::GO_ALIGN) * V::GO_ALIGN;
        let end = off + V::GO_SIZE;
        end.div_ceil(Self::GO_ALIGN) * Self::GO_ALIGN
    };
}

/// NewMemAwareHandleMap creates a new map for handle.
// go: NewMemAwareHandleMap L1
pub fn new_mem_aware_handle_map<V: GoLayout>() -> MemAwareHandleMap<V> {
    let mut res = MemAwareHandleMap {
        ints: MemAwareMap::default(),
        strs: MemAwareMap::default(),
        partition_ints: HashMap::new(),
        partition_strs: HashMap::new(),
    };
    res.ints.init(HashMap::new());
    res.strs.init(HashMap::new());
    res
}

impl<V: GoLayout> MemAwareHandleMap<V> {
    /// Get gets a value by a Handle.
    // go: MemAwareHandleMap.Get L1
    pub fn get(&self, h: &Handle) -> Option<&V> {
        let (mut ints, mut strs) = (&self.ints.m, &self.strs.m);
        if let Handle::Partition(ph) = h {
            let idx = ph.partition_id;
            if h.is_int() {
                ints = &self.partition_ints.get(&idx)?.m;
            } else {
                strs = &self.partition_strs.get(&idx)?.m;
            }
        }
        if h.is_int() {
            ints.get(&h.int_value())
        } else {
            strs.get(&h.encoded()[..]).map(|s| &s.val)
        }
    }

    /// Set sets a value with a Handle.
    // go: MemAwareHandleMap.Set L1
    pub fn set(&mut self, h: &Handle, val: V) -> i64 {
        let (ints, strs) = match h {
            Handle::Partition(ph) => {
                let idx = ph.partition_id;
                if h.is_int() {
                    (
                        Some(
                            self.partition_ints
                                .entry(idx)
                                .or_insert_with(new_mem_aware_map),
                        ),
                        None,
                    )
                } else {
                    (
                        None,
                        Some(
                            self.partition_strs
                                .entry(idx)
                                .or_insert_with(new_mem_aware_map),
                        ),
                    )
                }
            }
            _ => {
                if h.is_int() {
                    (Some(&mut self.ints), None)
                } else {
                    (None, Some(&mut self.strs))
                }
            }
        };
        if h.is_int() {
            return ints.expect("int map").set(h.int_value(), val);
        }
        strs.expect("str map").set(
            h.encoded().into_owned(),
            StrHandleValue { h: h.clone(), val },
        )
    }

    /// Range iterates the MemAwareHandleMap with fn, the fn returns true to continue,
    /// returns false to stop.
    // go: MemAwareHandleMap.Range L1 [M-MAPORDER]
    pub fn range(&self, mut f: impl FnMut(&Handle, &V) -> bool) {
        for (h, val) in &self.ints.m {
            if !f(&Handle::Int(IntHandle(*h)), val) {
                return;
            }
        }
        for str_val in self.strs.m.values() {
            if !f(&str_val.h, &str_val.val) {
                return;
            }
        }
        for (pid, v) in &self.partition_ints {
            for (h, val) in &v.m {
                let ph = Handle::Partition(new_partition_handle(*pid, Handle::Int(IntHandle(*h))));
                if !f(&ph, val) {
                    return;
                }
            }
        }
        for v in self.partition_strs.values() {
            for str_val in v.m.values() {
                if !f(&str_val.h, &str_val.val) {
                    return;
                }
            }
        }
    }

    /// The bytes accounted so far (Go: the `Bytes` fields of the inner maps, which the
    /// callers read through the deltas `Set` returns).
    // glue: test access to the accounted sizes
    pub fn accounted_bytes(&self) -> u64 {
        self.ints.bytes
            + self.strs.bytes
            + self.partition_ints.values().map(|m| m.bytes).sum::<u64>()
            + self.partition_strs.values().map(|m| m.bytes).sum::<u64>()
    }
}

// go: newMemAwareMap L1
fn new_mem_aware_map<K: Eq + Hash + GoLayout, V: GoLayout>() -> MemAwareMap<K, V> {
    crate::hack::new_mem_aware_map(0)
}
