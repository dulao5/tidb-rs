//! Port of `github.com/dgryski/go-farm` (`v0.0.0-20240924180020-3414d57e47da`), `Hash64`
//! and its supporting routines (`basics.go`, `farmhashcc.go`, `farmhashna.go`,
//! `farmhashuo.go`, `farmhashxo.go`).
//!
//! Used by `APPROX_COUNT_DISTINCT` (`crates/executor/src/aggfuncs/func_count_distinct.rs`,
//! spec D12.4 §2.2, §3.3). Pure safe Rust with explicit wrapping arithmetic (`M-INT`) so
//! `Hash64` is bit-identical to Go `dgryski/go-farm` on both amd64 and arm64 under debug,
//! release, and `release-checked`.

// Some primes between 2^63 and 2^64 for various uses (basics.go).
const K0: u64 = 0xc3a5c85c97cb3127;
const K1: u64 = 0xb492b66fbe98f273;
const K2: u64 = 0x9ae16a3b2f90404f;

#[inline]
fn read_u64_le(s: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(s[off..off + 8].try_into().unwrap())
}

#[inline]
fn read_u32_le(s: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(s[off..off + 4].try_into().unwrap())
}

#[derive(Clone, Copy, Default)]
struct Uint128 {
    lo: u64,
    hi: u64,
}

// go-dep: github.com/dgryski/go-farm@v0.0.0-20240924180020-3414d57e47da.hash128to64 L1 [M-INT]
#[inline]
fn hash128to64(x: Uint128) -> u64 {
    // Murmur-inspired hashing.
    const MUL: u64 = 0x9ddfea08eb382d69;
    let mut a = (x.lo ^ x.hi).wrapping_mul(MUL);
    a ^= a >> 47;
    let mut b = (x.hi ^ a).wrapping_mul(MUL);
    b ^= b >> 47;
    b = b.wrapping_mul(MUL);
    b
}

// go-dep: github.com/dgryski/go-farm@v0.0.0-20240924180020-3414d57e47da.shiftMix L1
#[inline]
fn shift_mix(val: u64) -> u64 {
    val ^ (val >> 47)
}

// go-dep: github.com/dgryski/go-farm@v0.0.0-20240924180020-3414d57e47da.hashLen16 L1
#[inline]
fn hash_len16(u: u64, v: u64) -> u64 {
    hash128to64(Uint128 { lo: u, hi: v })
}

// go-dep: github.com/dgryski/go-farm@v0.0.0-20240924180020-3414d57e47da.hashLen16Mul L1 [M-INT]
#[inline]
fn hash_len16_mul(u: u64, v: u64, mul: u64) -> u64 {
    // Murmur-inspired hashing.
    let mut a = (u ^ v).wrapping_mul(mul);
    a ^= a >> 47;
    let mut b = (v ^ a).wrapping_mul(mul);
    b ^= b >> 47;
    b = b.wrapping_mul(mul);
    b
}

// go-dep: github.com/dgryski/go-farm@v0.0.0-20240924180020-3414d57e47da.hashLen0to16 L1 [M-INT]
fn hash_len0to16(s: &[u8]) -> u64 {
    let slen = s.len() as u64;
    if slen >= 8 {
        let mul = K2.wrapping_add(slen.wrapping_mul(2));
        let a = read_u64_le(s, 0).wrapping_add(K2);
        let b = read_u64_le(s, (slen - 8) as usize);
        let c = b.rotate_right(37).wrapping_mul(mul).wrapping_add(a);
        let d = a.rotate_right(25).wrapping_add(b).wrapping_mul(mul);
        return hash_len16_mul(c, d, mul);
    }

    if slen >= 4 {
        let mul = K2.wrapping_add(slen.wrapping_mul(2));
        let a = read_u32_le(s, 0);
        return hash_len16_mul(
            slen.wrapping_add(u64::from(a) << 3),
            u64::from(read_u32_le(s, (slen - 4) as usize)),
            mul,
        );
    }

    if slen > 0 {
        let a = s[0];
        let b = s[(slen >> 1) as usize];
        let c = s[(slen - 1) as usize];
        let y = u32::from(a).wrapping_add(u32::from(b) << 8);
        let z = (slen as u32).wrapping_add(u32::from(c) << 2);
        return shift_mix(u64::from(y).wrapping_mul(K2) ^ u64::from(z).wrapping_mul(K0))
            .wrapping_mul(K2);
    }

    K2
}

// go-dep: github.com/dgryski/go-farm@v0.0.0-20240924180020-3414d57e47da.hashLen17to32 L1 [M-INT]
fn hash_len17to32(s: &[u8]) -> u64 {
    let slen = s.len();
    let mul = K2.wrapping_add((slen.wrapping_mul(2)) as u64);
    let a = read_u64_le(s, 0).wrapping_mul(K1);
    let b = read_u64_le(s, 8);
    let c = read_u64_le(s, slen - 8).wrapping_mul(mul);
    let d = read_u64_le(s, slen - 16).wrapping_mul(K2);
    hash_len16_mul(
        a.wrapping_add(b)
            .rotate_right(43)
            .wrapping_add(c.rotate_right(30))
            .wrapping_add(d),
        a.wrapping_add(b.wrapping_add(K2).rotate_right(18))
            .wrapping_add(c),
        mul,
    )
}

// go-dep: github.com/dgryski/go-farm@v0.0.0-20240924180020-3414d57e47da.weakHashLen32WithSeedsWords L1 [M-INT]
#[inline]
fn weak_hash_len32_with_seeds_words(
    w: u64,
    x: u64,
    y: u64,
    z: u64,
    mut a: u64,
    mut b: u64,
) -> (u64, u64) {
    a = a.wrapping_add(w);
    b = b.wrapping_add(a).wrapping_add(z).rotate_right(21);
    let c = a;
    a = a.wrapping_add(x);
    a = a.wrapping_add(y);
    b = b.wrapping_add(a.rotate_right(44));
    (a.wrapping_add(z), b.wrapping_add(c))
}

// go-dep: github.com/dgryski/go-farm@v0.0.0-20240924180020-3414d57e47da.weakHashLen32WithSeeds L1
#[inline]
fn weak_hash_len32_with_seeds(s: &[u8], a: u64, b: u64) -> (u64, u64) {
    weak_hash_len32_with_seeds_words(
        read_u64_le(s, 0),
        read_u64_le(s, 8),
        read_u64_le(s, 16),
        read_u64_le(s, 24),
        a,
        b,
    )
}

// go-dep: github.com/dgryski/go-farm@v0.0.0-20240924180020-3414d57e47da.hashLen33to64 L1 [M-INT]
fn hash_len33to64(s: &[u8]) -> u64 {
    let slen = s.len();
    let mul = K2.wrapping_add((slen as u64).wrapping_mul(2));
    let a = read_u64_le(s, 0).wrapping_mul(K2);
    let b = read_u64_le(s, 8);
    let c = read_u64_le(s, slen - 8).wrapping_mul(mul);
    let d = read_u64_le(s, slen - 16).wrapping_mul(K2);
    let y = a
        .wrapping_add(b)
        .rotate_right(43)
        .wrapping_add(c.rotate_right(30))
        .wrapping_add(d);
    let z = hash_len16_mul(
        y,
        a.wrapping_add(b.wrapping_add(K2).rotate_right(18))
            .wrapping_add(c),
        mul,
    );
    let e = read_u64_le(s, 16).wrapping_mul(mul);
    let f = read_u64_le(s, 24);
    let g = y.wrapping_add(read_u64_le(s, slen - 32)).wrapping_mul(mul);
    let h = z.wrapping_add(read_u64_le(s, slen - 24)).wrapping_mul(mul);
    hash_len16_mul(
        e.wrapping_add(f)
            .rotate_right(43)
            .wrapping_add(g.rotate_right(30))
            .wrapping_add(h),
        e.wrapping_add(f.wrapping_add(a).rotate_right(18))
            .wrapping_add(g),
        mul,
    )
}

// go-dep: github.com/dgryski/go-farm@v0.0.0-20240924180020-3414d57e47da.naHash64 L1 [M-INT]
fn na_hash64(mut s: &[u8]) -> u64 {
    let slen = s.len();
    let seed: u64 = 81;
    if slen <= 32 {
        if slen <= 16 {
            return hash_len0to16(s);
        }
        return hash_len17to32(s);
    }
    if slen <= 64 {
        return hash_len33to64(s);
    }
    // For strings over 64 bytes we loop.
    // Internal state consists of 56 bytes: v, w, x, y, and z.
    let mut v = Uint128::default();
    let mut w = Uint128::default();
    let mut x = seed.wrapping_mul(K2).wrapping_add(read_u64_le(s, 0));
    let mut y = seed.wrapping_mul(K1).wrapping_add(113);
    let mut z = shift_mix(y.wrapping_mul(K2).wrapping_add(113)).wrapping_mul(K2);
    // Set end so that after the loop we have 1 to 64 bytes left to process.
    let end_idx = ((slen - 1) / 64) * 64;
    let last64_idx = end_idx + ((slen - 1) & 63) - 63;
    let last64 = &s[last64_idx..];
    while s.len() > 64 {
        x = x
            .wrapping_add(y)
            .wrapping_add(v.lo)
            .wrapping_add(read_u64_le(s, 8))
            .rotate_right(37)
            .wrapping_mul(K1);
        y = y
            .wrapping_add(v.hi)
            .wrapping_add(read_u64_le(s, 48))
            .rotate_right(42)
            .wrapping_mul(K1);
        x ^= w.hi;
        y = y.wrapping_add(v.lo.wrapping_add(read_u64_le(s, 40)));
        z = z.wrapping_add(w.lo).rotate_right(33).wrapping_mul(K1);
        let (vlo, vhi) = weak_hash_len32_with_seeds(s, v.hi.wrapping_mul(K1), x.wrapping_add(w.lo));
        v.lo = vlo;
        v.hi = vhi;
        let (wlo, whi) = weak_hash_len32_with_seeds(
            &s[32..],
            z.wrapping_add(w.hi),
            y.wrapping_add(read_u64_le(s, 16)),
        );
        w.lo = wlo;
        w.hi = whi;
        std::mem::swap(&mut x, &mut z);
        s = &s[64..];
    }
    let mul = K1.wrapping_add((z & 0xff) << 1);
    // Make s point to the last 64 bytes of input.
    s = last64;
    w.lo = w.lo.wrapping_add(((slen - 1) & 63) as u64);
    v.lo = v.lo.wrapping_add(w.lo);
    w.lo = w.lo.wrapping_add(v.lo);
    x = x
        .wrapping_add(y)
        .wrapping_add(v.lo)
        .wrapping_add(read_u64_le(s, 8))
        .rotate_right(37)
        .wrapping_mul(mul);
    y = y
        .wrapping_add(v.hi)
        .wrapping_add(read_u64_le(s, 48))
        .rotate_right(42)
        .wrapping_mul(mul);
    x ^= w.hi.wrapping_mul(9);
    y = y.wrapping_add(v.lo.wrapping_mul(9).wrapping_add(read_u64_le(s, 40)));
    z = z.wrapping_add(w.lo).rotate_right(33).wrapping_mul(mul);
    let (vlo, vhi) = weak_hash_len32_with_seeds(s, v.hi.wrapping_mul(mul), x.wrapping_add(w.lo));
    v.lo = vlo;
    v.hi = vhi;
    let (wlo, whi) = weak_hash_len32_with_seeds(
        &s[32..],
        z.wrapping_add(w.hi),
        y.wrapping_add(read_u64_le(s, 16)),
    );
    w.lo = wlo;
    w.hi = whi;
    std::mem::swap(&mut x, &mut z);
    hash_len16_mul(
        hash_len16_mul(v.lo, w.lo, mul)
            .wrapping_add(shift_mix(y).wrapping_mul(K0))
            .wrapping_add(z),
        hash_len16_mul(v.hi, w.hi, mul).wrapping_add(x),
        mul,
    )
}

// go-dep: github.com/dgryski/go-farm@v0.0.0-20240924180020-3414d57e47da.naHash64WithSeed L1
#[cfg(test)]
fn na_hash64_with_seed(s: &[u8], seed: u64) -> u64 {
    na_hash64_with_seeds(s, K2, seed)
}

// go-dep: github.com/dgryski/go-farm@v0.0.0-20240924180020-3414d57e47da.naHash64WithSeeds L1 [M-INT]
fn na_hash64_with_seeds(s: &[u8], seed0: u64, seed1: u64) -> u64 {
    hash_len16(na_hash64(s).wrapping_sub(seed0), seed1)
}

// go-dep: github.com/dgryski/go-farm@v0.0.0-20240924180020-3414d57e47da.uoH L1 [M-INT]
#[inline]
fn uo_h(x: u64, y: u64, mul: u64, r: u32) -> u64 {
    let mut a = (x ^ y).wrapping_mul(mul);
    a ^= a >> 47;
    let b = (y ^ a).wrapping_mul(mul);
    b.rotate_right(r).wrapping_mul(mul)
}

/// Hash64WithSeeds hashes a byte slice and two u64 seeds and returns a u64 hash value.
// go-dep: github.com/dgryski/go-farm@v0.0.0-20240924180020-3414d57e47da.Hash64WithSeeds L1 [M-INT]
pub fn hash64_with_seeds(mut s: &[u8], seed0: u64, seed1: u64) -> u64 {
    let slen = s.len();
    if slen <= 64 {
        return na_hash64_with_seeds(s, seed0, seed1);
    }

    // For strings over 64 bytes we loop.
    // Internal state consists of 64 bytes: u, v, w, x, y, and z.
    let mut x = seed0;
    let mut y = seed1.wrapping_mul(K2).wrapping_add(113);
    let mut z = shift_mix(y.wrapping_mul(K2)).wrapping_mul(K2);
    let mut v = Uint128 {
        lo: seed0,
        hi: seed1,
    };
    let mut w = Uint128::default();
    let mut u = x.wrapping_sub(z);
    x = x.wrapping_mul(K2);
    let mul = K2.wrapping_add(u & 0x82);

    // Set end so that after the loop we have 1 to 64 bytes left to process.
    let end_idx = ((slen - 1) / 64) * 64;
    let last64_idx = end_idx + ((slen - 1) & 63) - 63;
    let last64 = &s[last64_idx..];

    while s.len() > 64 {
        let a0 = read_u64_le(s, 0);
        let a1 = read_u64_le(s, 8);
        let a2 = read_u64_le(s, 16);
        let a3 = read_u64_le(s, 24);
        let a4 = read_u64_le(s, 32);
        let a5 = read_u64_le(s, 40);
        let a6 = read_u64_le(s, 48);
        let a7 = read_u64_le(s, 56);
        x = x.wrapping_add(a0).wrapping_add(a1);
        y = y.wrapping_add(a2);
        z = z.wrapping_add(a3);
        v.lo = v.lo.wrapping_add(a4);
        v.hi = v.hi.wrapping_add(a5).wrapping_add(a1);
        w.lo = w.lo.wrapping_add(a6);
        w.hi = w.hi.wrapping_add(a7);

        x = x.rotate_right(26);
        x = x.wrapping_mul(9);
        y = y.rotate_right(29);
        z = z.wrapping_mul(mul);
        v.lo = v.lo.rotate_right(33);
        v.hi = v.hi.rotate_right(30);
        w.lo ^= x;
        w.lo = w.lo.wrapping_mul(9);
        z = z.rotate_right(32);
        z = z.wrapping_add(w.hi);
        w.hi = w.hi.wrapping_add(z);
        z = z.wrapping_mul(9);
        std::mem::swap(&mut u, &mut y);

        z = z.wrapping_add(a0).wrapping_add(a6);
        v.lo = v.lo.wrapping_add(a2);
        v.hi = v.hi.wrapping_add(a3);
        w.lo = w.lo.wrapping_add(a4);
        w.hi = w.hi.wrapping_add(a5).wrapping_add(a6);
        x = x.wrapping_add(a1);
        y = y.wrapping_add(a7);

        y = y.wrapping_add(v.lo);
        v.lo = v.lo.wrapping_add(x.wrapping_sub(y));
        v.hi = v.hi.wrapping_add(w.lo);
        w.lo = w.lo.wrapping_add(v.hi);
        w.hi = w.hi.wrapping_add(x.wrapping_sub(y));
        x = x.wrapping_add(w.hi);
        w.hi = w.hi.rotate_right(34);
        std::mem::swap(&mut u, &mut z);
        s = &s[64..];
    }
    // Make s point to the last 64 bytes of input.
    s = last64;
    u = u.wrapping_mul(9);
    v.hi = v.hi.rotate_right(28);
    v.lo = v.lo.rotate_right(20);
    w.lo = w.lo.wrapping_add(((slen - 1) & 63) as u64);
    u = u.wrapping_add(y);
    y = y.wrapping_add(u);
    x = y
        .wrapping_sub(x)
        .wrapping_add(v.lo)
        .wrapping_add(read_u64_le(s, 8))
        .rotate_right(37)
        .wrapping_mul(mul);
    y = (y ^ v.hi ^ read_u64_le(s, 48))
        .rotate_right(42)
        .wrapping_mul(mul);
    x ^= w.hi.wrapping_mul(9);
    y = y.wrapping_add(v.lo).wrapping_add(read_u64_le(s, 40));
    z = z.wrapping_add(w.lo).rotate_right(33).wrapping_mul(mul);
    let (vlo, vhi) = weak_hash_len32_with_seeds(s, v.hi.wrapping_mul(mul), x.wrapping_add(w.lo));
    v.lo = vlo;
    v.hi = vhi;
    let (wlo, whi) = weak_hash_len32_with_seeds(
        &s[32..],
        z.wrapping_add(w.hi),
        y.wrapping_add(read_u64_le(s, 16)),
    );
    w.lo = wlo;
    w.hi = whi;
    uo_h(
        hash_len16_mul(v.lo.wrapping_add(x), w.lo ^ y, mul)
            .wrapping_add(z)
            .wrapping_sub(u),
        uo_h(v.hi.wrapping_add(y), w.hi.wrapping_add(z), K2, 30) ^ x,
        K2,
        31,
    )
}

/// Hash64WithSeed hashes a byte slice and a u64 seed and returns a u64 hash value.
// go-dep: github.com/dgryski/go-farm@v0.0.0-20240924180020-3414d57e47da.Hash64WithSeed L1
#[cfg(test)]
pub fn hash64_with_seed(s: &[u8], seed: u64) -> u64 {
    if s.len() <= 64 {
        return na_hash64_with_seed(s, seed);
    }
    hash64_with_seeds(s, 0, seed)
}

// go-dep: github.com/dgryski/go-farm@v0.0.0-20240924180020-3414d57e47da.uoHash64 L1
fn uo_hash64(s: &[u8]) -> u64 {
    if s.len() <= 64 {
        return na_hash64(s);
    }
    hash64_with_seeds(s, 81, 0)
}

// go-dep: github.com/dgryski/go-farm@v0.0.0-20240924180020-3414d57e47da.h32 L1 [M-INT]
#[inline]
fn h32(s: &[u8], mul: u64) -> u64 {
    let slen = s.len();
    let mut a = read_u64_le(s, 0).wrapping_mul(K1);
    let mut b = read_u64_le(s, 8);
    let c = read_u64_le(s, slen - 8).wrapping_mul(mul);
    let d = read_u64_le(s, slen - 16).wrapping_mul(K2);
    let u = a
        .wrapping_add(b)
        .rotate_right(43)
        .wrapping_add(c.rotate_right(30))
        .wrapping_add(d);
    let v = a
        .wrapping_add(b.wrapping_add(K2).rotate_right(18))
        .wrapping_add(c);
    a = shift_mix((u ^ v).wrapping_mul(mul));
    b = shift_mix((v ^ a).wrapping_mul(mul));
    b
}

// go-dep: github.com/dgryski/go-farm@v0.0.0-20240924180020-3414d57e47da.h32Seeds L1 [M-INT]
#[inline]
fn h32_seeds(s: &[u8], mul: u64, seed0: u64, seed1: u64) -> u64 {
    let slen = s.len();
    let mut a = read_u64_le(s, 0).wrapping_mul(K1);
    let mut b = read_u64_le(s, 8);
    let c = read_u64_le(s, slen - 8).wrapping_mul(mul);
    let d = read_u64_le(s, slen - 16).wrapping_mul(K2);
    let u = a
        .wrapping_add(b)
        .rotate_right(43)
        .wrapping_add(c.rotate_right(30))
        .wrapping_add(d)
        .wrapping_add(seed0);
    let v = a
        .wrapping_add(b.wrapping_add(K2).rotate_right(18))
        .wrapping_add(c)
        .wrapping_add(seed1);
    a = shift_mix((u ^ v).wrapping_mul(mul));
    b = shift_mix((v ^ a).wrapping_mul(mul));
    b
}

// go-dep: github.com/dgryski/go-farm@v0.0.0-20240924180020-3414d57e47da.xohashLen33to64 L1 [M-INT]
fn xohash_len33to64(s: &[u8]) -> u64 {
    let slen = s.len();
    let mul0 = K2.wrapping_sub(30);
    let mul1 = K2
        .wrapping_sub(30)
        .wrapping_add(2u64.wrapping_mul(slen as u64));
    let h0 = h32(&s[0..32], mul0);
    let h1 = h32(&s[slen - 32..], mul1);
    h1.wrapping_mul(mul1).wrapping_add(h0).wrapping_mul(mul1)
}

// go-dep: github.com/dgryski/go-farm@v0.0.0-20240924180020-3414d57e47da.xohashLen65to96 L1 [M-INT]
fn xohash_len65to96(s: &[u8]) -> u64 {
    let slen = s.len();
    let mul0 = K2.wrapping_sub(114);
    let mul1 = K2
        .wrapping_sub(114)
        .wrapping_add(2u64.wrapping_mul(slen as u64));
    let h0 = h32(&s[..32], mul0);
    let h1 = h32(&s[32..64], mul1);
    let h2 = h32_seeds(&s[slen - 32..], mul1, h0, h1);
    h2.wrapping_mul(9)
        .wrapping_add(h0 >> 17)
        .wrapping_add(h1 >> 21)
        .wrapping_mul(mul1)
}

/// Hash64 hashes a byte slice and returns a 64-bit FarmHash value identical to
/// `github.com/dgryski/go-farm`'s `Hash64`.
// go-dep: github.com/dgryski/go-farm@v0.0.0-20240924180020-3414d57e47da.Hash64 L1
pub fn hash64(s: &[u8]) -> u64 {
    let slen = s.len();
    if slen <= 32 {
        if slen <= 16 {
            hash_len0to16(s)
        } else {
            hash_len17to32(s)
        }
    } else if slen <= 64 {
        xohash_len33to64(s)
    } else if slen <= 96 {
        xohash_len65to96(s)
    } else if slen <= 256 {
        na_hash64(s)
    } else {
        uo_hash64(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vectors copied verbatim from `dgryski/go-farm/farmhash_test.go` (`testData`).
    /// Columns: `(oh64na, oh64, oh64s, oh64ss, input)` where `seed = 32` for `oh64s` and
    /// `seed0 = 32, seed1 = 64` for `oh64ss`.
    const TEST_DATA: &[(u64, u64, u64, u64, &str)] = &[
        (
            0x9ae16a3b2f90404f,
            0x9ae16a3b2f90404f,
            0xb0403333574d37e4,
            0x1ad9361c3f563461,
            "",
        ),
        (
            0xb3454265b6df75e3,
            0xb3454265b6df75e3,
            0x779ef0ca4870bcc2,
            0xd95bffbe1f68b36d,
            "a",
        ),
        (
            0xaa8d6e5242ada51e,
            0xaa8d6e5242ada51e,
            0xbfb091da4ffef5d8,
            0xa05bfc8f958ce312,
            "ab",
        ),
        (
            0x24a5b3a074e7f369,
            0x24a5b3a074e7f369,
            0x7c6dc4691a7576b6,
            0x008b6aa9b6344c8d,
            "abc",
        ),
        (
            0x1a5502de4a1f8101,
            0x1a5502de4a1f8101,
            0xfdc6b0ac6369ca3a,
            0xf0d7f476a4cc1a4a,
            "abcd",
        ),
        (
            0xc22f4663e54e04d4,
            0xc22f4663e54e04d4,
            0x64b5b351c1cee4a2,
            0x6ecf50380f033790,
            "abcde",
        ),
        (
            0xc329379e6a03c2cd,
            0xc329379e6a03c2cd,
            0xb8e8ed73451fc99f,
            0x03850d708bfcb24b,
            "abcdef",
        ),
        (
            0x3c40c92b1ccb7355,
            0x3c40c92b1ccb7355,
            0x66301d1183086f9c,
            0x25fcd92789893a23,
            "abcdefg",
        ),
        (
            0xfee9d22990c82909,
            0xfee9d22990c82909,
            0xef6b24f6a776fd30,
            0xee098adfe88e7340,
            "abcdefgh",
        ),
        (
            0x332c8ed4dae5ba42,
            0x332c8ed4dae5ba42,
            0xb54b4c8bd2372ffc,
            0x4c405cec37506bfd,
            "abcdefghi",
        ),
        (
            0xad052244b781c4eb,
            0xad052244b781c4eb,
            0xd56f3e3e18d9cacc,
            0xe148f198dad5079d,
            "0123456789",
        ),
        (
            0x3ef4c03514208c77,
            0x3ef4c03514208c77,
            0xda0851051d8a9854,
            0xdd0bdad27bda1972,
            "0123456789 ",
        ),
        (
            0x496841e83a33cc91,
            0x496841e83a33cc91,
            0x27f085304094b99f,
            0x0e85e1954a10b28e,
            "0123456789-0",
        ),
        (
            0xd81bcb9f3679ac0c,
            0xd81bcb9f3679ac0c,
            0x938a277ef2446d80,
            0x378ce8623423a232,
            "0123456789~01",
        ),
        (
            0x5da5a6a117c606f6,
            0x5da5a6a117c606f6,
            0x2439b47b63cfb868,
            0x9b0a3f5504372975,
            "0123456789#012",
        ),
        (
            0x5361eae17c1ff6bc,
            0x5361eae17c1ff6bc,
            0xf629b20de846fc23,
            0x3ca6d68cb0129d5f,
            "0123456789@0123",
        ),
        (
            0x4283d4ef43627f64,
            0x4283d4ef43627f64,
            0x33abb5361a4815c1,
            0x5bec11b7f0eb5b0e,
            "0123456789'01234",
        ),
        (
            0x46a7416ed4861e3b,
            0x46a7416ed4861e3b,
            0xf4547ea9e910bc0e,
            0x5cfaf00ae1d9b34c,
            "0123456789=012345",
        ),
        (
            0xa4abb4e0da2c594c,
            0xa4abb4e0da2c594c,
            0x05d865e3844b83fe,
            0x8a6aa12f6f761cf2,
            "0123456789+0123456",
        ),
        (
            0xcf1c7d3ad54f9215,
            0xcf1c7d3ad54f9215,
            0x79be62145d9d67cf,
            0x6541cb3eebafd693,
            "0123456789*01234567",
        ),
        (
            0x07adf50b2ac764fc,
            0x07adf50b2ac764fc,
            0x2474703a1b29f6a9,
            0xb01e016d86ede665,
            "0123456789&012345678",
        ),
        (
            0xdebcba8e6f3eabd1,
            0xdebcba8e6f3eabd1,
            0x9fb28e096e6968db,
            0x91b2d71a5147967b,
            "0123456789^0123456789",
        ),
        (
            0x4dbd128af51d77e8,
            0x4dbd128af51d77e8,
            0x1e9ce746b2ffef52,
            0xac60f1c73ac2c016,
            "0123456789%0123456789£",
        ),
        (
            0xd78d5f852d522e6a,
            0xd78d5f852d522e6a,
            0x072fd1a4b21ed852,
            0x7a7ddf0143013fc2,
            "0123456789$0123456789!0",
        ),
        (
            0x80d73b843ba57db8,
            0x80d73b843ba57db8,
            0x8d8d5757a7bda0ba,
            0xb9857c5b86ba9e86,
            "size:  a.out:  bad magic",
        ),
        (
            0x8eb3808d1ccfc779,
            0x8eb3808d1ccfc779,
            0x5ba1410bdca911a7,
            0x78fc99025fd288e6,
            "Nepal premier won't resign.",
        ),
        (
            0xb944f8a16261e414,
            0xb944f8a16261e414,
            0x4e4d65c93956a760,
            0x5edcdee0fa2d5936,
            "C is as portable as Stonehedge!!",
        ),
        (
            0xe8f89ab6df9bdd25,
            0x2d072041b535155d,
            0xa3a2a2a6c80ebbd2,
            0xf88dbcbab3c05f4e,
            "Discard medicine more than two years old.",
        ),
        (
            0xa9961670ce2a46d9,
            0x361b79df08615cd6,
            0x9d0526b9480738b8,
            0xf31be505a4ee2623,
            "I wouldn't marry him with a ten foot pole.",
        ),
        (
            0xbdd69b798d6ba37a,
            0x1f232f3375914f0a,
            0x1110fe437d57c4d3,
            0x74a9361763b11819,
            "If the enemy is within range, then so are you.",
        ),
        (
            0xc2f8db8624fefc0e,
            0x1da6c1dfec23a597,
            0x755a46f13a764857,
            0xe7f5413a7f308fd6,
            "The major problem is with sendmail.  -Mark Horton",
        ),
        (
            0x5a0a6efd52e84e2a,
            0xa29944470950e8e4,
            0x3eb089865ab8b75e,
            0x9fb5f8a3ed197d12,
            "How can you write a big system without C++?  -Paul Glick",
        ),
        (
            0x786d7e1987023ca9,
            0x9f9e3cdeb570f926,
            0x54bf932850a761e1,
            0x636c00055b1d3e3f,
            "He who has a shady past knows that nice guys finish last.",
        ),
        (
            0x5d14f96c18fe3d5e,
            0xdcfb73d4de1111c6,
            0x7d87801f634222c4,
            0xfa0e0a8b2a984142,
            "Free! Free!/A trip/to Mars/for 900/empty jars/Burma Shave",
        ),
        (
            0xec8848fd3b266c10,
            0x3df4b8e109629602,
            0xddce3cea9126d550,
            0x8c085581e9f35148,
            "His money is twice tainted: 'taint yours and 'taint mine.",
        ),
        (
            0x2a578b80bb82147c,
            0xd71bdfedb6182a5d,
            0xf425894aebcce21a,
            0x7a02d6d816a67950,
            "The days of the digital watch are numbered.  -Tom Stoppard",
        ),
        (
            0x55182f8859eca4ce,
            0x8452fbb0c8f98c4f,
            0xf0b5522f8062b30f,
            0xb76f3e8c68eea2bc,
            "For every action there is an equal and opposite government program.",
        ),
        (
            0xabcdb319fcf2826c,
            0x98d2fbd5131a5860,
            0x7b609bd1b51446df,
            0xf45b1631b573f390,
            "You remind me of a TV show, but that's all right: I watch it anyway.",
        ),
        (
            0x1d85702503ac7eb4,
            0x796229f1faacec7e,
            0x4e58adfd902e4097,
            0x141a2e0ff5e0e729,
            "It's well we cannot hear the screams/That we create in others' dreams.",
        ),
        (
            0xa2b8bf3032021993,
            0xb8e2918a4398348d,
            0x50f9160881ddf6ff,
            0xbbdfff333383198f,
            "Give me a rock, paper and scissors and I will move the world.  CCFestoon",
        ),
        (
            0x38aa3175b37f305c,
            0x889b024bab17bf54,
            0x88387579f38d77c9,
            0xbfe4b3fa04bb6506,
            "It's a tiny change to the code and not completely disgusting. - Bob Manchek",
        ),
        (
            0x7e85d7b050ed2967,
            0x7fee06e367562d44,
            0x249fa05ebf23cc74,
            0x113915d477dc320b,
            "There is no reason for any individual to have a computer in their home. -Ken Olsen, 1977",
        ),
        (
            0x5a05644eb66e435e,
            0x4c349a4ff7ac0c89,
            0x7b88e6bee477c3e0,
            0x07d20be82d1835c9,
            "Even if I could be Shakespeare, I think I should still choose to be Faraday. - A. Huxley",
        ),
        (
            0x098eff6958c5e91a,
            0x098eff6958c5e91a,
            0x37d8c1e3d3f79073,
            0xdf1461ed0c9c3fcb,
            "The fugacity of a constituent in a mixture of gases at a given temperature is proportional to its mole fraction.  Lewis-Randall Rule",
        ),
        (
            0xc3f02c4ffd5d71e6,
            0xed25cfc61b15bddd,
            0x42b50de39d3f2225,
            0x2e29484ace9fbcf0,
            "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat. Duis aute irure dolor in reprehenderit in voluptate velit esse cillum dolore eu fugiat nulla pariatur. Excepteur sint occaecat cupidatat non proident, sunt in culpa qui officia deserunt mollit anim id est laborum.",
        ),
    ];

    #[test]
    fn test_farm_hash64_vectors() {
        for &(want_na, want_xo, want_s, want_ss, input) in TEST_DATA {
            let b = input.as_bytes();
            assert_eq!(na_hash64(b), want_na, "na_hash64({input:?})");
            assert_eq!(hash64(b), want_xo, "hash64({input:?})");
            assert_eq!(
                hash64_with_seed(b, 32),
                want_s,
                "hash64_with_seed({input:?})"
            );
            assert_eq!(
                hash64_with_seeds(b, 32, 64),
                want_ss,
                "hash64_with_seeds({input:?})"
            );
        }
    }
}
