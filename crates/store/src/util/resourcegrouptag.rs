//! go-file: pkg/util/resourcegrouptag/resource_group_tag.go
//!
//! Port of Go TiDB v8.5.8 `pkg/util/resourcegrouptag/resource_group_tag.go` (block D00.3,
//! 4 functions) for `ResourceGroupTagger` wiring in `tidb-store::copr` (D07.3 spec §2.2,
//! tracking issue `#416` for empty `SqlDigest`/`PlanDigest` until D02.2).

use protobuf::Message;
use tidb_base::Error;
use tidb_base::gofmt::Arg;
use tidb_kv::copr::ResourceGroupTagBuilder;
use tidb_tablecodec::rowindexcodec::{KeyKind, get_key_kind};
use tidb_tablecodec::tablecodec::decode_table_id;
use tikv_client::proto::coprocessor;

/// EncodeResourceGroupTag encodes sql digest and plan digest into resource group tag.
// go: EncodeResourceGroupTag L2 (raw digest bytes until D02.2 parser.Digest)
pub fn encode_resource_group_tag(
    sql_digest: Option<&[u8]>,
    plan_digest: Option<&[u8]>,
    label: tipb::ResourceGroupTagLabel,
) -> Option<Vec<u8>> {
    if sql_digest.is_none() && plan_digest.is_none() {
        return None;
    }
    let mut tag = tipb::ResourceGroupTag::default();
    tag.set_label(label);
    // Go gogoproto `optional int64 table_id = 4 [(gogoproto.nullable) = false]` always writes field 4.
    tag.set_table_id(0);
    if let Some(d) = sql_digest
        && !d.is_empty()
    {
        tag.set_sql_digest(d.to_vec());
    }
    if let Some(d) = plan_digest
        && !d.is_empty()
    {
        tag.set_plan_digest(d.to_vec());
    }
    tag.write_to_bytes().ok()
}

/// DecodeResourceGroupTag decodes a resource group tag and return the sql digest.
// go: DecodeResourceGroupTag L1
pub fn decode_resource_group_tag(data: &[u8]) -> Result<Vec<u8>, Error> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let mut tag = tipb::ResourceGroupTag::default();
    if tag.merge_from_bytes(data).is_err() {
        return Err(tidb_base::errors::errorf(
            "invalid resource group tag data %x",
            &[Arg::Bytes(data.to_vec())],
        ));
    }
    Ok(tag.take_sql_digest())
}

/// GetResourceGroupLabelByKey determines the tipb.ResourceGroupTagLabel of key.
// go: GetResourceGroupLabelByKey L1
pub fn get_resource_group_label_by_key(key: &[u8]) -> tipb::ResourceGroupTagLabel {
    match get_key_kind(key) {
        KeyKind::KeyKindRow => tipb::ResourceGroupTagLabel::ResourceGroupTagLabelRow,
        KeyKind::KeyKindIndex => tipb::ResourceGroupTagLabel::ResourceGroupTagLabelIndex,
        KeyKind::KeyKindUnknown => tipb::ResourceGroupTagLabel::ResourceGroupTagLabelUnknown,
    }
}

/// GetFirstKeyFromRequest gets the first Key of the coprocessor request.
// go: GetFirstKeyFromRequest L2 (coprocessor.Request subset used by copr)
pub fn get_first_key_from_cop_request(req: &coprocessor::Request) -> &[u8] {
    req.ranges.first().map_or(&[][..], |r| r.start.as_slice())
}

/// Encode a `tipb::ResourceGroupTag` for `key` using `builder` (decoding `TableId` and
/// `Label` via `tidb-tablecodec`).
// glue: bridges tidb-kv::ResourceGroupTagBuilder (rank 5) and tidb-tablecodec (rank 6)
pub fn encode_tag_with_key(builder: &ResourceGroupTagBuilder, key: &[u8]) -> Vec<u8> {
    if key.is_empty() {
        return builder.encode_tag_with_key_parts(
            key,
            0,
            tipb::ResourceGroupTagLabel::ResourceGroupTagLabelUnknown,
        );
    }
    let table_id = decode_table_id(key);
    let label = get_resource_group_label_by_key(key);
    builder.encode_tag_with_key_parts(key, table_id, label)
}
