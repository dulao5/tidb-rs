#!/usr/bin/env sysbench
-- Protocol smoke/perf test against tidb-server's stub handler (no tables needed).
-- Each event executes a server-side prepared `SELECT ?` with a random integer and checks that
-- one row comes back (sysbench 1.0 cannot fetch values of prepared-statement results); with
-- --text=on it runs `SELECT 1` through COM_QUERY and checks the value.
--
--   sysbench tools/protocol-smoke/protocol_smoke.lua --mysql-host=127.0.0.1 --mysql-port=4001 \
--     --mysql-user=root --db-ps-mode=auto --threads=8 --time=10 run
--
-- Homebrew's sysbench links MariaDB Connector/C 3.4, which requires TLS (tidb-server's
-- auto-tls provides it).

sysbench.cmdline.options = {
   text = {"Use COM_QUERY 'SELECT 1' instead of a prepared statement", false},
}

function thread_init()
   drv = sysbench.sql.driver()
   con = drv:connect()
   if not sysbench.opt.text then
      stmt = con:prepare("SELECT ?")
      param = stmt:bind_create(sysbench.sql.type.INT)
      stmt:bind_param(param)
   end
end

function event()
   if sysbench.opt.text then
      local rs = con:query("SELECT 1")
      local row = rs:fetch_row()
      assert(tonumber(row[1]) == 1, "unexpected SELECT 1 result")
   else
      local v = sysbench.rand.default(1, 1000000)
      param:set(v)
      local rs = stmt:execute()
      assert(rs.nrows == 1, "expected one row from SELECT ?")
   end
end

function thread_done()
   if stmt then stmt:close() end
   con:disconnect()
end
