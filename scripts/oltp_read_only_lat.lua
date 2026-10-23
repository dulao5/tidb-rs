#!/usr/bin/env sysbench
-- The stock oltp_read_only workload plus our own per-event latency histogram.
--
-- Homebrew's sysbench 1.0.20 on Apple Silicon reports every latency percentile as 0.00 (its
-- histogram stays empty), so this wrapper times each event (one oltp_read_only "transaction":
-- 10 point selects + 4 range queries by default, the same unit sysbench's own percentile uses)
-- with a monotonic clock via LuaJIT FFI. Each thread writes "<bucket> <count>" lines to
-- <lat_dir>/thread-<tid>.txt when it finishes; scripts/sysbench-run.sh merges them. Bucket b
-- covers latencies up to exp((b + 1) / 50) microseconds (~2% resolution).
--
--   sysbench scripts/oltp_read_only_lat.lua --lat_dir=/tmp/x <oltp_read_only options> run

require("oltp_read_only")

sysbench.cmdline.options.lat_dir =
   {"Directory for per-thread latency histograms (empty = disabled)", ""}

local ffi = require("ffi")
ffi.cdef[[
typedef struct { long tv_sec; long tv_nsec; } ab_timespec;
int clock_gettime(int clk_id, ab_timespec *tp);
]]
-- CLOCK_MONOTONIC is 6 on macOS, 1 on Linux.
local CLOCK_MONOTONIC = ffi.os == "OSX" and 6 or 1
local ts = ffi.new("ab_timespec")

local function now_us()
   ffi.C.clock_gettime(CLOCK_MONOTONIC, ts)
   return tonumber(ts.tv_sec) * 1e6 + tonumber(ts.tv_nsec) / 1e3
end

local hist = {}
local inner_event = event

function event()
   local t0 = now_us()
   inner_event()
   local us = now_us() - t0
   if us < 1 then us = 1 end
   local b = math.floor(math.log(us) * 50)
   hist[b] = (hist[b] or 0) + 1
end

local inner_thread_done = thread_done

function thread_done()
   local dir = sysbench.opt.lat_dir
   if dir ~= nil and dir ~= "" then
      local f = assert(io.open(string.format("%s/thread-%d.txt", dir, sysbench.tid), "w"))
      for b, n in pairs(hist) do
         f:write(string.format("%d %d\n", b, n))
      end
      f:close()
   end
   inner_thread_done()
end
