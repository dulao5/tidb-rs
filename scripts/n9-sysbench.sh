#!/usr/bin/env bash
# N9 §6.1 end-to-end: tidb-rs CPU per query under each accounting mode (TIDB_RS_CPU_ACCT / TIDB_RS_CPU_SAMPLE).
set -uo pipefail
BIN=${BIN:-$HOME/work/wt-n9/target/release/tidb-server}
PORT=27001; DUR=${DUR:-20}; ROUNDS=${ROUNDS:-2}
THREADS=${THREADS:-"8 32"}; WLS=${WLS:-"oltp_point_select oltp_read_only"}
CONFIGS=${CONFIGS:-"off:1 instant:1 win10:0 win10:0.01 win10:1 win1:1"}
hz=$(getconf CLK_TCK)
cpu_ticks() { awk '{print $14+$15}' /proc/$1/stat; }
echo "round,config,workload,threads,qps,cpu_us_per_query,cpu_util_cores"
for r in $(seq $ROUNDS); do
 for cfg in $CONFIGS; do
  mode=${cfg%%:*}; rate=${cfg##*:}
  TIDB_RS_CPU_ACCT=$mode TIDB_RS_CPU_SAMPLE=$rate $BIN -P $PORT --pd 127.0.0.1:25379 --auto-tls=false >/tmp/n9-server.log 2>&1 &
  pid=$!
  for i in $(seq 120); do mysql -h127.0.0.1 -P$PORT -uroot -N -e 'select 1' >/dev/null 2>&1 && break; sleep 0.5; done
  for wl in $WLS; do for t in $THREADS; do
    sysbench $wl --mysql-host=127.0.0.1 --mysql-port=$PORT --mysql-user=root --mysql-db=sbtest --tables=1 --table-size=100000 --threads=$t --time=5 --db-ps-mode=auto --skip_trx=on run >/dev/null 2>&1
    c0=$(cpu_ticks $pid)
    out=$(sysbench $wl --mysql-host=127.0.0.1 --mysql-port=$PORT --mysql-user=root --mysql-db=sbtest --tables=1 --table-size=100000 --threads=$t --time=$DUR --db-ps-mode=auto --skip_trx=on run 2>&1)
    c1=$(cpu_ticks $pid)
    q=$(echo "$out" | awk '/queries:/{print $2; exit}')
    qps=$(echo "$out" | awk -F'[()]' '/queries:/{split($2,a," "); print a[1]; exit}')
    awk -v r=$r -v c="$mode@$rate" -v w=$wl -v t=$t -v q=$q -v qps=$qps -v d=$((c1-c0)) -v hz=$hz -v dur=$DUR 'BEGIN{printf "%s,%s,%s,%s,%.0f,%.2f,%.2f\n", r,c,w,t,qps,(d/hz*1e6)/q,(d/hz)/dur}'
  done; done
  kill $pid; wait $pid 2>/dev/null
 done
done
