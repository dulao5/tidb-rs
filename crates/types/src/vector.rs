//! go-file: pkg/types/vector.go
//!
//! `VectorFloat32`, the value of the `VECTOR` column type: the dimension (u32, little
//! endian) followed by the f32 elements (little endian), the same bytes in memory and in
//! storage. Go aliases the elements with `unsafe.Slice` on `&data[4]`, whatever its
//! alignment; Rust reads and writes them through the [`Elements`] / [`ElementsMut`] views,
//! one unaligned little-endian load or store per element (no `unsafe`, crate-wide
//! `forbid(unsafe_code)`). The elements themselves are not copied.
//!
//! Go's zero-copy functions return vectors that alias the caller's bytes; here the vector
//! borrows them (`VectorFloat32<'a>` holds a `Cow<'a, [u8]>`), and the owning
//! constructors return `VectorFloat32<'static>`.

#![allow(non_upper_case_globals)]

use std::borrow::Cow;

use tidb_base::gofmt::Arg;
use tidb_base::gostd::strconv::format_float;
use tidb_base::{Error, errors};

use crate::jsoniter;

// Go `init` panics on big-endian machines ("VectorFloat32 only supports little endian").
// go: init L2 (compile-time assertion instead of a panic at init)
const _: () = assert!(
    cfg!(target_endian = "little"),
    "VectorFloat32 only supports little endian"
);

/// `types.UnspecifiedLength` (pkg/parser/types/etc.go, block D02.2; tidb-types does not
/// depend on tidb-parser).
// glue: pkg/parser/types.UnspecifiedLength
const UnspecifiedLength: isize = -1;

/// CreateVectorFloat32 creates a VectorFloat32. Returns error if there are invalid values like NaN and Inf.
// go: CreateVectorFloat32 L1 [M-ERR]
pub fn create_vector_float32(vector: &[f32]) -> Result<VectorFloat32<'static>, Error> {
    for &v in vector {
        if (v as f64).is_nan() {
            let value_error = errors::errorf("NaN not allowed in vector", &[]);
            return Err(value_error);
        }
        if (v as f64).is_infinite() {
            let value_error = errors::errorf("infinite value not allowed in vector", &[]);
            return Err(value_error);
        }
    }
    let mut vec = init_vector_float32(vector.len());
    vec.elements_mut().copy_from(vector);
    Ok(vec)
}

/// MustCreateVectorFloat32 creates a VectorFloat32. Panics if there are invalid values like NaN and Inf.
// go: MustCreateVectorFloat32 L1
pub fn must_create_vector_float32(v: &[f32]) -> VectorFloat32<'static> {
    match create_vector_float32(v) {
        Ok(r) => r,
        Err(err) => panic!("{err}"),
    }
}

/// VectorFloat32 represents a vector of float32.
///
/// Memory Format:
/// 4 byte       - Length
/// 4 byte * N   - Data in Float32
///
/// Normally, the data layout in storage (i.e. after serialization) is identical
/// to the memory layout. However, in BigEndian machines, we have BigEndian in
/// memory and always have LittleEndian in storage or during data exchange.
#[derive(Debug, PartialEq, Eq)]
pub struct VectorFloat32<'a> {
    data: Cow<'a, [u8]>, // Note: data must be a well-formatted byte slice (len >= 4)
}

/// ZeroVectorFloat32 is a zero value of VectorFloat32.
pub static ZeroVectorFloat32: VectorFloat32<'static> = VectorFloat32 {
    data: Cow::Borrowed(&[0, 0, 0, 0]),
};

/// InitVectorFloat32 initializes a vector with the given dimension. The values are initialized to zero.
// go: InitVectorFloat32 L1 [M-INT]
pub fn init_vector_float32(dims: usize) -> VectorFloat32<'static> {
    let mut data = vec![0u8; 4 + dims * 4];
    data[..4].copy_from_slice(&(dims as u32).to_le_bytes());
    VectorFloat32 {
        data: Cow::Owned(data),
    }
}

/// CheckVectorDimValid checks if the vector's dimension is valid.
// go: CheckVectorDimValid L1 [M-ERR]
pub fn check_vector_dim_valid(dim: isize) -> Result<(), Error> {
    const maxVectorDimension: isize = 16383;
    if dim < 0 {
        return Err(errors::errorf(
            "dimensions for type vector must be at least 0",
            &[],
        ));
    }
    if dim > maxVectorDimension {
        return Err(errors::errorf(
            "vector cannot have more than %d dimensions",
            &[Arg::from(maxVectorDimension)],
        ));
    }
    Ok(())
}

/// The elements of a vector (Go: the `[]float32` of `Elements()`), read in place.
#[derive(Debug, Clone, Copy)]
pub struct Elements<'a>(&'a [u8]);

impl<'a> Elements<'a> {
    // glue: len() of the Go slice
    pub fn len(&self) -> usize {
        self.0.len() / 4
    }

    // glue: len() == 0 of the Go slice
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Element `i` (Go `v[i]`; panics out of range like Go).
    // glue: Go slice index
    #[inline]
    pub fn get(&self, i: usize) -> f32 {
        let b = &self.0[i * 4..i * 4 + 4];
        f32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }

    // glue: `for _, v := range elements`
    #[inline]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = f32> + 'a {
        self.0
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
    }

    /// `elements[:n]`.
    // glue: Go reslice
    pub fn prefix(&self, n: usize) -> Elements<'a> {
        Elements(&self.0[..n * 4])
    }

    // glue: copy into a Go []float32
    pub fn to_vec(&self) -> Vec<f32> {
        self.iter().collect()
    }
}

/// The elements of a vector, written in place (Go: assignments to `Elements()[i]`).
#[derive(Debug)]
pub struct ElementsMut<'a>(&'a mut [u8]);

impl ElementsMut<'_> {
    // glue: len() of the Go slice
    pub fn len(&self) -> usize {
        self.0.len() / 4
    }

    // glue: len() == 0 of the Go slice
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    // glue: Go slice index
    #[inline]
    pub fn get(&self, i: usize) -> f32 {
        let b = &self.0[i * 4..i * 4 + 4];
        f32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }

    /// `elements[i] = v` (panics out of range like Go).
    // glue: Go slice element assignment
    #[inline]
    pub fn set(&mut self, i: usize, v: f32) {
        self.0[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// `copy(elements, src)`: copies `min(len, src.len())` elements.
    // glue: Go copy
    pub fn copy_from(&mut self, src: &[f32]) -> usize {
        let n = self.len().min(src.len());
        for (i, &v) in src[..n].iter().enumerate() {
            self.set(i, v);
        }
        n
    }
}

impl<'a> VectorFloat32<'a> {
    /// CheckDimsFitColumn checks if the vector has the expected dimension, which is defined by the column type or cast type.
    // go: VectorFloat32.CheckDimsFitColumn L1 [M-ERR]
    pub fn check_dims_fit_column(&self, expected_flen: isize) -> Result<(), Error> {
        if expected_flen != UnspecifiedLength && self.len() as isize != expected_flen {
            return Err(errors::errorf(
                "vector has %d dimensions, does not fit VECTOR(%d)",
                &[Arg::from(self.len()), Arg::from(expected_flen)],
            ));
        }
        Ok(())
    }

    /// Len returns the length (dimension) of the vector.
    // go: VectorFloat32.Len L1
    #[allow(clippy::len_without_is_empty)] // Go's is IsZeroValue
    #[inline]
    pub fn len(&self) -> usize {
        let d = &self.data;
        u32::from_le_bytes([d[0], d[1], d[2], d[3]]) as usize
    }

    /// Elements returns a mutable typed slice of the elements.
    ///
    /// Read-only view here; see [`VectorFloat32::elements_mut`]. Panics like Go when the
    /// length word exceeds the data (Go: `&v.data[4]`).
    // go: VectorFloat32.Elements L2 (a view type instead of an aliasing []float32; elements_mut for writes)
    #[inline]
    pub fn elements(&self) -> Elements<'_> {
        let l = self.len();
        if l == 0 {
            return Elements(&[]);
        }
        Elements(&self.data[4..4 + l * 4])
    }

    /// The writable form of `Elements()`: the vector's bytes become owned (copied once if
    /// they were borrowed, Go would write through to the aliased bytes).
    // go-part: VectorFloat32.Elements
    pub fn elements_mut(&mut self) -> ElementsMut<'_> {
        let l = self.len();
        if l == 0 {
            return ElementsMut(&mut []);
        }
        let data = self.data.to_mut();
        ElementsMut(&mut data[4..4 + l * 4])
    }

    /// TruncatedString prints the vector in a truncated form, which is useful for
    /// outputting in logs or EXPLAIN statements.
    // go: VectorFloat32.TruncatedString L1
    pub fn truncated_string(&self) -> String {
        const maxDisplayElements: usize = 5;

        let mut truncated_elements = 0;
        let mut elements = self.elements();

        if elements.len() > maxDisplayElements {
            truncated_elements = elements.len() - maxDisplayElements;
            elements = elements.prefix(maxDisplayElements);
        }

        let mut buf = String::with_capacity(2 + self.len() * 2);
        buf.push('[');
        for (i, v) in elements.iter().enumerate() {
            if i > 0 {
                buf.push(',');
            }
            buf.push_str(&format_float(v as f64, b'g', 2, 32));
        }
        if truncated_elements > 0 {
            buf.push_str(&format!(",({truncated_elements} more)..."));
        }
        buf.push(']');

        buf
    }

    /// String returns a string representation of the vector, which can be parsed later.
    // go: VectorFloat32.String L1 [M-NAME]
    #[allow(clippy::inherent_to_string)]
    pub fn string(&self) -> String {
        let elements = self.elements();

        let mut buf = String::with_capacity(2 + self.len() * 2);
        buf.push('[');
        for (i, v) in elements.iter().enumerate() {
            if i > 0 {
                buf.push(',');
            }
            buf.push_str(&format_float(v as f64, b'f', -1, 32));
        }
        buf.push(']');

        buf
    }

    /// ZeroCopySerialize serializes the vector into a new byte slice, without memory copy.
    // go: VectorFloat32.ZeroCopySerialize L1
    pub fn zero_copy_serialize(&self) -> &[u8] {
        &self.data
    }

    /// SerializeTo serializes the vector into the byte slice.
    // go: VectorFloat32.SerializeTo L1
    pub fn serialize_to(&self, mut b: Vec<u8>) -> Vec<u8> {
        b.extend_from_slice(&self.data);
        b
    }

    /// SerializedSize returns the size of the serialized data.
    // go: VectorFloat32.SerializedSize L1
    pub fn serialized_size(&self) -> usize {
        self.data.len()
    }

    /// EstimatedMemUsage returns the estimated memory usage.
    ///
    /// `unsafe.Sizeof(v)` is Go's: the struct is one slice header (24 bytes on the 64-bit
    /// targets), so memory accounting matches Go's.
    // go: VectorFloat32.EstimatedMemUsage L1
    pub fn estimated_mem_usage(&self) -> usize {
        const sizeofVectorFloat32: usize = 24;
        sizeofVectorFloat32 + self.data.len()
    }

    /// Clone returns a deep copy of the vector.
    // go: VectorFloat32.Clone L1 [M-NAME]
    #[allow(clippy::should_implement_trait)]
    pub fn clone(&self) -> VectorFloat32<'static> {
        VectorFloat32 {
            data: Cow::Owned(self.data.to_vec()),
        }
    }

    /// IsZeroValue returns true if the vector is a zero value (which length is zero).
    // go: VectorFloat32.IsZeroValue L1
    pub fn is_zero_value(&self) -> bool {
        self.len() == 0
    }
}

/// PeekBytesAsVectorFloat32 trys to peek some bytes from b, until
/// we can deserialize a VectorFloat32 from those bytes.
///
/// The size is computed in u32 as in Go (`elements*4 + 4` wraps for elements >= 2^30).
// go: PeekBytesAsVectorFloat32 L1 [M-ERR, M-INT]
#[inline]
pub fn peek_bytes_as_vector_float32(b: &[u8]) -> Result<usize, Error> {
    if b.len() < 4 {
        return Err(bad_vector_header(b.len()));
    }

    let elements = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let total_data_size = elements.wrapping_mul(4).wrapping_add(4);
    if b.len() < total_data_size as usize {
        return Err(bad_vector_value(b.len(), total_data_size));
    }
    Ok(total_data_size as usize)
}

// glue: the first error of PeekBytesAsVectorFloat32, out of line (cold path)
#[cold]
#[inline(never)]
fn bad_vector_header(len: usize) -> Error {
    errors::errorf("bad VectorFloat32 value header (len=%d)", &[Arg::from(len)])
}

// glue: the second error of PeekBytesAsVectorFloat32, out of line (cold path)
#[cold]
#[inline(never)]
fn bad_vector_value(len: usize, expected: u32) -> Error {
    errors::errorf(
        "bad VectorFloat32 value (len=%d, expected=%d)",
        &[Arg::from(len), Arg::from(expected)],
    )
}

/// ZeroCopyDeserializeVectorFloat32 deserializes the byte slice into a vector, without memory copy.
/// Note: b must not be mutated, because this function does zero copy.
///
/// Returns the vector and the remaining bytes; on error Go returns `ZeroVectorFloat32, b`.
// go: ZeroCopyDeserializeVectorFloat32 L1 [M-ERR]
#[inline]
pub fn zero_copy_deserialize_vector_float32(b: &[u8]) -> Result<(VectorFloat32<'_>, &[u8]), Error> {
    let length = peek_bytes_as_vector_float32(b)?;
    Ok((
        VectorFloat32 {
            data: Cow::Borrowed(&b[..length]),
        },
        &b[length..],
    ))
}

/// ParseVectorFloat32 parses a string into a vector.
// go: ParseVectorFloat32 L1 [M-ERR]
pub fn parse_vector_float32(s: &[u8]) -> Result<VectorFloat32<'static>, Error> {
    let mut values: Vec<f32> = Vec::new();
    let mut value_error: Option<Error> = None;
    // We explicitly use a JSON float parser to reject other JSON types.
    let mut parser = jsoniter::parse_string(s);
    parser.read_array_cb(|parser| {
        let v = parser.read_float64();
        if v.is_nan() {
            value_error = Some(errors::errorf("NaN not allowed in vector", &[]));
            return false;
        }
        if v.is_infinite() {
            value_error = Some(errors::errorf("infinite value not allowed in vector", &[]));
            return false;
        }
        values.push(v as f32);
        true
    });
    if parser.error.is_some() {
        return Err(errors::errorf(
            "Invalid vector text: %s",
            &[Arg::go_string(s.to_vec())],
        ));
    }
    if let Some(value_error) = value_error {
        return Err(value_error);
    }

    let dim = values.len();
    check_vector_dim_valid(dim as isize)?;

    let mut vec = init_vector_float32(dim);
    vec.elements_mut().copy_from(&values);
    Ok(vec)
}
