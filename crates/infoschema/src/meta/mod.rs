//! go-file: pkg/meta/meta.go
//!
//! `pkg/meta`: the meta data stored in TiKV under the `m` prefix, in the `structure`
//! layout. tidb-rs is read-only (D06.2 scoping): only the readers of `meta.Mutator` (the
//! `meta.Reader` interface and the helpers it uses) are ported; the writers run in Go TiDB
//! (X, tracker rules). `NewMutator` (a `kv.Transaction` carrier) is not ported: the readers
//! are reached through [`new_reader`].
//!
//! Part 2 (specs/D06.2-meta/plan.md): `FastUnmarshalTableNameInfo` / `ListSimpleTables`
//! and the job name filter (`pkg/util/partialjson`), `IterAllTables`, `GetRUStats`,
//! `GetOldestSchemaVersion`, `pkg/meta/autoid`, `pkg/meta/metabuild`.
//!
//! Go-shaped results (M-ERRVAL): where Go returns a value together with an error that its
//! callers read (`GetInt64`'s clamped value, a partially decoded `DBInfo`, ...), the Rust
//! function returns `(value, Result<(), Error>)`.

#![allow(non_upper_case_globals)]

mod autoid;
mod reader;

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::bytes::Regex;
use tidb_base::Error;
use tidb_base::dbterror::ClassMeta;
use tidb_base::errno;
use tidb_base::gofmt::Arg;
use tidb_base::gostd::strconv;
use tidb_base::mysql;
use tidb_model::DBInfo;
use tidb_model::Job;
use tidb_model::PolicyInfo;
use tidb_model::ResourceGroupInfo;
use tidb_model::ResourceGroupSettings;
use tidb_model::SchemaDiff;
use tidb_model::StatePublic;
use tidb_model::TableInfo;
use tidb_parser::model::CIStr;
use tidb_parser::model::MediumPriorityValue;

use crate::structure::HashPair;
use crate::structure::ReverseHashIterator;
use crate::structure::TxStructure;
use crate::structure::new_hash_reverse_iter;
use crate::structure::new_hash_reverse_iter_begin_with_field;

pub use autoid::AutoIDAccessor;
pub use autoid::AutoIDAccessors;
pub use autoid::AutoIDGroup;
pub use autoid::new_auto_id_accessors;
pub use reader::new_reader;

pub(crate) const mMetaPrefix: &[u8] = b"m";
const mNextGlobalIDKey: &[u8] = b"NextGlobalID";
const mSchemaVersionKey: &[u8] = b"SchemaVersionKey";
const mDBs: &[u8] = b"DBs";
const mDBPrefix: &str = "DB";
const mTablePrefix: &str = "Table";
const mSequencePrefix: &str = "SID";
const mSeqCyclePrefix: &str = "SequenceCycle";
const mTableIDPrefix: &str = "TID";
const mIncIDPrefix: &str = "IID";
const mRandomIDPrefix: &str = "TARID";
const mBootstrapKey: &[u8] = b"BootstrapKey";
const mSchemaDiffPrefix: &str = "Diff";
const mPolicies: &[u8] = b"Policies";
const mPolicyPrefix: &str = "Policy";
const mResourceGroups: &[u8] = b"ResourceGroups";
const mResourceGroupPrefix: &str = "RG";
const mPolicyGlobalID: &[u8] = b"PolicyGlobalID";
const mDDLTableVersion: &[u8] = b"DDLTableVersion";
const mBDRRole: &[u8] = b"BDRRole";
const mMetaDataLock: &[u8] = b"metadataLock";
const mSchemaCacheSize: &[u8] = b"SchemaCacheSize";

const mIngestMaxBatchSplitRangesKey: &[u8] = b"IngestMaxBatchSplitRanges";
const mIngestMaxSplitRangesPerSecKey: &[u8] = b"IngestMaxSplitRangesPerSec";
const mIngestMaxInflightKey: &[u8] = b"IngestMaxInflight";
const mIngestMaxPerSecKey: &[u8] = b"IngestMaxReqPerSec";

const mDDLJobListKey: &[u8] = b"DDLJobList";
const mDDLJobAddIdxList: &[u8] = b"DDLJobAddIdxList";
const mDDLJobHistoryKey: &[u8] = b"DDLJobHistory";

/// The id of the default resource group.
const defaultGroupID: i64 = 1;

/// The `default` resource group as Go returns it when none is stored
/// (`resourcegroup.DefaultResourceGroupName`).
// glue: Go's package variable defaultRGroupMeta (built per call: ResourceGroupInfo is not Clone)
fn default_r_group_meta() -> ResourceGroupInfo {
    ResourceGroupInfo {
        resource_group_settings: Some(Box::new(ResourceGroupSettings {
            ru_rate: i32::MAX as u64,
            burst_limit: -1,
            priority: MediumPriorityValue,
            ..Default::default()
        })),
        id: defaultGroupID,
        name: CIStr::new("default"),
        state: StatePublic,
    }
}

/// CurrentMagicByteVer is the current magic byte version, used for future meta compatibility.
pub const CurrentMagicByteVer: u8 = 0x00;
/// MaxInt48 is the max value of int48.
pub const MaxInt48: i64 = 0x0000_FFFF_FFFF_FFFF;
/// MaxGlobalID reserves 1000 IDs. Use MaxInt48 to reserves the high 2 bytes to compatible
/// with Multi-tenancy.
pub const MaxGlobalID: i64 = MaxInt48 - 1000;

const typeUnknown: i32 = 0;
const typeJSON: i32 = 1;

/// ErrDBExists is the error for db exists.
pub static ErrDBExists: LazyLock<Error> =
    LazyLock::new(|| ClassMeta.new_std(errno::ErrDBCreateExists));
/// ErrDBNotExists is the error for db not exists.
pub static ErrDBNotExists: LazyLock<Error> = LazyLock::new(|| ClassMeta.new_std(errno::ErrBadDB));
/// ErrPolicyExists is the error for policy exists.
pub static ErrPolicyExists: LazyLock<Error> =
    LazyLock::new(|| ClassMeta.new_std(errno::ErrPlacementPolicyExists));
/// ErrPolicyNotExists is the error for policy not exists.
pub static ErrPolicyNotExists: LazyLock<Error> =
    LazyLock::new(|| ClassMeta.new_std(errno::ErrPlacementPolicyNotExists));
/// ErrResourceGroupExists is the error for resource group exists.
pub static ErrResourceGroupExists: LazyLock<Error> =
    LazyLock::new(|| ClassMeta.new_std(errno::ErrResourceGroupExists));
/// ErrResourceGroupNotExists is the error for resource group not exists.
pub static ErrResourceGroupNotExists: LazyLock<Error> =
    LazyLock::new(|| ClassMeta.new_std(errno::ErrResourceGroupNotExists));
/// ErrTableExists is the error for table exists.
pub static ErrTableExists: LazyLock<Error> =
    LazyLock::new(|| ClassMeta.new_std(errno::ErrTableExists));
/// ErrTableNotExists is the error for table not exists.
pub static ErrTableNotExists: LazyLock<Error> =
    LazyLock::new(|| ClassMeta.new_std(errno::ErrNoSuchTable));
/// ErrDDLReorgElementNotExist is the error for reorg element not exists.
pub static ErrDDLReorgElementNotExist: LazyLock<Error> =
    LazyLock::new(|| ClassMeta.new_std(errno::ErrDDLReorgElementNotExist));
/// ErrInvalidString is the error for invalid string to parse
pub static ErrInvalidString: LazyLock<Error> =
    LazyLock::new(|| ClassMeta.new_std(errno::ErrInvalidCharacterString));

/// DDLTableVersion is to display ddl related table versions
pub type DDLTableVersion = i64;
/// InitDDLTableVersion is the original version.
pub const InitDDLTableVersion: DDLTableVersion = 0;
/// BaseDDLTableVersion is for support concurrent DDL, it added tidb_ddl_job, tidb_ddl_reorg
/// and tidb_ddl_history.
pub const BaseDDLTableVersion: DDLTableVersion = 1;
/// MDLTableVersion is for support MDL tables.
pub const MDLTableVersion: DDLTableVersion = 2;
/// BackfillTableVersion is for support distributed reorg stage, it added
/// tidb_background_subtask, tidb_background_subtask_history.
pub const BackfillTableVersion: DDLTableVersion = 3;
/// DDLNotifierTableVersion is for support ddl notifier, it added tidb_ddl_notifier.
pub const DDLNotifierTableVersion: DDLTableVersion = 4;

/// JobListKeyType is a key type of the DDL job queue.
pub type JobListKeyType = Vec<u8>;
/// DefaultJobListKey keeps all actions of DDL jobs except "add index".
/// TODO: deprecate it.
pub const DefaultJobListKey: &[u8] = mDDLJobListKey;
/// AddIndexJobListKey only keeps the action of adding index.
pub const AddIndexJobListKey: &[u8] = mDDLJobAddIdxList;

/// `fmt.Sprintf("%s:%d", prefix, id)`.
// glue: the "<prefix>:<id>" keys of meta.go
fn prefixed_id(prefix: &str, id: i64) -> Vec<u8> {
    format!("{prefix}:{id}").into_bytes()
}

/// `strconv.Atoi(strings.TrimPrefix(string(k), prefix+":"))`, Go-shaped.
// glue: the ID parse of the Parse*Key functions
fn parse_id(k: &[u8], prefix: &str) -> (i64, Result<(), Error>) {
    let rest = if has_colon_prefix_or_exact(k, prefix) {
        &k[prefix.len() + 1..]
    } else {
        k
    };
    let (n, err) = strconv::atoi(rest);
    (n, err.map_err(Error::plain))
}

/// Mutator is for handling meta information in a transaction. In tidb-rs only its read
/// methods are ported (D06.2 scoping), and it is built by [`new_reader`].
#[derive(Clone, Debug)]
pub struct Mutator {
    txn: TxStructure,
    /// StartTS is the txn's start TS.
    pub start_ts: u64,
    job_list_key: JobListKeyType,
}

/// DBkey encodes the dbID into dbKey.
// go: DBkey L1
pub fn db_key_of(db_id: i64) -> Vec<u8> {
    prefixed_id(mDBPrefix, db_id)
}

/// ParseDBKey decodes the dbkey to get dbID. Go-shaped (the ID is returned with Atoi's
/// error).
// go: ParseDBKey L1
pub fn parse_db_key(dbkey: &[u8]) -> (i64, Result<(), Error>) {
    if !is_db_key(dbkey) {
        return (
            0,
            Err(ErrInvalidString.gen_with_stack("fail to parse dbKey", vec![])),
        );
    }
    parse_id(dbkey, mDBPrefix)
}

/// IsDBkey checks whether the dbKey comes from DBKey().
// go: IsDBkey L1
pub fn is_db_key(db_key: &[u8]) -> bool {
    has_colon_prefix_or_exact(db_key, mDBPrefix)
}

/// `strings.HasPrefix(s, prefix+":")` (including `s == prefix+":"`).
// glue: strings.HasPrefix with the ":" separator
fn has_colon_prefix_or_exact(k: &[u8], prefix: &str) -> bool {
    k.len() > prefix.len() && k.starts_with(prefix.as_bytes()) && k[prefix.len()] == b':'
}

/// AutoTableIDKey decodes the auto tableID key.
// go: AutoTableIDKey L1
pub fn auto_table_id_key(table_id: i64) -> Vec<u8> {
    prefixed_id(mTableIDPrefix, table_id)
}

/// IsAutoTableIDKey checks whether the key is auto tableID key.
// go: IsAutoTableIDKey L1
pub fn is_auto_table_id_key(key: &[u8]) -> bool {
    has_colon_prefix_or_exact(key, mTableIDPrefix)
}

/// ParseAutoTableIDKey decodes the tableID from the auto tableID key. Go-shaped.
// go: ParseAutoTableIDKey L1
pub fn parse_auto_table_id_key(key: &[u8]) -> (i64, Result<(), Error>) {
    if !is_auto_table_id_key(key) {
        return (
            0,
            Err(ErrInvalidString.gen_with_stack("fail to parse autoTableKey", vec![])),
        );
    }
    parse_id(key, mTableIDPrefix)
}

/// AutoIncrementIDKey decodes the auto inc table key.
// go: AutoIncrementIDKey L1
pub fn auto_increment_id_key(table_id: i64) -> Vec<u8> {
    prefixed_id(mIncIDPrefix, table_id)
}

/// IsAutoIncrementIDKey checks whether the key is auto increment key.
// go: IsAutoIncrementIDKey L1
pub fn is_auto_increment_id_key(key: &[u8]) -> bool {
    has_colon_prefix_or_exact(key, mIncIDPrefix)
}

/// ParseAutoIncrementIDKey decodes the tableID from the auto increment key. Go-shaped.
// go: ParseAutoIncrementIDKey L1
pub fn parse_auto_increment_id_key(key: &[u8]) -> (i64, Result<(), Error>) {
    if !is_auto_increment_id_key(key) {
        return (
            0,
            Err(ErrInvalidString.gen_with_stack("fail to parse autoIncrementKey", vec![])),
        );
    }
    parse_id(key, mIncIDPrefix)
}

/// AutoRandomTableIDKey encodes the auto random tableID key.
// go: AutoRandomTableIDKey L1
pub fn auto_random_table_id_key(table_id: i64) -> Vec<u8> {
    prefixed_id(mRandomIDPrefix, table_id)
}

/// IsAutoRandomTableIDKey checks whether the key is auto random tableID key.
// go: IsAutoRandomTableIDKey L1
pub fn is_auto_random_table_id_key(key: &[u8]) -> bool {
    has_colon_prefix_or_exact(key, mRandomIDPrefix)
}

/// ParseAutoRandomTableIDKey decodes the tableID from the auto random tableID key.
/// Go-shaped.
// go: ParseAutoRandomTableIDKey L1
pub fn parse_auto_random_table_id_key(key: &[u8]) -> (i64, Result<(), Error>) {
    if !is_auto_random_table_id_key(key) {
        return (
            0,
            Err(ErrInvalidString.gen_with_stack("fail to parse AutoRandomTableIDKey", vec![])),
        );
    }
    parse_id(key, mRandomIDPrefix)
}

/// TableKey encodes the tableID into tableKey.
// go: TableKey L1
pub fn table_key_of(table_id: i64) -> Vec<u8> {
    prefixed_id(mTablePrefix, table_id)
}

/// IsTableKey checks whether the tableKey comes from TableKey().
// go: IsTableKey L1
pub fn is_table_key(table_key: &[u8]) -> bool {
    has_colon_prefix_or_exact(table_key, mTablePrefix)
}

/// ParseTableKey decodes the tableKey to get tableID. Note Go checks the prefix `Table`
/// without the colon. Go-shaped.
// go: ParseTableKey L1
pub fn parse_table_key(table_key: &[u8]) -> (i64, Result<(), Error>) {
    if !table_key.starts_with(mTablePrefix.as_bytes()) {
        return (
            0,
            Err(ErrInvalidString.gen_with_stack("fail to parse tableKey", vec![])),
        );
    }
    parse_id(table_key, mTablePrefix)
}

/// SequenceKey encodes the sequence key.
// go: SequenceKey L1
pub fn sequence_key_of(sequence_id: i64) -> Vec<u8> {
    prefixed_id(mSequencePrefix, sequence_id)
}

/// IsSequenceKey checks whether the key is sequence key.
// go: IsSequenceKey L1
pub fn is_sequence_key(key: &[u8]) -> bool {
    has_colon_prefix_or_exact(key, mSequencePrefix)
}

/// ParseSequenceKey decodes the tableID from the sequence key. Go-shaped.
// go: ParseSequenceKey L1
pub fn parse_sequence_key(key: &[u8]) -> (i64, Result<(), Error>) {
    if !is_sequence_key(key) {
        return (
            0,
            Err(ErrInvalidString.gen_with_stack("fail to parse sequence key", vec![])),
        );
    }
    parse_id(key, mSequencePrefix)
}

/// DDLJobHistoryKey is only used for testing.
// go: DDLJobHistoryKey L1
pub fn ddl_job_history_key(m: &Mutator, job_id: i64) -> Vec<u8> {
    m.txn
        .encode_hash_data_key(mDDLJobHistoryKey, &Mutator::job_id_key(job_id))
}

/// MustLoadFilterAttr is used to filter the table info attributes that must be loaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MustLoadFilterAttr {
    /// The attribute text searched in the JSON.
    pub attr: String,
    /// LoadIfMissing: the table must be loaded if the text is missing (else if present).
    pub load_if_missing: bool,
}

const checkForeignKeyAttributesNil: &str = r#""fk_info":null"#;
const checkForeignKeyAttributesZero: &str = r#""fk_info":[]"#;

/// checkAttributesInOrder is the attributes that must be checked in order.
static checkAttributesInOrder: LazyLock<Vec<MustLoadFilterAttr>> = LazyLock::new(|| {
    [
        (r#""partition":null"#, true),
        (r#""Lock":null"#, true),
        (r#""tiflash_replica":null"#, true),
        (r#""temp_table_type":0"#, true),
        (r#""policy_ref_info":null"#, true),
        (r#""ttl_info":null"#, true),
        (r#""affinity":{"#, false),
    ]
    .iter()
    .map(|&(a, l)| MustLoadFilterAttr {
        attr: a.to_string(),
        load_if_missing: l,
    })
    .collect()
});

/// `bytes.Index(s, sep)`.
// glue: bytes.Index
fn bytes_index(s: &[u8], sep: &[u8]) -> Option<usize> {
    if sep.is_empty() {
        return Some(0);
    }
    s.windows(sep.len()).position(|w| w == sep)
}

// go: isTableInfoMustLoad L1
fn is_table_info_must_load_impl(
    json: &[u8],
    is_check_foreign_key_attrs_in_order: bool,
    filter_attrs: &[MustLoadFilterAttr],
) -> bool {
    let mut json = json;
    if is_check_foreign_key_attrs_in_order {
        let idx = bytes_index(json, checkForeignKeyAttributesNil.as_bytes())
            .or_else(|| bytes_index(json, checkForeignKeyAttributesZero.as_bytes()));
        match idx {
            None => return true,
            Some(idx) => json = &json[idx..],
        }
    }
    for filter in filter_attrs {
        match bytes_index(json, filter.attr.as_bytes()) {
            None => {
                if filter.load_if_missing {
                    return true;
                }
                continue;
            }
            Some(idx) => {
                if !filter.load_if_missing {
                    return true;
                }
                json = &json[idx..];
            }
        }
    }
    false
}

/// IsTableInfoMustLoad checks whether the table info needs to be loaded.
/// If the byte representation contains all the given attributes,
/// then it does not need to be loaded and this function will return false.
/// Otherwise, it will return true, indicating that the table info should be loaded.
/// Since attributes are checked in sequence, it's important to choose the order carefully.
// go: IsTableInfoMustLoad L1
pub fn is_table_info_must_load(json: &[u8]) -> bool {
    is_table_info_must_load_impl(json, true, &checkAttributesInOrder)
}

/// NameExtractRegexp is exported for testing.
pub const NameExtractRegexp: &str = r#""O":"([^"\\]*(?:\\.[^"\\]*)*)","#;

/// Unescape is exported for testing.
// go: Unescape L1
pub fn unescape(s: &[u8]) -> Vec<u8> {
    let s = replace_all(s, br#"\""#, br#"""#);
    replace_all(&s, br"\\", br"\")
}

/// `strings.ReplaceAll` on bytes (non-empty `old`).
// glue: strings.ReplaceAll
fn replace_all(s: &[u8], old: &[u8], new: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i..].starts_with(old) {
            out.extend_from_slice(new);
            i += old.len();
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    out
}

/// The regexps of GetAllNameToIDAndTheMustLoadedTableInfo, on bytes: Go's `\d` is ASCII
/// and its `regexp` reads invalid UTF-8 as one U+FFFD per byte, which matches the same
/// spans as byte matching here (`.` and `[^"\\]` never stop inside a multi-byte rune).
static idRegex: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?-u)"id":([0-9]+)"#).expect("valid regexp"));
static nameLRegex: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("(?-u){NameExtractRegexp}")).expect("valid regexp"));

/// detachMagicByte splits the magic byte from the value.
// go: detachMagicByte L1
fn detach_magic_byte(value: &[u8]) -> Result<&[u8], Error> {
    if value.is_empty() {
        panic!("runtime error: slice bounds out of range [:1] with capacity 0");
    }
    let (magic, data) = (value[0], &value[1..]);
    match which_magic_type(magic) {
        typeJSON => {
            if magic != CurrentMagicByteVer {
                return Err(tidb_base::errors::new(
                    "incompatible magic type handling module",
                ));
            }
            Ok(data)
        }
        _ => Err(tidb_base::errors::new("unknown magic type handling module")),
    }
}

// go: whichMagicType L1
fn which_magic_type(b: u8) -> i32 {
    if b <= 0x3F {
        return typeJSON;
    }
    typeUnknown
}

/// `json.Unmarshal(data, &v)` into a fresh `T`: the (possibly partially filled) value and
/// the error.
// glue: json.Unmarshal into a new model value
fn unmarshal<T: Default>(
    data: &[u8],
    f: impl FnOnce(&mut T, &[u8]) -> Result<(), Error>,
) -> (T, Result<(), Error>) {
    let mut v = T::default();
    let r = f(&mut v, data);
    (v, r)
}

/// LastJobIterator is the iterator for gets latest history.
pub struct HLastJobIterator<'a> {
    iter: ReverseHashIterator<'a>,
}

impl HLastJobIterator<'_> {
    /// GetLastJobs gets last several jobs. The name filter (`IsJobMatch` with schema / table
    /// names) is part 2: with no names Go's `IsJobMatch` returns true without reading the
    /// job, which is what this does.
    // go: HLastJobIterator.GetLastJobs L2 (no name filter yet)
    pub async fn get_last_jobs(&mut self, num: usize) -> Result<Vec<Job>, Error> {
        let mut jobs: Vec<Job> = Vec::with_capacity(num);
        while self.iter.valid() && jobs.len() < num {
            let mut job = Job::default();
            job.decode(self.iter.value())?;
            jobs.push(job);
            self.iter.next().await?;
        }
        Ok(jobs)
    }
}

/// Element has the information of the backfill job's type and ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Element {
    /// The element ID.
    pub id: i64,
    /// The element type key.
    pub type_key: Vec<u8>,
}

/// ColumnElementKey is the key for column element.
pub const ColumnElementKey: &[u8] = b"_col_";
/// IndexElementKey is the key for index element.
pub const IndexElementKey: &[u8] = b"_idx_";
const elementKeyLen: usize = 5;

impl Element {
    /// String defines a Stringer function for debugging and pretty printing.
    // go: Element.String L1
    pub fn string(&self) -> Vec<u8> {
        let mut s = format!("ID:{},TypeKey:", self.id).into_bytes();
        s.extend_from_slice(&self.type_key);
        s
    }

    /// EncodeElement encodes an Element into a byte slice.
    /// It's exported for testing.
    // go: Element.EncodeElement L1
    pub fn encode_element(&self) -> Vec<u8> {
        let mut b = vec![0u8; 13];
        let n = self.type_key.len().min(elementKeyLen);
        b[..n].copy_from_slice(&self.type_key[..n]);
        b[elementKeyLen..].copy_from_slice(&(self.id as u64).to_be_bytes());
        b
    }
}

/// DecodeElement decodes values from a byte slice generated with an element.
/// It's exported for testing.
// go: DecodeElement L1
pub fn decode_element(b: &[u8]) -> Result<Element, Error> {
    if b.len() < elementKeyLen + 8 {
        return Err(tidb_base::errors::errorf(
            "invalid encoded element %q length %d",
            &[Arg::Bytes(b.to_vec()), Arg::Int(b.len() as i64, "int")],
        ));
    }
    let prefix = &b[..elementKeyLen];
    let rest = &b[elementKeyLen..];
    let tp = if prefix == IndexElementKey {
        IndexElementKey
    } else if prefix == ColumnElementKey {
        ColumnElementKey
    } else {
        return Err(tidb_base::errors::errorf(
            "invalid encoded element key prefix %q",
            &[Arg::Bytes(prefix.to_vec())],
        ));
    };
    let id = u64::from_be_bytes(rest[..8].try_into().expect("8 bytes"));
    Ok(Element {
        id: id as i64,
        type_key: tp.to_vec(),
    })
}

impl Mutator {
    /// GlobalIDKey returns the key for the global ID.
    // go: Mutator.GlobalIDKey L1
    pub fn global_id_key(&self) -> Vec<u8> {
        self.txn.encode_string_data_key(mNextGlobalIDKey)
    }

    /// GetGlobalID gets current global id. Go-shaped.
    // go: Mutator.GetGlobalID L1
    pub async fn get_global_id(&self) -> (i64, Result<(), Error>) {
        self.txn.get_int64(mNextGlobalIDKey).await
    }

    /// GetPolicyID gets current policy global id. Go-shaped.
    // go: Mutator.GetPolicyID L1
    pub async fn get_policy_id(&self) -> (i64, Result<(), Error>) {
        self.txn.get_int64(mPolicyGlobalID).await
    }

    // go: Mutator.policyKey L1
    fn policy_key(policy_id: i64) -> Vec<u8> {
        prefixed_id(mPolicyPrefix, policy_id)
    }

    // go: Mutator.resourceGroupKey L1
    fn resource_group_key(group_id: i64) -> Vec<u8> {
        prefixed_id(mResourceGroupPrefix, group_id)
    }

    // go: Mutator.dbKey L1
    fn db_key(db_id: i64) -> Vec<u8> {
        db_key_of(db_id)
    }

    // go: Mutator.autoTableIDKey L1
    pub(crate) fn auto_table_id_key(table_id: i64) -> Vec<u8> {
        auto_table_id_key(table_id)
    }

    // go: Mutator.autoIncrementIDKey L1
    pub(crate) fn auto_increment_id_key(table_id: i64) -> Vec<u8> {
        auto_increment_id_key(table_id)
    }

    // go: Mutator.autoRandomTableIDKey L1
    pub(crate) fn auto_random_table_id_key(table_id: i64) -> Vec<u8> {
        auto_random_table_id_key(table_id)
    }

    // go: Mutator.tableKey L1
    fn table_key(table_id: i64) -> Vec<u8> {
        table_key_of(table_id)
    }

    // go: Mutator.sequenceKey L1
    pub(crate) fn sequence_key(sequence_id: i64) -> Vec<u8> {
        sequence_key_of(sequence_id)
    }

    // go: Mutator.sequenceCycleKey L1
    pub(crate) fn sequence_cycle_key(sequence_id: i64) -> Vec<u8> {
        prefixed_id(mSeqCyclePrefix, sequence_id)
    }

    /// GetAutoIDAccessors gets the controller for auto IDs.
    // go: Mutator.GetAutoIDAccessors L1
    pub fn get_auto_id_accessors(&self, db_id: i64, table_id: i64) -> AutoIDAccessors<'_> {
        new_auto_id_accessors(self, db_id, table_id)
    }

    /// GetSchemaVersionWithNonEmptyDiff gets current global schema version, if diff is nil,
    /// we should return version - 1. Consider the following scenario:
    /// t1            t2              t3             t4
    /// |             |               |
    /// update schema version         |              set diff
    /// stale readers can't see diff now, so we should return version - 1.
    // go: Mutator.GetSchemaVersionWithNonEmptyDiff L1
    pub async fn get_schema_version_with_non_empty_diff(&self) -> Result<i64, Error> {
        let (mut v, err) = self.txn.get_int64(mSchemaVersionKey).await;
        err?;
        let (diff, err) = self.get_schema_diff(v).await;
        err?;
        if diff.is_none() && v > 0 {
            v -= 1;
        }
        Ok(v)
    }

    /// EncodeSchemaDiffKey returns the raw kv key for a schema diff
    // go: Mutator.EncodeSchemaDiffKey L1
    pub fn encode_schema_diff_key(&self, schema_version: i64) -> Vec<u8> {
        let diff_key = Self::schema_diff_key(schema_version);
        self.txn.encode_string_data_key(&diff_key)
    }

    /// GetSchemaVersion gets current global schema version. Go-shaped.
    // go: Mutator.GetSchemaVersion L1
    pub async fn get_schema_version(&self) -> (i64, Result<(), Error>) {
        self.txn.get_int64(mSchemaVersionKey).await
    }

    // go: Mutator.checkDBExists L1
    async fn check_db_exists(&self, db_key: &[u8]) -> Result<(), Error> {
        let v = self.txn.h_get(mDBs, db_key).await?;
        if v.is_none() {
            return Err(ErrDBNotExists.gen_with_stack("database doesn't exist", vec![]));
        }
        Ok(())
    }

    /// GetBDRRole get BDR role from storage (Go `string`, bytes).
    // go: Mutator.GetBDRRole L1
    pub async fn get_bdr_role(&self) -> Result<Vec<u8>, Error> {
        Ok(self.txn.get(mBDRRole).await?.unwrap_or_default())
    }

    /// CheckDDLTableVersion check if the tables related to concurrent DDL exists. Go-shaped:
    /// -1 with an error.
    // go: Mutator.CheckDDLTableVersion L1
    pub async fn check_ddl_table_version(&self) -> (DDLTableVersion, Result<(), Error>) {
        let v = match self.txn.get(mDDLTableVersion).await {
            Err(e) => return (-1, Err(e)),
            Ok(v) => v.unwrap_or_default(),
        };
        if v.is_empty() {
            return (InitDDLTableVersion, Ok(()));
        }
        match strconv::atoi(&v) {
            (_, Err(e)) => (-1, Err(Error::plain(e))),
            (ver, Ok(())) => (ver, Ok(())),
        }
    }

    /// GetSystemDBID gets the system DB ID. return (0, nil) indicates that the system DB
    /// does not exist.
    // go: Mutator.GetSystemDBID L1
    pub async fn get_system_db_id(&self) -> Result<i64, Error> {
        let dbs = self.list_databases().await?;
        for db in dbs {
            if db.name.l == mysql::SystemDB {
                return Ok(db.id);
            }
        }
        Ok(0)
    }

    /// GetMetadataLock gets the metadata lock: (enable, isNull). Go-shaped.
    // go: Mutator.GetMetadataLock L1
    pub async fn get_metadata_lock(&self) -> (bool, bool, Result<(), Error>) {
        let val = match self.txn.get(mMetaDataLock).await {
            Err(e) => return (false, false, Err(e)),
            Ok(v) => v.unwrap_or_default(),
        };
        if val.is_empty() {
            return (false, true, Ok(()));
        }
        (val == b"1", false, Ok(()))
    }

    /// GetSchemaCacheSize gets the schema cache size: (size, isNull). Go-shaped.
    // go: Mutator.GetSchemaCacheSize L1
    pub async fn get_schema_cache_size(&self) -> (u64, bool, Result<(), Error>) {
        let val = match self.txn.get(mSchemaCacheSize).await {
            Err(e) => return (0, false, Err(e)),
            Ok(v) => v.unwrap_or_default(),
        };
        if val.is_empty() {
            return (0, true, Ok(()));
        }
        let (size, err) = strconv::parse_uint(&val, "ParseUint");
        (size, false, err.map_err(Error::plain))
    }

    /// IterDatabases iterates all the DBInfo.
    // go: Mutator.IterDatabases L1
    pub async fn iter_databases(
        &self,
        mut f: impl FnMut(DBInfo) -> Result<(), Error>,
    ) -> Result<(), Error> {
        self.txn
            .h_get_iter(mDBs, |r| {
                let (db_info, err) = unmarshal(&r.value, DBInfo::unmarshal_json);
                err?;
                f(db_info)
            })
            .await
    }

    /// IterTables iterates all the table at once, in order to avoid oom.
    // go: Mutator.IterTables L1
    pub async fn iter_tables(
        &self,
        db_id: i64,
        mut f: impl FnMut(TableInfo) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let db_key = Self::db_key(db_id);
        self.check_db_exists(&db_key).await?;
        self.txn
            .h_get_iter(&db_key, |r| {
                if !r.field.starts_with(mTablePrefix.as_bytes()) {
                    return Ok(());
                }
                let (mut tb_info, err) = unmarshal(&r.value, TableInfo::unmarshal_json);
                err?;
                tb_info.dbid = db_id;
                f(tb_info)
            })
            .await
    }

    /// GetMetasByDBID return all meta information of a database.
    /// Note(dongmen): This method is used by TiCDC to reduce the time of changefeed
    /// initialization. Ref: https://github.com/pingcap/tiflow/issues/11109
    // go: Mutator.GetMetasByDBID L1
    pub async fn get_metas_by_db_id(&self, db_id: i64) -> Result<Vec<HashPair>, Error> {
        let db_key = Self::db_key(db_id);
        self.check_db_exists(&db_key).await?;
        let (res, err) = self.txn.h_get_all(&db_key).await;
        err.map(|()| res)
    }

    /// GetAllNameToIDAndTheMustLoadedTableInfo gets all the fields and values and table
    /// info for special attributes in a hash. It's used to get some infos for information
    /// schema cache in a faster way. If a table contains any of the attributes listed in
    /// checkSubstringsInOrder, it must be loaded during schema full load. hasSpecialAttributes()
    /// is a subset of it, the difference is that: If a table need to be resident in-memory,
    /// its table info MUST be loaded. If a table info is loaded, it's NOT NECESSARILY to be
    /// keep in-memory. Go-shaped: what was gathered before an error is returned with it.
    /// Panics like Go when a table value has no `"id":<digits>` or name (regexp miss).
    // go: Mutator.GetAllNameToIDAndTheMustLoadedTableInfo L1
    pub async fn get_all_name_to_id_and_the_must_loaded_table_info(
        &self,
        db_id: i64,
    ) -> (BTreeMap<Vec<u8>, i64>, Vec<TableInfo>, Result<(), Error>) {
        let db_key = Self::db_key(db_id);
        if let Err(e) = self.check_db_exists(&db_key).await {
            return (BTreeMap::new(), Vec::new(), Err(e));
        }
        let mut res = BTreeMap::new();
        let mut table_infos = Vec::new();
        let err = self
            .txn
            .iterate_hash(&db_key, |field, value| {
                if !field.starts_with(b"Table") {
                    return Ok(());
                }
                let id_match = idRegex.captures(value);
                let name_l_match = nameLRegex.captures(value);
                let Some(id_match) = id_match else {
                    panic!("runtime error: index out of range [1] with length 0");
                };
                let (id, err) = strconv::atoi(id_match.get(1).map_or(&[][..], |m| m.as_bytes()));
                err.map_err(Error::plain)?;
                let name = match name_l_match {
                    Some(c) => c.get(1).map(|m| m.as_bytes().to_vec()).unwrap_or_default(),
                    None => panic!("runtime error: index out of range [1] with length 0"),
                };
                let key = unescape(&name);
                res.insert(key, id);
                if is_table_info_must_load_impl(value, true, &checkAttributesInOrder) {
                    let (mut tb_info, err) = unmarshal(value, TableInfo::unmarshal_json);
                    err?;
                    tb_info.dbid = db_id;
                    table_infos.push(tb_info);
                }
                Ok(())
            })
            .await;
        (res, table_infos, err)
    }

    /// ListTables shows all tables in database.
    // go: Mutator.ListTables L2 (no context: tidb-kv's Context has no cancellation)
    pub async fn list_tables(&self, db_id: i64) -> Result<Vec<TableInfo>, Error> {
        let res = self.get_metas_by_db_id(db_id).await?;
        let mut tables = Vec::with_capacity(res.len() / 2);
        for r in res {
            if !r.field.starts_with(mTablePrefix.as_bytes()) {
                continue;
            }
            let (mut tb_info, err) = unmarshal(&r.value, TableInfo::unmarshal_json);
            err?;
            tb_info.dbid = db_id;
            tables.push(tb_info);
        }
        Ok(tables)
    }

    /// ListDatabases shows all databases.
    // go: Mutator.ListDatabases L1
    pub async fn list_databases(&self) -> Result<Vec<DBInfo>, Error> {
        let (res, err) = self.txn.h_get_all(mDBs).await;
        err?;
        let mut dbs = Vec::with_capacity(res.len());
        for r in res {
            let (db_info, err) = unmarshal(&r.value, DBInfo::unmarshal_json);
            err?;
            dbs.push(db_info);
        }
        Ok(dbs)
    }

    /// GetDatabase gets the database value with ID. Go-shaped: a value that does not decode
    /// is returned (partially decoded) with the error.
    // go: Mutator.GetDatabase L1
    pub async fn get_database(&self, db_id: i64) -> (Option<DBInfo>, Result<(), Error>) {
        let db_key = Self::db_key(db_id);
        let value = match self.txn.h_get(mDBs, &db_key).await {
            Err(e) => return (None, Err(e)),
            Ok(None) => return (None, Ok(())),
            Ok(Some(v)) => v,
        };
        let (db_info, err) = unmarshal(&value, DBInfo::unmarshal_json);
        (Some(db_info), err)
    }

    /// ListPolicies shows all policies.
    // go: Mutator.ListPolicies L1
    pub async fn list_policies(&self) -> Result<Vec<PolicyInfo>, Error> {
        let (res, err) = self.txn.h_get_all(mPolicies).await;
        err?;
        let mut policies = Vec::with_capacity(res.len());
        for r in res {
            let value = detach_magic_byte(&r.value)?;
            let (policy, err) = unmarshal(value, PolicyInfo::unmarshal_json);
            err?;
            policies.push(policy);
        }
        Ok(policies)
    }

    /// GetPolicy gets the database value with ID. Go-shaped.
    // go: Mutator.GetPolicy L1
    pub async fn get_policy(&self, policy_id: i64) -> (Option<PolicyInfo>, Result<(), Error>) {
        let policy_key = Self::policy_key(policy_id);
        let value = match self.txn.h_get(mPolicies, &policy_key).await {
            Err(e) => return (None, Err(e)),
            Ok(None) => {
                return (
                    None,
                    Err(ErrPolicyNotExists.gen_with_stack(
                        "policy id : %d doesn't exist",
                        vec![Arg::Int(policy_id, "int64")],
                    )),
                );
            }
            Ok(Some(v)) => v,
        };
        let value = match detach_magic_byte(&value) {
            Err(e) => return (None, Err(e)),
            Ok(v) => v,
        };
        let (policy, err) = unmarshal(value, PolicyInfo::unmarshal_json);
        (Some(policy), err)
    }

    /// ListResourceGroups shows all resource groups (with the default group if none is
    /// named `default`).
    // go: Mutator.ListResourceGroups L1
    pub async fn list_resource_groups(&self) -> Result<Vec<ResourceGroupInfo>, Error> {
        let (res, err) = self.txn.h_get_all(mResourceGroups).await;
        err?;
        let mut has_default = false;
        let mut groups = Vec::with_capacity(res.len());
        for r in res {
            let value = detach_magic_byte(&r.value)?;
            let (group, err) = unmarshal(value, ResourceGroupInfo::unmarshal_json);
            err?;
            has_default = has_default || group.name.l == "default";
            groups.push(group);
        }
        if !has_default {
            groups.push(default_r_group_meta());
        }
        Ok(groups)
    }

    /// GetResourceGroup gets the database value with ID. Go-shaped.
    // go: Mutator.GetResourceGroup L1
    pub async fn get_resource_group(
        &self,
        group_id: i64,
    ) -> (Option<ResourceGroupInfo>, Result<(), Error>) {
        let group_key = Self::resource_group_key(group_id);
        let value = match self.txn.h_get(mResourceGroups, &group_key).await {
            Err(e) => return (None, Err(e)),
            Ok(None) => {
                // the default group is not persistent to tikv by default.
                if group_id == defaultGroupID {
                    return (Some(default_r_group_meta()), Ok(()));
                }
                return (
                    None,
                    Err(ErrResourceGroupNotExists.gen_with_stack(
                        "resource group id : %d doesn't exist",
                        vec![Arg::Int(group_id, "int64")],
                    )),
                );
            }
            Ok(Some(v)) => v,
        };
        let value = match detach_magic_byte(&value) {
            Err(e) => return (None, Err(e)),
            Ok(v) => v,
        };
        let (group, err) = unmarshal(value, ResourceGroupInfo::unmarshal_json);
        (Some(group), err)
    }

    /// GetTable gets the table value in database with tableID. Go-shaped.
    // go: Mutator.GetTable L1
    pub async fn get_table(
        &self,
        db_id: i64,
        table_id: i64,
    ) -> (Option<TableInfo>, Result<(), Error>) {
        let db_key = Self::db_key(db_id);
        if let Err(e) = self.check_db_exists(&db_key).await {
            return (None, Err(e));
        }
        let table_key = Self::table_key(table_id);
        let value = match self.txn.h_get(&db_key, &table_key).await {
            Err(e) => return (None, Err(e)),
            Ok(None) => return (None, Ok(())),
            Ok(Some(v)) => v,
        };
        let (mut table_info, err) = unmarshal(&value, TableInfo::unmarshal_json);
        table_info.dbid = db_id;
        (Some(table_info), err)
    }

    /// CheckTableExists checks if the table is existed with dbID and tableID.
    // go: Mutator.CheckTableExists L1
    pub async fn check_table_exists(&self, db_id: i64, table_id: i64) -> Result<bool, Error> {
        let db_key = Self::db_key(db_id);
        self.check_db_exists(&db_key).await?;
        let table_key = Self::table_key(table_id);
        let v = self.txn.h_get(&db_key, &table_key).await?;
        Ok(v.is_some())
    }

    /// GetAllDDLJobsInQueue gets all DDL Jobs in the current queue. The default queue is
    /// the Mutator's (empty for a [`new_reader`] reader, as in Go).
    // go: Mutator.GetAllDDLJobsInQueue L1
    pub async fn get_all_ddl_jobs_in_queue(
        &self,
        job_list_key: Option<&[u8]>,
    ) -> Result<Vec<Job>, Error> {
        let list_key = job_list_key.unwrap_or(&self.job_list_key);
        let Some(values) = self.txn.l_get_all(list_key).await? else {
            return Ok(Vec::new());
        };
        let mut jobs = Vec::with_capacity(values.len());
        for val in values {
            let mut job = Job::default();
            job.decode(&val)?;
            jobs.push(job);
        }
        Ok(jobs)
    }

    // go: Mutator.jobIDKey L1
    fn job_id_key(id: i64) -> [u8; 8] {
        (id as u64).to_be_bytes()
    }

    // go: Mutator.getHistoryDDLJob L1
    async fn get_history_ddl_job_inner(
        &self,
        key: &[u8],
        id: i64,
    ) -> (Option<Job>, Result<(), Error>) {
        let value = match self.txn.h_get(key, &Self::job_id_key(id)).await {
            Err(e) => return (None, Err(e)),
            Ok(None) => return (None, Ok(())),
            Ok(Some(v)) => v,
        };
        let mut job = Job::default();
        let err = job.decode(&value);
        (Some(job), err)
    }

    /// GetHistoryDDLJob gets a history DDL job. Go-shaped. (Go observes a metrics
    /// histogram here; tidb-rs has no meta metrics yet.)
    // go: Mutator.GetHistoryDDLJob L2 (no MetaHistogram)
    pub async fn get_history_ddl_job(&self, id: i64) -> (Option<Job>, Result<(), Error>) {
        self.get_history_ddl_job_inner(mDDLJobHistoryKey, id).await
    }

    /// GetHistoryDDLCount the count of all history DDL jobs. Go-shaped.
    // go: Mutator.GetHistoryDDLCount L1
    pub async fn get_history_ddl_count(&self) -> (u64, Result<(), Error>) {
        self.txn.h_get_len(mDDLJobHistoryKey).await
    }

    /// An int ingest parameter: (val, isNull). Go-shaped.
    // glue: the shared body of GetIngestMaxBatchSplitRanges / GetIngestMaxInflight
    async fn get_ingest_int(&self, key: &[u8]) -> (i64, bool, Result<(), Error>) {
        let s_val = match self.txn.get(key).await {
            Err(e) => return (0, false, Err(e)),
            Ok(None) => return (0, true, Ok(())),
            Ok(Some(v)) => v,
        };
        let (val, err) = strconv::atoi(&s_val);
        (val, false, err.map_err(Error::plain))
    }

    /// A float ingest parameter: (val, isNull). Go-shaped.
    // glue: the shared body of GetIngestMaxSplitRangesPerSec / GetIngestMaxPerSec
    async fn get_ingest_float(&self, key: &[u8]) -> (f64, bool, Result<(), Error>) {
        let s_val = match self.txn.get(key).await {
            Err(e) => return (0.0, false, Err(e)),
            Ok(None) => return (0.0, true, Ok(())),
            Ok(Some(v)) => v,
        };
        let (val, err) = tidb_types::gostd::parse_float(&String::from_utf8_lossy(&s_val));
        (val, false, err.map_err(|e| Error::plain(e.to_string())))
    }

    /// GetIngestMaxBatchSplitRanges gets the max batch split ranges for ingest.
    // go: Mutator.GetIngestMaxBatchSplitRanges L1
    pub async fn get_ingest_max_batch_split_ranges(&self) -> (i64, bool, Result<(), Error>) {
        self.get_ingest_int(mIngestMaxBatchSplitRangesKey).await
    }

    /// GetIngestMaxSplitRangesPerSec gets the max split ranges per second for ingest.
    // go: Mutator.GetIngestMaxSplitRangesPerSec L1
    pub async fn get_ingest_max_split_ranges_per_sec(&self) -> (f64, bool, Result<(), Error>) {
        self.get_ingest_float(mIngestMaxSplitRangesPerSecKey).await
    }

    /// GetIngestMaxInflight gets the max inflight for ingest.
    // go: Mutator.GetIngestMaxInflight L1
    pub async fn get_ingest_max_inflight(&self) -> (i64, bool, Result<(), Error>) {
        self.get_ingest_int(mIngestMaxInflightKey).await
    }

    /// GetIngestMaxPerSec gets the max per second for ingest.
    // go: Mutator.GetIngestMaxPerSec L1
    pub async fn get_ingest_max_per_sec(&self) -> (f64, bool, Result<(), Error>) {
        self.get_ingest_float(mIngestMaxPerSecKey).await
    }

    /// GetLastHistoryDDLJobsIterator gets latest N history DDL jobs iterator.
    // go: Mutator.GetLastHistoryDDLJobsIterator L1
    pub async fn get_last_history_ddl_jobs_iterator(&self) -> Result<HLastJobIterator<'_>, Error> {
        let iter = new_hash_reverse_iter(&self.txn, mDDLJobHistoryKey).await?;
        Ok(HLastJobIterator { iter })
    }

    /// GetHistoryDDLJobsIterator gets the jobs iterator begin with startJobID.
    // go: Mutator.GetHistoryDDLJobsIterator L1
    pub async fn get_history_ddl_jobs_iterator(
        &self,
        start_job_id: i64,
    ) -> Result<HLastJobIterator<'_>, Error> {
        let field = Self::job_id_key(start_job_id);
        let iter =
            new_hash_reverse_iter_begin_with_field(&self.txn, mDDLJobHistoryKey, &field).await?;
        Ok(HLastJobIterator { iter })
    }

    /// GetBootstrapVersion returns the version of the server which bootstrap the store.
    /// If the store is not bootstraped, the version will be zero. Go-shaped.
    // go: Mutator.GetBootstrapVersion L1
    pub async fn get_bootstrap_version(&self) -> (i64, Result<(), Error>) {
        self.txn.get_int64(mBootstrapKey).await
    }

    // go: Mutator.schemaDiffKey L1
    fn schema_diff_key(schema_version: i64) -> Vec<u8> {
        prefixed_id(mSchemaDiffPrefix, schema_version)
    }

    /// GetSchemaDiff gets the modification information on a given schema version.
    /// Go-shaped. (Go observes a metrics histogram here; tidb-rs has no meta metrics yet.)
    // go: Mutator.GetSchemaDiff L2 (no MetaHistogram)
    pub async fn get_schema_diff(
        &self,
        schema_version: i64,
    ) -> (Option<SchemaDiff>, Result<(), Error>) {
        let diff_key = Self::schema_diff_key(schema_version);
        let data = match self.txn.get(&diff_key).await {
            Err(e) => return (None, Err(e)),
            Ok(v) => v.unwrap_or_default(),
        };
        if data.is_empty() {
            return (None, Ok(()));
        }
        let (diff, err) = unmarshal(&data, SchemaDiff::unmarshal_json);
        (Some(diff), err)
    }
}

/// GetTableInfoWithAttributes retrieves all the table infos for a given db. The filterAttrs
/// are used to filter out any table that is not needed. Go-shaped: what was gathered before
/// an error is returned with it.
// go: GetTableInfoWithAttributes L1
pub async fn get_table_info_with_attributes(
    m: &Mutator,
    db_id: i64,
    filter_attrs: &[MustLoadFilterAttr],
) -> (Vec<TableInfo>, Result<(), Error>) {
    let db_key = Mutator::db_key(db_id);
    if let Err(e) = m.check_db_exists(&db_key).await {
        return (Vec::new(), Err(e));
    }
    let mut table_infos = Vec::new();
    let err = m
        .txn
        .iterate_hash(&db_key, |field, value| {
            if !field.starts_with(b"Table") {
                return Ok(());
            }
            if is_table_info_must_load_impl(value, false, filter_attrs) {
                let (mut tb_info, err) = unmarshal(value, TableInfo::unmarshal_json);
                err?;
                tb_info.dbid = db_id;
                table_infos.push(tb_info);
            }
            Ok(())
        })
        .await;
    (table_infos, err)
}
