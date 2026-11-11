//! The transaction memory buffer a system-table write fills, over a snapshot.
//!
//! Go writes through `kv.Transaction` (client-go's MemDB over a TiKV snapshot). The writer of
//! this crate is pure: it reads a [`Snapshot`] the caller provides and collects its writes in
//! a [`MemBuffer`], which the caller turns into a TiKV transaction (spec §6.2 steps 1–6).
//! Only the parts of client-go's MemDB the REPLACE path uses are here: values (empty = delete),
//! the key flags it sets, and the assertion rule "only the first assertion takes effect".

use std::collections::BTreeMap;

use tidb_base::Error;

/// A read view of the committed data (the transaction's snapshot at its start ts).
pub trait Snapshot {
    /// The committed value of key, `None` if it does not exist.
    // glue: snapshot read of the caller's transaction (kv.Snapshot.Get)
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error>;
}

impl Snapshot for BTreeMap<Vec<u8>, Vec<u8>> {
    // glue: an in-memory snapshot (tests)
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        Ok(BTreeMap::get(self, key).cloned())
    }
}

/// The assertion on a key's existence that the prewrite carries (`kv.SetAssert*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Assertion {
    /// `kv.SetAssertExist`: the key must exist.
    Exist,
    /// `kv.SetAssertNotExist`: the key must not exist.
    NotExist,
    /// `kv.SetAssertUnknown`: no assertion (an explicit one, so later ones do not apply).
    Unknown,
}

/// The key flags of a buffered key that the REPLACE path sets (a subset of client-go's
/// `KeyFlags`; the D07.0 keyflags are X).
// glue: subset of kv.KeyFlags / client-go unionstore flags (presume-not-exists, need-locked, assertion)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyFlags {
    /// `kv.SetPresumeKeyNotExists`: the existence is checked lazily, by the prewrite
    /// (client-go sends the mutation as `Op_Insert`).
    pub presume_not_exists: bool,
    /// `kv.SetNeedLocked`: lock the key in a pessimistic transaction (no effect in an
    /// optimistic one).
    pub need_locked: bool,
    /// The first assertion set on the key.
    pub assertion: Option<Assertion>,
}

impl KeyFlags {
    /// The flags as the govec records list them (`flagsStr` of the systable domain).
    // glue: govec rendering of the flags
    pub fn render(&self) -> String {
        let mut s: Vec<&str> = Vec::new();
        if self.presume_not_exists {
            s.push("presume_not_exists");
        }
        match self.assertion {
            Some(Assertion::Exist) => s.push("assert_exist"),
            Some(Assertion::NotExist) => s.push("assert_not_exist"),
            Some(Assertion::Unknown) => s.push("assert_unknown"),
            None => {}
        }
        if self.need_locked {
            s.push("need_locked");
        }
        s.join(",")
    }
}

/// One buffered key: its value (empty = delete) and flags.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The value; empty is a delete.
    pub value: Vec<u8>,
    /// The key flags.
    pub flags: KeyFlags,
}

/// The writes of one transaction, in key order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemBuffer {
    entries: BTreeMap<Vec<u8>, Entry>,
}

impl MemBuffer {
    /// An empty buffer.
    // glue: empty transaction buffer
    pub fn new() -> MemBuffer {
        MemBuffer::default()
    }

    /// The buffered keys in key order.
    // glue: iteration of the buffer (MemBuffer.Iter)
    pub fn entries(&self) -> &BTreeMap<Vec<u8>, Entry> {
        &self.entries
    }

    /// Whether nothing was written (the statement left the table unchanged).
    // glue: MemBuffer.Len() == 0
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `MemBuffer.GetLocal`: the buffered value of key (`Some(empty)` for a buffered
    /// delete), `None` if the key is not buffered.
    // glue: kv.MemBuffer.GetLocal
    pub fn get_local(&self, key: &[u8]) -> Option<&[u8]> {
        self.entries.get(key).map(|e| e.value.as_slice())
    }

    /// `kv.Transaction.Get`: the buffered value, else the snapshot's. A buffered delete is
    /// "not found".
    // glue: kv.Transaction.Get (union of the memory buffer and the snapshot)
    pub fn get(&self, snap: &dyn Snapshot, key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        match self.entries.get(key) {
            Some(e) if e.value.is_empty() => Ok(None),
            Some(e) => Ok(Some(e.value.clone())),
            None => snap.get(key),
        }
    }

    // glue: buffer slot of a key
    fn entry(&mut self, key: &[u8]) -> &mut Entry {
        self.entries.entry(key.to_vec()).or_insert_with(|| Entry {
            value: Vec::new(),
            flags: KeyFlags::default(),
        })
    }

    /// `MemBuffer.Set` / `SetWithFlags`: flags add to the key's flags.
    // glue: kv.MemBuffer.SetWithFlags
    pub fn set_with_flags(&mut self, key: &[u8], value: Vec<u8>, presume_not_exists: bool) {
        let e = self.entry(key);
        e.value = value;
        e.flags.presume_not_exists |= presume_not_exists;
    }

    /// `MemBuffer.Delete` / `DeleteWithFlags(key, kv.SetNeedLocked)`.
    // glue: kv.MemBuffer.DeleteWithFlags
    pub fn delete_with_flags(&mut self, key: &[u8], need_locked: bool) {
        let e = self.entry(key);
        e.value = Vec::new();
        e.flags.need_locked |= need_locked;
    }

    /// `Transaction.SetAssertion`: only the first assertion of a key takes effect.
    // glue: kv.Transaction.SetAssertion (client-go: the first assertion wins)
    pub fn set_assertion(&mut self, key: &[u8], a: Assertion) {
        let e = self.entry(key);
        if e.flags.assertion.is_none() {
            e.flags.assertion = Some(a);
        }
    }
}
