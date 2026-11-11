//! The SET GLOBAL writer against a live cluster (D05.2 spec §6.6 mixed-cluster test,
//! `scripts/systable-mixed.sh`):
//!
//!     systable_cli <pd> set-global <name> <value>     # replaceGlobalVariablesTableValue + etcd notify
//!     systable_cli <pd> set-tidb <name> <value> <c>   # SetTiDBTableValue (mysql.tidb)
//!     systable_cli <pd> get <table> <name>            # getTableValue
//!     systable_cli <pd> loop <n> <name> <prefix>      # n writes of <prefix><i> (concurrency tests)
//!
//! NO_ETCD=1 skips the etcd notification.

use std::sync::Arc;

use poc_domain::systable_io::{EtcdNotifier, TikvStore, TikvSysTables};
use poc_kv::KvClient;
use tidb_systable::session_io::SysTables;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let pd = args[1].clone();
    let kv = rt.block_on(KvClient::connect(vec![pd.clone()])).unwrap();
    // NO_ETCD=1: no notification (the other instances see the value on their 30 s reload).
    let etcd = if std::env::var("NO_ETCD").is_ok() {
        None
    } else {
        rt.block_on(etcd_client::Client::connect([format!("http://{pd}")], None))
            .ok()
    };
    let store = Arc::new(TikvSysTables::new(kv, rt.handle().clone()));
    let w = SysTables::new(Arc::new(TikvStore(store.clone())));
    if args[2] == "raw" {
        // The index value and the record value of the row (hex), handle masked.
        use tidb_systable::session_io::SysTableStore;
        use tidb_tablecodec::tablecodec as tc;
        let mut txn = TikvStore(store).begin().unwrap();
        let tbl = txn.table_info(&args[3]).unwrap();
        let idx = &tbl.indices.as_ref().unwrap()[0];
        let col = &tbl.columns.as_ref().unwrap()[0];
        let mut vals = vec![tidb_types::datum::new_collation_string_datum(
            args[4].as_bytes().to_vec(),
            col.field_type.get_collate().to_owned(),
        )];
        let loc = tidb_base::gostd::time::utc();
        let (k, _) = tc::gen_index_key(&loc, &tbl, idx, tbl.id, &mut vals, None, None).unwrap();
        let iv = txn.get(&k).unwrap().unwrap();
        let h = tc::decode_handle_in_index_value(&iv).unwrap().unwrap();
        let rv = txn
            .get(&tc::encode_row_key_with_handle(tbl.id, &h))
            .unwrap()
            .unwrap();
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        let hb = h.int_value().to_be_bytes();
        let hh = hex(&[(hb[0] ^ 0x80)][..]) + &hex(&hb[1..]);
        println!("index  {}", hex(&iv).replace(&hh, "<handle>"));
        println!("record {}", hex(&rv));
        return;
    }
    let n = EtcdNotifier {
        etcd,
        rt: rt.handle().clone(),
    };
    let msg = |e: tidb_base::Error| String::from_utf8_lossy(&e.error()).into_owned();
    let r = match args[2].as_str() {
        "set-global" => w
            .replace_global_variables_table_value(&args[3], &args[4], false, &n, &mut || Ok(()))
            .map(|()| String::new()),
        "set-tidb" => w
            .set_tidb_table_value(&args[3], &args[4], &args[5])
            .map(|()| String::new()),
        "get" => w.get_table_value(&args[3], &args[4]),
        "loop" => {
            let count: usize = args[3].parse().unwrap();
            let mut res = Ok(String::new());
            for i in 0..count {
                if let Err(e) = w.replace_global_variables_table_value(
                    &args[4],
                    &format!("{}{i}", args[5]),
                    false,
                    &n,
                    &mut || Ok(()),
                ) {
                    res = Err(e);
                    break;
                }
            }
            res
        }
        c => panic!("unknown command {c}"),
    };
    match r {
        Ok(s) => println!("{s}"),
        Err(e) => {
            eprintln!("error: {}", msg(e));
            std::process::exit(1);
        }
    }
}
