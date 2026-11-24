# TiKV 请求的批量发送（BatchCommands）：client-go 怎么做，client-rust fork 怎么对应

状态：2026-11-25，设计说明 + 实现记录（client-rust 分支 `tidb-rs/batch-commands`）。负责人 2026-11-24 批准做这件事，目的是让预处理点查在 8 线程下过性能门禁（[perf-point-select.md](../VS1-vertical-slice/perf-point-select.md) §6 第 1 条）。

放在 D07.1（存储层）下面，因为这是 client-go ↔ client-rust 对照表 [client-go-parity.md](client-go-parity.md) §5 第 1 行（"没有 BatchCommands 流"）的实现。

对照基准：client-go `c3b5079`（TiDB v8.5.8 的 `go.mod` 所用，`v2.0.8-0.20260803075849-c3b50791b9fb`）。路径记法：`C` = client-go 的 `internal/client/`，`R` = client-rust 的 `src/`。

## 1. client-go 怎么做

### 1.1 什么时候走批量

- 条件：配置 `max-batch-size > 0`（默认 128），而且目标是 TiKV（不是 TiFlash）（`C/client.go:309`、`:588`、`:622`）。
- 请求类型：`tikvrpc.Request.ToBatchCommandsRequest` 列出的才走批量（`tikvrpc/tikvrpc.go:639-706`）：Get、Scan、Prewrite、Commit、Cleanup、BatchGet、BatchRollback、ScanLock、ResolveLock、GC、DeleteRange、Raw 的 Get / BatchGet / Put / BatchPut / Delete / BatchDelete / DeleteRange / Scan、**Coprocessor**、PessimisticLock / Rollback、CheckTxnStatus、CheckSecondaryLocks、TxnHeartBeat、Flashback、Flush、BufferBatchGet 等。其他（RawGetKeyTTL、RawCAS、RawBatchScan、RawCoprocessor、UnsafeDestroyRange、流式 coprocessor、MPP、调试接口）走一元 RPC。
- 两条路共用同一组 gRPC 连接（每个 store `grpc-connection-count` 条，默认 4，`C/client.go:315-383`）。

### 1.2 结构

- **每个 store 一个 `batchConn`**（`C/client_batch.go:336-370`）：一个容量为 `max-batch-size` 的 channel，调用方把请求放进去；一个**发送 goroutine**（`batchSendLoop`）。
- **每条连接一个 `batchCommandsClient`**（`:823-863`）：一条长期存在的 `BatchCommands` 双向流，一个**接收 goroutine**（`batchRecvLoop`），一个 `batched` 表（请求 id → 等待者），`sent` 计数。
- **请求 id**：`batchConn` 的 builder 逐个分配（`:197`、`:258`），一个 store 内唯一；响应按 id 找回等待者（`:1069-1091`）。

### 1.3 发送循环（`batchSendLoop`，`:610-677`）

1. 阻塞等第一个请求，然后不阻塞地把 channel 里已有的请求全部取出，最多 `max-batch-size` 个（`fetchAllPendingRequests`，`:389-431`）。同时算出"队头请求的到达间隔"（与上一批最新请求的时间差）。
2. 不满一批时，可能再等一会儿（`fetchMorePendingRequests`，`:433-483`）：
   - TiKV 报告的传输层负载（响应里的 `transport_layer_load`）超过 `overload-threshold`（默认 200）且 `max-batch-wait-time > 0`（默认 0，即关闭）时：最多等 `max-batch-wait-time`，凑够 `batch-wait-size`（默认 8）个；
   - 否则按 `batch-policy`（默认 `standard`）：到达间隔的指数平均小于 80 µs 时，最多等 **100 µs**，凑够"最近批大小的平均值"个（`turboBatchTrigger`，`:488-603`）；`basic` 不等，`positive` 总是等。
   - 等完再不阻塞地取一次，中间让出一次（`runtime.Gosched`）。
3. 按轮询选一条连接（`getClientAndSend`，`:684-748`）：跳过正在重建流的（`tryLock`）和超过 `max-concurrency-request-limit`（默认不限）的；一条都选不到时，所有请求立即以 "no available connections" 失败（`:714-723`）。
4. 给每个请求分配 id、登记到 `batched`，组成一个 `BatchCommandsRequest` 写进流（`:729-744`、`:882-911`）。写失败时这一批立即失败。

### 1.4 接收循环（`batchRecvLoop`，`:1019-1100`）

- 收到一个 `BatchCommandsResponse`：逐个 id 找到等待者，没被取消的交给它，从 `batched` 删除（`:1069-1091`）；记下 `transport_layer_load`（`:1093-1097`）；带 `HealthFeedback` 的交给事件监听者（慢节点评分，`:1058-1067`）。
- 流出错：**先让这条流上所有在途请求立即失败**（`failPendingRequests`，`:1145`），然后等连接就绪（`waitConnReady`，拨号超时 5 s）再重建流，失败就按 `BoTiKVRPC`（100 ms → 2 s，等抖动）退避重试，直到成功（`recreateStreamingClient`，`:1108-1162`）。重建期间发送循环跳过这条连接。
- panic 时记录、计数并重启这个 goroutine（`:1020-1031`）。

### 1.5 调用方、超时与取消（`sendBatchRequest`，`:1209-1282`）

- 一个计时器覆盖"放进 channel"和"等响应"两段，时长是这次 RPC 的超时（与一元 RPC 相同：Get 30 s、BatchGet 60 s，或请求自带的 `max_execution_duration`）。超时返回 `DeadlineExceeded`（"wait sendLoop" / "wait recvLoop timeout"）；上层（`onSendFail`）按发送失败处理，与一元 RPC 超时相同。
- 调用方的 context 取消时，把条目标为已取消：还没发出的不再发，已发出的响应到了直接丢弃。
- 3 分钟没有请求的 `batchConn` 标为空闲，下次使用时整个连接组关闭重建（`:485`、`:1284-1303`）。

### 1.6 监控

`tidb_tikvclient_batch_*`：批大小、排队请求数、发送 / 接收循环各阶段耗时、等待更多请求的次数、过载等待次数、连接不可用时长、选不到连接的次数（`metrics/metrics.go:369-560`）。

## 2. Rust 的对应（client-rust `R/store/batch.rs`）

| client-go | client-rust |
|---|---|
| `batchConn`（每个 store） | `BatchConn`：`KvRpcClient` 持有（`Arc`），与一元 RPC 共用同一组 tonic `Channel` |
| 只对 TiKV 批量（`enableBatch := req.StoreTp == tikvrpc.TiKV`，`C/client.go:586-589`） | 按 store 的 `engine` 标签判断类型（`StoreType::of_store`，同 `tikvrpc.GetStoreTypeByMeta`，`tikvrpc/endpoint.go:81-91`），连接 store 时传入（`KvConnect::connect_store`）；TiFlash / TiFlash compute 的客户端没有 `BatchConn`，发给它们的 coprocessor 等请求一律是一元 RPC。client-go 按请求判断，这里按 store 判断：一个 store 地址只属于一种引擎，结果相同 |
| `batchCommandsCh`（容量 `max-batch-size`） | tokio 有界 `mpsc`，容量相同 |
| 发送 goroutine | 一个 tokio 任务（`SendLoop::run`）；取请求、到达间隔、`turboBatchTrigger`、过载等待、轮询选连接、`buildWithLimit` 逐项对应 |
| 接收 goroutine + 流 | 每条连接一个 tokio 任务（`recv_loop`），持有 tonic 的 `batch_commands` 双向流；请求经一个无界 channel 进入流 |
| `batched`（`sync.Map`） | `Mutex<HashMap<id, oneshot::Sender>>`；一批响应只加一次锁 |
| `entry.canceled` | 调用方放弃等待（超时，或它的 future 被丢弃）时 oneshot 的接收端随之丢弃；发送循环看到 `is_closed()` 就跳过，响应到了直接丢掉。**取消安全**：调用方在任何 `.await` 点被丢弃都不会留下悬挂状态，`batched` 里的项在响应到达或流失败时清掉（与 Go 相同） |
| `tryLock` / `reCreating` | `Mutex<StreamState>`：发送时持锁；重建时置 `recreating`，发送循环跳过 |
| 超时计时器 | `tokio::time::timeout_at`，同一个截止时间覆盖两段；错误是 `Status::deadline_exceeded`，进入现有的重试逻辑（`grpc_error_action`：换副本，不清缓存），与一元 RPC 超时相同 |
| 流失败 | 在途请求立即以流的错误失败（`Status`，保留传输层的 source），重试逻辑按连接级失败处理（换副本并清 store 缓存、查 store 存活），与一元 RPC 遇到同样的连接错误时相同 |
| `batchConn.Close` | 最后一个 `KvRpcClient` 句柄丢弃时（store 缓存失效后），`watch` 发送端随之丢弃：两个任务退出，流关闭，剩下的请求以 "batch client closed" 失败 |
| 配置 | `Config::batch`（`BatchConfig`）：`max_batch_size` 128、`batch_policy` "standard"、`max_batch_wait_time` 0、`batch_wait_size` 8、`overload_threshold` 200、`max_concurrency_request_limit` 不限，名字和默认值同 client-go（`config/client.go:50-51`、`:170-175`、`:194`）。`max_batch_size = 0` 时全部走一元 RPC |
| 走批量的请求 | `Request::batch_cmd` / `batch_response`：client-go 列表中 client-rust 有的类型全部走批量（含 coprocessor）；其余仍是一元 RPC。RPC 耗时指标（`tidb_tikvclient_request_seconds` 等）两条路记法相同 |
| 监控 | `batch_requests`、`batch_pending_requests`、`batch_wait_overload`、`batch_client_unavailable_seconds`、`batch_client_no_available_connection_total`，名字、标签、分桶同 Go |

## 3. 有意不做的

- **请求转发**（`forwardedHost`，`enable-forwarding` 默认关）：client-rust 没有转发。
- **优先级队列**（`PriorityQueue`、`highTaskPriority`）：client-rust 不发资源管控的优先级（见 N10），所有请求都是普通优先级，队列退化为先进先出。
- **HealthFeedback**（慢节点评分）：client-rust 没有副本选择器的慢节点逻辑，收到后忽略。
- **空闲 3 分钟回收连接组**：store 缓存失效时句柄会被丢弃（见上表）；不做按时间的回收，连接一直保留。
- **预编码请求**（`encodeRequestCmd`，在调用方的 goroutine 里序列化）：tonic 在发送流的任务里统一编码。
- **异步接口**（`SendRequestAsync`）、发送 / 接收循环分阶段耗时等其余几个监控项、panic 后重启循环（Rust 任务 panic 时该任务结束；见 §4 第 4 条）。

## 4. 与 client-go 的差异（含 file:line）

1. **100 µs 的等待**（`standard` 策略，`C/client_batch.go:491`、`:650-653`）：tokio 的计时器精度是 1 ms，所以 1 ms 以下的等待改为"查一次队列、让出一次"循环直到截止时间或凑够数量（`R/store/batch.rs` `fetch_more_pending`）。效果相同，等待期间这个任务会占着一个工作线程（Go 的 `time.Timer` 不占）。1 ms 及以上的等待（过载等待）用普通计时器。
   "让出一次"（Go 的 `runtime.Gosched`，`:479`）用任务自己唤醒自己、返回一次 Pending 实现（`yield_once`），不用 `tokio::task::yield_now`：后者把唤醒推迟到工作线程轮询完 I/O 驱动之后，每批多一次系统调用和一段延迟。实测用 `yield_now` 时 `standard` 策略在 8 线程下比不等待的 `basic` 慢约 6%，改成 `yield_once` 后差距约 4%（§7）。实际负载下 Go 的平均批大小约 1.4，"凑够平均批大小"的条件通常已经满足，`standard` 的主要开销就是这一次让出。
2. **重建流前的"等连接就绪"**（`waitConnReady`，`:966-994`）：tonic 的 `Channel` 不暴露连接状态，断开后在下一次调用时自动重连。所以第一次重建立即进行；重建出的流在收到任何响应之前又失败时，才按 `BoTiKVRPC` 退避（`R/store/batch.rs` `recv_loop`）。退避期间这条连接不可用，与 Go 相同。
3. **流的建立是惰性的**：grpc-go 的 `NewStream` 在连接就绪时立即返回；tonic 的双向流在第一次发送时才真正发出请求头，建立失败表现为这条流上第一批请求的失败（随后按第 2 条重建）。
4. **panic**：Go 的发送 / 接收循环 recover 后重启自己（`:611-621`、`:1020-1031`）；Rust 的任务 panic 会结束该任务：发送循环结束后这个 store 的请求以 "batchConn closed" 失败，直到 store 缓存失效、句柄重建。两个循环里没有会 panic 的业务代码（只有锁中毒）。
5. **leftover 的发送顺序**：没有优先级，同一批里按到达顺序（Go 的堆不保证顺序）。

## 5. 待负责人决定的事

没有挡住实现的决定。实现完全按 client-go 的默认值（批量开启、`standard` 策略），门禁已经通过（§7.1）。可选的一项：

1. **默认批量策略**。本机 8 线程下 `basic`（不等待；v8.3.0 之前 client-go 的行为）比 `standard` 再快约 4%（§7.2），原因是 §4 第 1 条的那一次让出。建议保持 client-go 的 `standard`；如果以后要改，只改 tidb-rs 传给 client-rust 的配置，不改 client-rust 的默认值。

## 6. 测试和测量

- client-rust 单测（`R/store/batch.rs`，对一个实现了 `BatchCommands` 的假 TiKV）：并发请求被合批且各自拿到自己的响应（响应乱序返回）；超时（"wait recvLoop timeout"）和调用方放弃等待后流仍可用；流失败时在途请求立即失败、流被重建；不走批量的类型仍是一元 RPC；TiFlash / TiFlash compute store 没有批量客户端，coprocessor 走一元 RPC，store 的引擎标签决定连接方式；超过 4 MiB 的响应；丢弃客户端后流关闭；多线程、多连接下的大量调用；`buildWithLimit` 和 `turboBatchTrigger` 与 Go 的公式一致。原有的一元 RPC 测试改为 `max_batch_size = 0`。
- 性能和端到端对比：见 §7。

## 7. 测量结果（2026-11-25，本机 tiup playground v8.5.8，方法见 [perf-point-select.md](../VS1-vertical-slice/perf-point-select.md) §2）

### 7.1 预处理点查（`tools/perf/point_select_gate.py`）

client-rust `186d194`（本分支最终提交）。"改之前"是同一份 tidb-rs 源码、只把 client-rust 换回 main 固定的 `4d81d4e`，在同一时段测。

| 指标（新栈 / Go，Go 实例级 plan cache ON） | 改之前（3 轮） | 改之后（5 轮） | 门槛 |
|---|--:|--:|--:|
| 吞吐，1 线程 | 0.972 [0.949, 0.994]：FAIL | **1.076** [1.024, 1.132]：PASS | ≥ 1.0 |
| 吞吐，8 线程 | 0.970 [0.959, 0.980]：FAIL | **1.061** [1.052, 1.070]：PASS | ≥ 1.0 |
| p50，1 / 8 线程 | 1.030 / 1.036：FAIL | 0.946 / 0.946：PASS | ≤ 1.0 |
| p99，1 / 8 线程 | 0.827 / 0.995 | 0.680 / 0.900：PASS | ≤ 1.0 |
| 每条语句的分配次数 | 335.0（Go 223.5）：FAIL | 302.0（Go 224.4）：FAIL | ≤ 1.0 |

新栈 QPS：1 线程 9181 → 9929，8 线程 31522 → 34526；Go 同期 9227–9449 / 32509–32542。Go 默认配置（会话级 plan cache，3 轮）：1 线程 1.039、8 线程 1.056，p50 / p99 都通过。分配次数少了 33 次（一元调用的请求头、请求体、流状态），仍是 Go 的 1.35 倍，原因见 perf-point-select.md §6 第 2–4 条，不属于本项。

**结论：预处理点查的吞吐和延迟在 1 线程、8 线程下都过门禁；分配次数仍不过。**

### 7.2 批量策略的对比（8 线程，各 3 轮，同一时段）

| 策略 | 新栈 QPS | 新栈 / Go |
|---|--:|--:|
| 一元 RPC（`max_batch_size = 0`） | 31672 | 1.034 |
| `standard`，让出用 `tokio::task::yield_now` | 31455 | 1.030 |
| `standard`，让出用 `yield_once`（最终实现，client-go 默认） | 32891 | 1.069 |
| `basic`（不等待） | 33370 / 34208 | 1.067 / 1.132 |
| `positive`（总是等 100 µs） | 26911 | 0.880 |

这几次运行之间 Go 一侧波动约 ±3%，只能比较同一行里的新栈 QPS。`basic` 比 `standard` 再快约 4%，但默认值按 client-go（`standard`），不改（§5）。

### 7.3 端到端对比

最终提交（tidb-rs 本分支 + client-rust `186d194`）与只换回旧 client-rust 的同一源码，分别跑 `tools/weekly/run.sh ab`（`RS_STATUS_PORT=10581`），prototype 模式和 `--new-stack=fallback` 两次，每次都带 strict 一轮：

| 运行 | 两边逐条结果 |
|---|---|
| fallback：sysbench / constants / sample / sql-layer | 新、旧完全相同：match 1360 / 34 / 8 / 94，mismatch 12 / 9 / 0 / 0（都是 main 上已有的） |
| strict：同上四组 | 新、旧完全相同：match 799 / 14 / 4 / 46，mismatch 4 / 9 / 0 / 0 |
| prototype 模式（默认 `off`） | 新、旧完全相同 |
| 锁解析（`run.sh locks`：CheckTxnStatus、ResolveLock、CheckSecondaryLocks、ScanLock 都走批量流） | 18 个用例，新、旧都是 18 个与 Go 结果相同、15 个符合预期，逐项相同 |

没有 panic、断开的连接或新的 1105；strict 一轮的路由计数只差 3 条：旧版本有 3 条语句因 schema 版本还没追上而交给 prototype（`schema_version`），新版本这 3 条走了新栈，属于时序差别。

**写路径**：tidb-rs 服务端没有 SQL 写路径（只读节点；`SET GLOBAL` 返回 1235，DML 不在新栈上执行），所以 ab 不覆盖写。tidb-rs 中唯一通过 client-rust 写 TiKV 的代码是 `SET GLOBAL` 的写入器（`crates/poc/domain/src/systable_io.rs`，乐观事务的 prewrite + commit），服务端还没有接上它。用它的命令行（`systable_cli <pd> loop 200 ...`）对 perf 集群单独测了：串行 200 次写入新、旧版本都全部成功，Go 读到最后的值；两个进程并发写同一行时新、旧版本相同，都是一方遇到 WriteConflict 后退出（命令行不重试）。悲观锁（`SELECT ... FOR UPDATE`）tidb-rs 不执行。

## 所用 ID

- D07.1：存储层（client-rust 之上的 kv / store 接口）；N10：资源管控（RU）的例外设计。
