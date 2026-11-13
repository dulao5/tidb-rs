// lockgen leaves real Percolator locks in a TiKV cluster, with the same client-go as TiDB
// v8.5.8 (c3b5079), for the mixed-cluster lock-resolution check of dulao5/tidb-rs#101
// (step 3). It drives the two-phase commit step by step through client-go's exported test
// probes (tikv.TxnProbe / CommitterProbe), as client-go's own integration tests do.
//
//	lockgen -pd 127.0.0.1:25379 <command> [flags]
//
// Commands (keys and values are hex; every key is a raw TiKV key such as a TiDB row key):
//
//	prewrite   -keys k1,k2,... -values v1,v2,... [-ttl ms] [-async] [-commit-primary]
//	           Prewrite all keys (the first is the primary) and stop. -commit-primary also
//	           commits the primary only, leaving the secondaries locked. -async uses async
//	           commit (the transaction is then committed by whoever recovers it).
//	pessimistic -keys k1,... -hold seconds
//	           A pessimistic transaction that locks the keys and stays open, then rolls back.
//	scanlocks  -start k -end k
//	           Print the locks in [start, end) as "<start_ts> <key hex> <type>", one per line.
//	get        -keys k1,...
//	           Print the committed values at the current timestamp.
package main

import (
	"context"
	"encoding/hex"
	"flag"
	"fmt"
	"math"
	"os"
	"strings"
	"time"

	"github.com/pingcap/kvproto/pkg/kvrpcpb"
	"github.com/pingcap/log"
	"github.com/tikv/client-go/v2/config"
	"github.com/tikv/client-go/v2/kv"
	"github.com/tikv/client-go/v2/oracle"
	"github.com/tikv/client-go/v2/tikv"
	"github.com/tikv/client-go/v2/tikvrpc"
	"github.com/tikv/client-go/v2/txnkv"
)

func must(err error) {
	if err != nil {
		fmt.Fprintln(os.Stderr, "lockgen:", err)
		os.Exit(1)
	}
}

func hexList(s string) [][]byte {
	var out [][]byte
	for _, p := range strings.Split(s, ",") {
		if p == "" {
			continue
		}
		b, err := hex.DecodeString(p)
		must(err)
		out = append(out, b)
	}
	return out
}

func main() {
	pd := flag.String("pd", "127.0.0.1:25379", "PD address")
	if len(os.Args) < 2 {
		fmt.Fprintln(os.Stderr, "usage: lockgen [-pd addr] prewrite|pessimistic|scanlocks|get [flags]")
		os.Exit(2)
	}
	// Global flags first, then the command and its flags.
	args := os.Args[1:]
	var cmd string
	for i, a := range args {
		if !strings.HasPrefix(a, "-") && (i == 0 || !strings.HasPrefix(args[i-1], "-pd")) {
			cmd = a
			must(flag.CommandLine.Parse(args[:i]))
			args = args[i+1:]
			break
		}
	}
	fs := flag.NewFlagSet(cmd, flag.ExitOnError)
	keys := fs.String("keys", "", "hex keys, comma separated")
	values := fs.String("values", "", "hex values, comma separated")
	ttl := fs.Uint64("ttl", 3000, "lock TTL in ms")
	async := fs.Bool("async", false, "async commit")
	commitPrimary := fs.Bool("commit-primary", false, "commit the primary only")
	hold := fs.Int("hold", 30, "seconds to hold pessimistic locks")
	start := fs.String("start", "", "scan start key (hex)")
	end := fs.String("end", "", "scan end key (hex)")
	must(fs.Parse(args))

	// client-go and the PD client log to stdout at INFO; keep stdout for results.
	if lg, props, err := log.InitLogger(&log.Config{Level: "error"}); err == nil {
		log.ReplaceGlobals(lg, props)
	}
	config.UpdateGlobal(func(c *config.Config) { c.TiKVClient.AsyncCommit.SafeWindow = time.Second })
	client, err := txnkv.NewClient([]string{*pd})
	must(err)
	defer client.Close()
	store := tikv.StoreProbe{KVStore: client.KVStore}
	ctx := context.Background()

	switch cmd {
	case "prewrite":
		ks, vs := hexList(*keys), hexList(*values)
		if len(ks) == 0 || len(ks) != len(vs) {
			must(fmt.Errorf("-keys and -values must be non-empty and of equal length"))
		}
		txn, err := store.Begin()
		must(err)
		for i := range ks {
			must(txn.Set(ks[i], vs[i]))
		}
		committer, err := txn.NewCommitter(1)
		must(err)
		committer.SetPrimaryKey(ks[0])
		committer.SetLockTTL(*ttl)
		if *async {
			committer.SetUseAsyncCommit()
			// As TiDB: min_commit_ts from a fresh timestamp, max_commit_ts in the safe window.
			ts, err := store.GetOracle().GetTimestamp(ctx, &oracle.Option{TxnScope: oracle.GlobalTxnScope})
			must(err)
			committer.SetMinCommitTS(ts + 1)
			committer.SetMaxCommitTS(math.MaxUint64)
		}
		must(committer.PrewriteAllMutations(ctx))
		if *commitPrimary {
			commitTS, err := store.GetOracle().GetTimestamp(ctx, &oracle.Option{TxnScope: oracle.GlobalTxnScope})
			must(err)
			// Commit the primary only (a CommitRequest for that key); the secondaries stay
			// locked, as when a committer stops after committing its primary.
			bo := tikv.NewBackoffer(ctx, 20000)
			loc, err := store.GetRegionCache().LocateKey(bo, ks[0])
			must(err)
			req := tikvrpc.NewRequest(tikvrpc.CmdCommit, &kvrpcpb.CommitRequest{
				StartVersion:  txn.StartTS(),
				Keys:          [][]byte{ks[0]},
				CommitVersion: commitTS,
			})
			resp, err := store.SendReq(bo, req, loc.Region, 10*time.Second)
			must(err)
			if regionErr, _ := resp.GetRegionError(); regionErr != nil {
				must(fmt.Errorf("commit primary: region error %v", regionErr))
			}
			if keyErr := resp.Resp.(*kvrpcpb.CommitResponse).GetError(); keyErr != nil {
				must(fmt.Errorf("commit primary: %v", keyErr))
			}
			fmt.Printf("start_ts %d commit_ts %d\n", txn.StartTS(), commitTS)
		} else {
			fmt.Printf("start_ts %d\n", txn.StartTS())
		}
		// Leave the locks: do not commit, roll back or clean up.
		os.Exit(0)
	case "pessimistic":
		ks := hexList(*keys)
		txn, err := store.Begin()
		must(err)
		txn.SetPessimistic(true)
		forUpdateTS, err := store.GetOracle().GetTimestamp(ctx, &oracle.Option{TxnScope: oracle.GlobalTxnScope})
		must(err)
		lockCtx := kv.NewLockCtx(forUpdateTS, kv.LockAlwaysWait, time.Now())
		must(txn.LockKeys(ctx, lockCtx, ks...))
		fmt.Printf("start_ts %d locked %d keys, holding %ds\n", txn.StartTS(), len(ks), *hold)
		time.Sleep(time.Duration(*hold) * time.Second)
		must(txn.Rollback())
	case "scanlocks":
		// A raw ScanLock on the region of -start (the ranges used here fit in one region),
		// so that a shared lock wrapper shows as such: client-go's GC scan expands it.
		ts, err := store.GetOracle().GetTimestamp(ctx, &oracle.Option{TxnScope: oracle.GlobalTxnScope})
		must(err)
		s, err := hex.DecodeString(*start)
		must(err)
		e, err := hex.DecodeString(*end)
		must(err)
		bo := tikv.NewBackoffer(ctx, 20000)
		loc, err := store.GetRegionCache().LocateKey(bo, s)
		must(err)
		req := tikvrpc.NewRequest(tikvrpc.CmdScanLock, &kvrpcpb.ScanLockRequest{
			MaxVersion: ts, StartKey: s, EndKey: e, Limit: 1024,
		})
		resp, err := store.SendReq(bo, req, loc.Region, 10*time.Second)
		must(err)
		if regionErr, _ := resp.GetRegionError(); regionErr != nil {
			must(fmt.Errorf("scan lock: region error %v", regionErr))
		}
		for _, l := range resp.Resp.(*kvrpcpb.ScanLockResponse).GetLocks() {
			if members := l.GetSharedLockInfos(); len(members) > 0 {
				ts := make([]string, 0, len(members))
				for _, m := range members {
					ts = append(ts, fmt.Sprintf("%d:%s", m.GetLockVersion(), m.GetLockType()))
				}
				fmt.Printf("%s %s %s[%s]\n", "shared", hex.EncodeToString(l.GetKey()), l.GetLockType(), strings.Join(ts, ","))
				continue
			}
			fmt.Printf("%d %s %s\n", l.GetLockVersion(), hex.EncodeToString(l.GetKey()), l.GetLockType())
		}
	case "get":
		ks := hexList(*keys)
		txn, err := store.Begin()
		must(err)
		for _, k := range ks {
			v, err := txn.Get(ctx, k)
			if err != nil {
				fmt.Printf("%s error %v\n", hex.EncodeToString(k), err)
				continue
			}
			fmt.Printf("%s %s\n", hex.EncodeToString(k), hex.EncodeToString(v.Value))
		}
	default:
		must(fmt.Errorf("unknown command %q", cmd))
	}
}
