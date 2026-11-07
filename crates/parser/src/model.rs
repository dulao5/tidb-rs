//! go-file: pkg/parser/model/model.go
//!
//! Block D02.3: the enums and `CIStr` of `parser/model` (meta/model, the planner and the
//! executor use them). Go-named constants per M-GONAME; Go `int`/`byte` per M-INT.

#![allow(non_upper_case_globals)]

use tidb_base::Error;
use tidb_base::gostd::json::{self, Decoder, Raw};

/// TableLockType is the type of the table lock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TableLockType(pub u8);

/// TableLockNone means this table lock is absent.
pub const TableLockNone: TableLockType = TableLockType(0);
/// TableLockRead means the session with this lock can read the table (but not write it).
pub const TableLockRead: TableLockType = TableLockType(1);
/// TableLockReadLocal is not supported.
pub const TableLockReadLocal: TableLockType = TableLockType(2);
/// TableLockReadOnly is used to set a table into read-only status.
pub const TableLockReadOnly: TableLockType = TableLockType(3);
/// TableLockWrite means only the session with this lock has write/read permission.
pub const TableLockWrite: TableLockType = TableLockType(4);
/// TableLockWriteLocal means the session with this lock has write/read permission, and the
/// other session still has read permission.
pub const TableLockWriteLocal: TableLockType = TableLockType(5);

impl TableLockType {
    /// String implements fmt.Stringer interface.
    // go: TableLockType.String L1 [M-NAME]
    pub fn to_string(self) -> &'static str {
        match self {
            TableLockNone => "NONE",
            TableLockRead => "READ",
            TableLockReadLocal => "READ LOCAL",
            TableLockReadOnly => "READ ONLY",
            TableLockWriteLocal => "WRITE LOCAL",
            TableLockWrite => "WRITE",
            _ => "",
        }
    }
}

/// ViewAlgorithm is VIEW's SQL ALGORITHM characteristic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ViewAlgorithm(pub isize);

/// AlgorithmUndefined is `ALGORITHM = UNDEFINED`.
pub const AlgorithmUndefined: ViewAlgorithm = ViewAlgorithm(0);
/// AlgorithmMerge is `ALGORITHM = MERGE`.
pub const AlgorithmMerge: ViewAlgorithm = ViewAlgorithm(1);
/// AlgorithmTemptable is `ALGORITHM = TEMPTABLE`.
pub const AlgorithmTemptable: ViewAlgorithm = ViewAlgorithm(2);

impl ViewAlgorithm {
    /// String implements fmt.Stringer interface.
    // go: ViewAlgorithm.String L1 [M-NAME]
    pub fn to_string(self) -> &'static str {
        match self {
            AlgorithmMerge => "MERGE",
            AlgorithmTemptable => "TEMPTABLE",
            AlgorithmUndefined => "UNDEFINED",
            _ => "UNDEFINED",
        }
    }
}

/// ViewSecurity is VIEW's SQL SECURITY characteristic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ViewSecurity(pub isize);

/// SecurityDefiner is `SQL SECURITY DEFINER`.
pub const SecurityDefiner: ViewSecurity = ViewSecurity(0);
/// SecurityInvoker is `SQL SECURITY INVOKER`.
pub const SecurityInvoker: ViewSecurity = ViewSecurity(1);

impl ViewSecurity {
    /// String implements fmt.Stringer interface.
    // go: ViewSecurity.String L1 [M-NAME]
    pub fn to_string(self) -> &'static str {
        match self {
            SecurityInvoker => "INVOKER",
            SecurityDefiner => "DEFINER",
            _ => "DEFINER",
        }
    }
}

/// ViewCheckOption is VIEW's WITH CHECK OPTION clause part.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ViewCheckOption(pub isize);

/// CheckOptionLocal is `WITH LOCAL CHECK OPTION`.
pub const CheckOptionLocal: ViewCheckOption = ViewCheckOption(0);
/// CheckOptionCascaded is `WITH CASCADED CHECK OPTION`.
pub const CheckOptionCascaded: ViewCheckOption = ViewCheckOption(1);

impl ViewCheckOption {
    /// String implements fmt.Stringer interface.
    // go: ViewCheckOption.String L1 [M-NAME]
    pub fn to_string(self) -> &'static str {
        match self {
            CheckOptionLocal => "LOCAL",
            CheckOptionCascaded => "CASCADED",
            _ => "CASCADED",
        }
    }
}

/// PartitionType is the type for PartitionInfo.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PartitionType(pub isize);

/// PartitionTypeNone: actually non-partitioned, but during DDL keeping the table as a
/// single partition.
pub const PartitionTypeNone: PartitionType = PartitionType(0);
/// PartitionTypeRange is `PARTITION BY RANGE`.
pub const PartitionTypeRange: PartitionType = PartitionType(1);
/// PartitionTypeHash is `PARTITION BY HASH`.
pub const PartitionTypeHash: PartitionType = PartitionType(2);
/// PartitionTypeList is `PARTITION BY LIST`.
pub const PartitionTypeList: PartitionType = PartitionType(3);
/// PartitionTypeKey is `PARTITION BY KEY`.
pub const PartitionTypeKey: PartitionType = PartitionType(4);
/// PartitionTypeSystemTime is `PARTITION BY SYSTEM_TIME`.
pub const PartitionTypeSystemTime: PartitionType = PartitionType(5);

impl PartitionType {
    /// String implements fmt.Stringer interface.
    // go: PartitionType.String L1 [M-NAME]
    pub fn to_string(self) -> &'static str {
        match self {
            PartitionTypeRange => "RANGE",
            PartitionTypeHash => "HASH",
            PartitionTypeList => "LIST",
            PartitionTypeKey => "KEY",
            PartitionTypeSystemTime => "SYSTEM_TIME",
            PartitionTypeNone => "NONE",
            _ => "",
        }
    }
}

/// PrimaryKeyType is the type of primary key: "clustered", "nonclustered", and ""(default).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PrimaryKeyType(pub i8);

/// PrimaryKeyTypeDefault is the default primary key type.
pub const PrimaryKeyTypeDefault: PrimaryKeyType = PrimaryKeyType(0);
/// PrimaryKeyTypeClustered is `CLUSTERED`.
pub const PrimaryKeyTypeClustered: PrimaryKeyType = PrimaryKeyType(1);
/// PrimaryKeyTypeNonClustered is `NONCLUSTERED`.
pub const PrimaryKeyTypeNonClustered: PrimaryKeyType = PrimaryKeyType(2);

impl PrimaryKeyType {
    /// String implements fmt.Stringer interface.
    // go: PrimaryKeyType.String L1 [M-NAME]
    pub fn to_string(self) -> &'static str {
        match self {
            PrimaryKeyTypeClustered => "CLUSTERED",
            PrimaryKeyTypeNonClustered => "NONCLUSTERED",
            _ => "",
        }
    }
}

/// IndexType is the type of index.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexType(pub isize);

/// IndexTypeInvalid is an unset index type.
pub const IndexTypeInvalid: IndexType = IndexType(0);
/// IndexTypeBtree is `USING BTREE`.
pub const IndexTypeBtree: IndexType = IndexType(1);
/// IndexTypeHash is `USING HASH`.
pub const IndexTypeHash: IndexType = IndexType(2);
/// IndexTypeRtree is `USING RTREE`.
pub const IndexTypeRtree: IndexType = IndexType(3);
/// IndexTypeHypo is a hypothetical index.
pub const IndexTypeHypo: IndexType = IndexType(4);
/// IndexTypeHNSW is a vector index.
pub const IndexTypeHNSW: IndexType = IndexType(5);

impl IndexType {
    /// String implements Stringer interface.
    // go: IndexType.String L1 [M-NAME]
    pub fn to_string(self) -> &'static str {
        match self {
            IndexTypeBtree => "BTREE",
            IndexTypeHash => "HASH",
            IndexTypeRtree => "RTREE",
            IndexTypeHypo => "HYPO",
            IndexTypeHNSW => "HNSW",
            _ => "",
        }
    }
}

/// ReferOptionType is the type for refer options.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReferOptionType(pub isize);

/// ReferOptionNoOption: no refer option.
pub const ReferOptionNoOption: ReferOptionType = ReferOptionType(0);
/// ReferOptionRestrict is `RESTRICT`.
pub const ReferOptionRestrict: ReferOptionType = ReferOptionType(1);
/// ReferOptionCascade is `CASCADE`.
pub const ReferOptionCascade: ReferOptionType = ReferOptionType(2);
/// ReferOptionSetNull is `SET NULL`.
pub const ReferOptionSetNull: ReferOptionType = ReferOptionType(3);
/// ReferOptionNoAction is `NO ACTION`.
pub const ReferOptionNoAction: ReferOptionType = ReferOptionType(4);
/// ReferOptionSetDefault is `SET DEFAULT`.
pub const ReferOptionSetDefault: ReferOptionType = ReferOptionType(5);

impl ReferOptionType {
    /// String implements fmt.Stringer interface.
    // go: ReferOptionType.String L1 [M-NAME]
    pub fn to_string(self) -> &'static str {
        match self {
            ReferOptionRestrict => "RESTRICT",
            ReferOptionCascade => "CASCADE",
            ReferOptionSetNull => "SET NULL",
            ReferOptionNoAction => "NO ACTION",
            ReferOptionSetDefault => "SET DEFAULT",
            _ => "",
        }
    }
}

/// CIStr is case insensitive string.
///
/// The fields are Go strings, held here as `String`: identifiers reach `CIStr` through the
/// lexer (valid UTF-8) or from schema JSON, whose decoder already replaces invalid UTF-8.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct CIStr {
    /// Original string (JSON key `O`).
    pub o: String,
    /// Lower case string (JSON key `L`).
    pub l: String,
}

impl CIStr {
    /// String implements fmt.Stringer interface.
    // go: CIStr.String L1 [M-NAME]
    pub fn to_string(&self) -> &str {
        &self.o
    }

    /// NewCIStr creates a new CIStr.
    // go: NewCIStr L1
    pub fn new(s: &str) -> CIStr {
        CIStr {
            o: s.to_owned(),
            l: tidb_base::gostd::to_lower(s),
        }
    }

    /// The struct decoding of `type T CIStr` into `cis` (in place, like Go's `(*T)(cis)`).
    // go-part: CIStr.UnmarshalJSON
    fn decode_struct(cis: &mut CIStr, d: &mut Decoder, v: &Raw) {
        d.object(v, "T", "model.T", &["O", "L"], |d, i, v| match i {
            0 => d.string(v, &mut cis.o),
            _ => d.string(v, &mut cis.l),
        });
    }

    /// UnmarshalJSON implements the user defined unmarshal method. CIStr can be unmarshaled
    /// from a single string, so PartitionDefinition.Name stays compatible during upgrades.
    // go: CIStr.UnmarshalJSON L1 [M-ERR]
    pub fn unmarshal_json(&mut self, b: &[u8]) -> Result<(), Error> {
        // json.Unmarshal(b, (*T)(cis))
        let first = json::unmarshal_raw(b).map(|v| {
            let mut d = Decoder::new();
            Self::decode_struct(self, &mut d, &v);
            d.finish()
        });
        if let Ok(Ok(())) = first {
            return Ok(());
        }
        // Unmarshal CIStr from a single string.
        let v = json::unmarshal_raw(b).map_err(Error::plain)?;
        let mut d = Decoder::new();
        d.string(&v, &mut self.o);
        d.finish().map_err(Error::plain)?;
        self.l = tidb_base::gostd::to_lower(&self.o);
        Ok(())
    }

    /// `json.Marshal(cis)`: the default struct encoding `{"O":…,"L":…}`.
    // glue: encoding/json struct encoder of CIStr (no MarshalJSON in Go)
    pub fn marshal_json(&self) -> String {
        let mut out = String::from("{\"O\":");
        json::append_string(&mut out, &self.o);
        out.push_str(",\"L\":");
        json::append_string(&mut out, &self.l);
        out.push('}');
        out
    }

    /// MemoryUsage return the memory usage of CIStr: two Go string headers (16 bytes each on
    /// 64-bit) plus the bytes of both strings.
    // go: CIStr.MemoryUsage L1
    pub fn memory_usage(&self) -> i64 {
        16 * 2 + (self.o.len() + self.l.len()) as i64
    }
}

/// RunawayActionType is the type of runaway action.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RunawayActionType(pub i32);

/// RunawayActionNone: no action.
pub const RunawayActionNone: RunawayActionType = RunawayActionType(0);
/// RunawayActionDryRun only records the runaway query.
pub const RunawayActionDryRun: RunawayActionType = RunawayActionType(1);
/// RunawayActionCooldown lowers the priority of the runaway query.
pub const RunawayActionCooldown: RunawayActionType = RunawayActionType(2);
/// RunawayActionKill kills the runaway query.
pub const RunawayActionKill: RunawayActionType = RunawayActionType(3);
/// RunawayActionSwitchGroup switches the runaway query to another resource group.
pub const RunawayActionSwitchGroup: RunawayActionType = RunawayActionType(4);

/// RunawayWatchType is the type of runaway watch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RunawayWatchType(pub i32);

/// WatchNone: no watch.
pub const WatchNone: RunawayWatchType = RunawayWatchType(0);
/// WatchExact matches the exact SQL text.
pub const WatchExact: RunawayWatchType = RunawayWatchType(1);
/// WatchSimilar matches the SQL digest.
pub const WatchSimilar: RunawayWatchType = RunawayWatchType(2);
/// WatchPlan matches the plan digest.
pub const WatchPlan: RunawayWatchType = RunawayWatchType(3);

impl RunawayWatchType {
    /// String implements fmt.Stringer interface.
    // go: RunawayWatchType.String L1 [M-NAME]
    pub fn to_string(self) -> &'static str {
        match self {
            WatchExact => "EXACT",
            WatchSimilar => "SIMILAR",
            WatchPlan => "PLAN",
            _ => "NONE",
        }
    }
}

/// RunawayOptionType is the runaway's option type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RunawayOptionType(pub isize);

/// RunawayRule is the `QUERY_LIMIT` rule option.
pub const RunawayRule: RunawayOptionType = RunawayOptionType(0);
/// RunawayAction is the `ACTION` option.
pub const RunawayAction: RunawayOptionType = RunawayOptionType(1);
/// RunawayWatch is the `WATCH` option.
pub const RunawayWatch: RunawayOptionType = RunawayOptionType(2);

impl RunawayActionType {
    /// String implements fmt.Stringer interface.
    // go: RunawayActionType.String L1 [M-NAME]
    pub fn to_string(self) -> &'static str {
        match self {
            RunawayActionDryRun => "DRYRUN",
            RunawayActionCooldown => "COOLDOWN",
            RunawayActionKill => "KILL",
            RunawayActionSwitchGroup => "SWITCH_GROUP",
            _ => "DRYRUN",
        }
    }
}

/// ColumnChoice is the type of the column choice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ColumnChoice(pub u8);

/// DefaultChoice: no column choice given.
pub const DefaultChoice: ColumnChoice = ColumnChoice(0);
/// AllColumns is `ALL COLUMNS`.
pub const AllColumns: ColumnChoice = ColumnChoice(1);
/// PredicateColumns is `PREDICATE COLUMNS`.
pub const PredicateColumns: ColumnChoice = ColumnChoice(2);
/// ColumnList is `COLUMNS c1, c2`.
pub const ColumnList: ColumnChoice = ColumnChoice(3);

impl ColumnChoice {
    /// String implements fmt.Stringer interface.
    // go: ColumnChoice.String L1 [M-NAME]
    pub fn to_string(self) -> &'static str {
        match self {
            AllColumns => "ALL",
            PredicateColumns => "PREDICATE",
            ColumnList => "LIST",
            _ => "DEFAULT",
        }
    }
}

/// LowPriorityValue is the value of `LOW` priority.
pub const LowPriorityValue: u64 = 1;
/// MediumPriorityValue is the value of `MEDIUM` priority.
pub const MediumPriorityValue: u64 = 8;
/// HighPriorityValue is the value of `HIGH` priority.
pub const HighPriorityValue: u64 = 16;

/// PriorityValueToName converts the priority value to corresponding name.
// go: PriorityValueToName L1
pub fn priority_value_to_name(value: u64) -> &'static str {
    match value {
        LowPriorityValue => "LOW",
        MediumPriorityValue => "MEDIUM",
        HighPriorityValue => "HIGH",
        _ => "MEDIUM",
    }
}
