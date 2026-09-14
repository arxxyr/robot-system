# vendor iceoryx2 / iceoryx2-cal 0.9.3 来源说明

> 两个上游运行时 crate 的本地副本，供根 `Cargo.toml` 的 `[patch.crates-io]` 接入。
> 版本不变（仍 0.9.3），只把 source 从 crates.io 换成本地 path；目的是让后续
> RR 严格修正（R1 CAL / R2 IOX）落在单一补丁图上。其余 `iceoryx2-bb-*` /
> `iceoryx2-pal-*` / `iceoryx2-log*` 仍从 crates.io 解析，不升级、不 vendor。

## 原件身份

| 目录 | 原始发布包 | SHA256（.crate，等于 vendor 前根 Cargo.lock checksum） |
|---|---|---|
| `3rd-party/iceoryx2/` | `iceoryx2-0.9.3.crate` | `e226c6041866b416e8c384d69a54a8578cb26619725253351e3e17ea5f32627f` |
| `3rd-party/iceoryx2-cal/` | `iceoryx2-cal-0.9.3.crate` | `bd00991d2e43e9315f555f8ec07bda7fd1a67da3beac9a504b5edaf7de25502f` |

- 来源：本机 cargo registry 缓存 `~/.cargo/registry/cache/index.crates.io-1949cf8c6b5b557f/`
  中的原始 `.crate`（与 `https://static.crates.io/crates/<name>/<name>-0.9.3.crate` 同一文件）。
- 上游 commit：两包 `.cargo_vcs_info.json` 均指 `eclipse-iceoryx/iceoryx2`
  `6ef7c55176a8487ae7ca53c2e0649d5df9a1b3b4`，`path_in_vcs` 分别 `iceoryx2` / `iceoryx2-cal`。
- 提取方式：直接从 tar 流写出全部成员（iceoryx2 93 件、iceoryx2-cal 75 件），逐件
  SHA256 与原包一致；成员全部为普通文件、无链接、无 `..`。不复制 registry 解包目录，
  因此不含本机解包标记 `.cargo-ok`。
- 保留原件：已规范化的 `Cargo.toml`（crates.io 发布形态，依赖为 registry 版本要求）、
  `Cargo.toml.orig`（上游 workspace 继承形态，仅作历史，不用于构建）、`.cargo_vcs_info.json`、
  `BUILD.bazel`、上游 `Cargo.lock`（嵌套锁，根构建不读取）、`README.md`（仅 iceoryx2）。
- 许可证：两包 `license = "MIT OR Apache-2.0"`，逐源文件带 SPDX 头。

## 原包缺失、本仓库补入的文件

原始 `.crate` **不含** `LICENSE-APACHE` / `LICENSE-MIT` / `NOTICE.md`，也不含
`.cargo-checksum.json`。本仓库只补入前三者，取自同一上游 commit
`6ef7c55176a8487ae7ca53c2e0649d5df9a1b3b4` 的仓库根目录（两个 vendor 目录各一份）：

| 文件 | SHA256 | 字节 |
|---|---|---|
| `LICENSE-APACHE` | `62c7a1e35f56406896d7aa7ca52d0cc0d272ac022b5d2796e7d6905db8a3636a` | 9723 |
| `LICENSE-MIT` | `23f18e03dc49df91622fe2a76176497404e46ced8a715d9d2b67a7446571cca3` | 1023 |
| `NOTICE.md` | `4001e4a44dc66b4092318f2cc7d464df72de96e3fa2ac43ac65665e5e67b601c` | 1718 |

`.cargo-checksum.json`：原包不含，本仓库也未生成；原件校验输入以上表 `.crate` SHA256
为准。

## 接入方式与边界

- 根 `Cargo.toml`：`[patch.crates-io]` 仅这两条；`[workspace] exclude` 显式列出两个目录
  （以及预留的 `tools/rr-memory-model`），本方 14 个 members 不变。两包不是 workspace
  成员：`--workspace` / `--all` 门只选本方 crate，vendor 作为库依赖编译。
- 根 `Cargo.lock` 由 Cargo 自动重解析生成，对应两条 `[[package]]` 去掉 `source` / `checksum`，
  其余依赖版本不变。
- 上游 `tests/tests.rs`（两包各一）显式注册为 `[[test]]`，但只 `extern crate`
  未发布、未声明的 `iceoryx2_tests_common` / `iceoryx2_cal_tests_common`，**无法编译运行**。
  原件保留不删；任何“跑过完整上游集成测试”的说法都不成立。
- 实测（2026-09-14，cargo 1.100.0-nightly 3c0b53475）：从根执行
  `cargo test --locked -p iceoryx2 --lib` 退出码 101，
  `error: package `iceoryx2` cannot be tested because it requires dev-dependencies and is not a member of the workspace`；
  `cargo test --locked -p iceoryx2-cal --lib --features std` 退出码 101，
  `error: cannot specify features for packages outside of workspace`。库内既有
  `#[cfg(test)]` 单测依赖 dev-dependency `iceoryx2-bb-testing`（不在根锁中）。后续
  vendor 私有门的运行方式另定。
- 实测同日：两包作为 path 依赖编译时 rustc 不再 `--cap-lints allow`，上游
  `src/port/client.rs:611` 的 `Atomic::<u64>::fetch_update` 在当前 nightly 触发
  `deprecated` 警告（registry 形态下被 cap 隐藏），`CARGO_BUILD_WARNINGS=deny` 的
  test/build 门因此失败。

## 与原包的唯一差异（R0 nightly 兼容补正）

root 于 2026-09-14 批准的唯一源码改动，不静音警告、不改 Ordering / 闭包 / expect / 取模：

| 文件 | 行 | 改动 | 原件 SHA256 → 现 SHA256 |
|---|---|---|---|
| `3rd-party/iceoryx2/src/port/client.rs` | 611 | `Client::next_request_id` 内 `.fetch_update(` → `.try_update(` | `800696371d9f42eb1044a241eca0ae1ace4f64e9574ec967c1e793c05d2fb301` → `1bdfbe6b73452a3e42e55574c37217ddf3be694aea3f1f11ee72a03f0c6303c0` |

依据：nightly 文档中 `fetch_update` 是 `try_update` 的别名（后者 1.95 起可用，前者
1.99 起弃用）；本项目为 nightly 硬要求，不受上游 `rust-version = "1.85"` 约束。
截至 R0 提交（cbed745），除此一处，两目录其余全部成员与原包逐件一致（当时 iceoryx2-cal 无改动）；R1 起的差异见下文“R1 变更”一节。

## R1 变更（2026-09-14）

### iceoryx2：清单一处兼容差异

root `build_warning_decision.md` 批准：从 `3rd-party/iceoryx2/Cargo.toml` 仅删除冗余的
`readme = "README.md"` 一行（Cargo 1.100 对成员清单报 `explicit package.readme can be inferred`，
在 `CARGO_BUILD_WARNINGS=deny` 的嵌套门下会被判错）。删除前后用真实 `cargo metadata` 核对：
`iceoryx2` 的 readme 仍解析到同一文件 `README.md`，根 / 嵌套依赖图与两把锁身份不变。
原发布清单 SHA256 `4fef092c4659862a7c79ce7c72c3414c71551cb926fc1f1db97236a53064903a`（见原件逐件清单）
→ 现 `ae4af353a4789dc583394850da54453c865319860805fbd4c5a63b6a3d462ced`。当前清单不再是 pristine，
原件身份仍以上文 `.crate` SHA256 为准。iceoryx2 的源码差异仍只有 R0 那一处 `try_update`。

### iceoryx2-cal：zero_copy_connection 增量

按已 ACK 的 RR strict r2/r3 与 root R1 实施令，在 `iceoryx2-cal` 的 `src/zero_copy_connection/`
落地 CAL 层增量；不改上游运行时算法之外的任何文件，不改 iceoryx2-bb-* / pal-*，不改共享内存
字段与布局：

| 文件 | 变更 |
|---|---|
| `channel_state.rs`（新增，core-only） | 同源状态转移原语：`AtomicState` trait（load / 强 CAS）与全部 Ordering；`prepare`（CLOSED→合法请求，AcqRel/Acquire）、`prepare_strict`（另限 0..<2^62）、`set_hint`、`has_hint`、`has_state`、`finish`（仅原请求 / 其 hint → CLOSED，Release；遇同请求新 hint 继续一次）、`mark_orphaned`（仅合法 Open/hint → 私有 ORPHANED=MAX−1，AcqRel）、`terminal`、`classify_prepared`（Acquire 三态）。tools/rr-memory-model 通过 `#[path]` 包含同一文件。 |
| `mod.rs` | 为真实 `iceoryx2_bb_concurrency::atomic::AtomicU64` 实现 `AtomicState`；公开 `ZeroCopyPortDetails` 默认方法改为薄兼容层：合法请求参数走同源函数，哨兵参数保持原 Relaxed 行为（见下表）；`is_channel_closed` 同时认 CLOSED / ORPHANED；新增公开 `TrySendReceipt` / `EnqueueIssue`、`ZeroCopySender::try_send_observed`、跨 crate 薄入口 `strict_prepare` / `strict_channel_status` 与 `StrictChannelStatus` / `StrictPrepareError`。 |
| `common.rs` | `try_send` 与 `try_send_observed` 共用唯一真实 push 内核 `push_offset`；publication 之后始终 `Enqueued`（旧 used 移除失败为 `issue`，不回滚新引用、不冒回收）；`try_send` 投影原 Result 与原日志。`blocking_send` 满队列等待改为识别两个终止值（Acquire），断链错误仍优先。`remove_port` 改为逐 channel 同源 `mark_orphaned` 真实 CAS 后才原 `cleanup_shared_memory`；`cfg(test)` 才有的钩子停在 CAS 后 / 端点位移除前，生产无此停顿。`cfg(test)` 才有的 `test_only_forget_used_offset` 只真实移除 used 登记。 |
| `used_chunk_list.rs` | 仅 `cfg(test)` 的只读查询 `contains`（活借用期间核 used 登记，不用破坏性 `remove_all`）。 |
| `tests.rs`（新增，`cfg(test)`） | 私有具名测试：同源原语单测、公开哨兵兼容边缘、真实回执 / used / borrow 所有权、活背压与三种分类、端点移除后断链优先、ORPHANED 窗口内阻塞发送有界返回、两种清理胜者、strict 桥接入口；对 process_local 与 posix_shared_memory 两后端各跑一遍。 |

公开 API 行为差异（合法请求参数改走同源函数；哨兵参数保持原行为）：

| 公开方法 | 合法请求参数（0..=2^62） | `CHANNEL_STATE_CLOSED` 参数（原行为保持） |
|---|---|---|
| `set_channel_state` | 同源 `prepare`：成功条件不变（当前须为 CLOSED），Ordering 由 Relaxed 提升为 AcqRel/Acquire | CLOSED 上仍是成功的 no-op；不是 strict prepare 资格 |
| `set_disconnect_hint` | 同源 `set_hint`：只匹配原请求，不能从 ORPHANED / CLOSED 转换 | 不改变值 |
| `has_disconnect_hint` | 同源 `has_hint`（Acquire） | CLOSED 上为真 |
| `has_channel_state` | 同源 `has_state`（Acquire，含 hint 形态） | 恒为假 |
| `close_channel` | 同源 `finish`（Release；失败重分类，不改其它代次 / ORPHANED） | 不改变其它值 |
| `is_channel_closed` | **行为变化**：CLOSED 与 ORPHANED 都为真（原来只认 CLOSED）；strict 三态另见 `strict_channel_status` | 同左 |

`strict_channel_status` 只是 CAL 观测原语：同值 CLOSED 分不清初始 / 复用代次，`Closed` 只对持有
本次成功 `strict_prepare` 记录、原连接 route / pin 且 channel 未复用的调用方成立；资格、代次与映射
责任在 IOX（R2）。上游 `tests/tests.rs`、附带 `Cargo.lock`、`Cargo.toml.orig` 等原件不动。

私有测试入口：`3rd-party/Cargo.toml` 虚拟 workspace（两个 vendor 目录为成员，`[patch.crates-io]`
同指两目录，根 workspace 继续 exclude），独立 `3rd-party/Cargo.lock` 以根锁为初始输入由 Cargo 生成，
与根锁只差上游 dev-dependencies 带来的仅测试包；目标目录 `target/rr-vendor-tests`。运行命令见该
manifest 注释。上游 `[[test]] tests/tests.rs` 引用未发布的 `*_tests_common`，不在选择范围。

## R2 增量（2026-09-14，IOX 严格请求 / 原连接保留 / 退出所有权 / 受检构造 / 真实故障夹具）

基线：R1 提交 0513a8c；原件身份仍以上文 `.crate` SHA256 为准。本节只列 R2 相对 R1 的差异与理由；
差异明细见 `git diff 0513a8c -- 3rd-party`，证据在 `target/post_e_h2_rt/s48/rr_r2/`。

### iceoryx2-cal

| 文件 | 变更 |
|---|---|
| `Cargo.toml` | 新增默认关闭 feature `r2-test-fixtures`（root `real_fault_fixture_decision.md`）；不加依赖、不改锁。 |
| `zero_copy_connection/mod.rs` | `ZeroCopyCreationError` 新增 `CapacityOverflow`；re-export `checked_connection_management_size`；`ZeroCopySender` 在 feature `r2-test-fixtures` 下新增 `r2_fixture_forget_used_offset` / `r2_fixture_is_used` / `r2_fixture_arm_one_overflow_attempt` / `r2_fixture_armed` / `r2_fixture_hits` 与 `FixtureArmError`。 |
| `zero_copy_connection/common.rs` | 新增 `checked_connection_management_size`（受检镜像原 `SharedManagementData::const_memory_size`：builder 同样的 clamp、completion = buffer + borrow + 1、每个内部数组先 `Layout::array`、总量 ≤ isize::MAX；私有 `Channel` / `SegmentDetails` 布局单源），真实 `Builder::create_or_open_shm` 改用它并在创建任何共享资源前以 `CapacityOverflow` typed 拒绝；三处未受检 `const_memory_size` 删除。`Sender` 在 feature 下多一份进程本地原子 `R2Fixture`（三态 CAS 所有权：独占安装 → Release 发布 → 先 CAS 独占再匹配消费或读取快照，避免重装目标的 ABA）：`push_offset` 只对匹配的一次性目标放过“非 overflow 且队满”的 publication 前拒绝，之后仍走同一 push / 真实挤出 / 真实旧 used 移除分类；夹具的 forget / is_used / arm 目标先受检（channel / segment 范围、sample_size 已知、offset 对齐、used 索引在容量内），非法目标 typed 拒绝（`FixtureTargetError`）。R1 的 `cfg(test)` `test_only_*` 语义不变。 |
| `zero_copy_connection/used_chunk_list.rs` | `contains` 改为 `cfg(any(test, feature = "r2-test-fixtures"))`。 |
| `dynamic_storage/process_local.rs` | `StorageDetails::checked_layout`（`size_of::<T>() + supplementary` 受检并 `Layout::from_size_align`），`Builder::create_impl` 失败映射现有 `DynamicStorageCreateError::InternalError`，已验证 Layout 按值交 `new`；不再 `from_size_align_unchecked`。 |

### iceoryx2

| 文件 | 变更 |
|---|---|
| `Cargo.toml` | 新增默认关闭 feature `r2-test-fixtures = ["iceoryx2-cal/r2-test-fixtures"]`。 |
| `lib.rs` | `pub mod strict;`。 |
| `strict/{mod,client,server}.rs`（新增） | 受检成对入口 `StrictClient` / `StrictServer`（工厂 `create_single_server_strict` / `create_single_reply_strict`）：真核 `max_servers == 1`、Static、DiscardData、无自定义 backpressure handler、请求不 overflow（两端相同配置域）；strict 请求序号独立计数 `1..=2^62−1` 不回绕；同一 shared 锁内 维护 → update → active 上限 → 唯一 Static server 精确双向匹配 → 预留固定槽并真实 pin 原 route（不可 Clone 的 `RoutePin` 原件）→ CAL `strict_prepare` → 唯一 `deliver_one_observed`；publication 前拒绝按原件撤销预留并完整返回原 request，发布后只做不可失败提交，原 pin 按值交 `StrictPendingResponse`（私有持 post-push issue；Drop 转 Detached 只发原 hint）；`StrictServer::receive` 核 `origin == header.client_id`，`StrictActiveRequest` 按值进入唯一回复 loan（`StrictActiveCore` 持 Drop/finish），先建 loan RAII 再执行用户 header `Default`，`assume_init` 按字段转移不整对象 transmute，`write_from_slice` 长度不等即 panic 由 RAII 归还，`send` 同锁先按完整原 client id resolve 再唯一投递。 |
| `strict/tests.rs`、`strict/fixture_tests.rs`（新增，cfg(test)） | 主集覆盖实际不同 N、原 route/key、完整身份、Default/initializer 展开及退出顺序；默认关闭的 `r2-test-fixtures` 子模块含真实 post-push issue 和 completion 满队列的两种释放顺序。仅测试 getter/字段后置 DropMarker/原 unregister 账不进入默认生产布局或回收判定。 |
| `port/client.rs` | `ClientSharedState` 新增 `strict_slots`（Free / Reserved / Live / Detached，长度 = override 后真实 N，只在 `StrictPinned` 申请）、`retry_update`、`server_list_state` pub(crate)；`Drop` 首句 `dispose_detached_for_client_exit`（按值消费本地原件与原 pin）再注销 handle；`maintain_detached`（先按原 pin 有界 drain 真实回复，再 Closed && 无数据 && borrow 0 才释放）；`Client::new(factory, RetentionPolicy)`：全部派生容量按 override 后真实 N 受检（`required_amount_of_chunks_*` 改 checked、`checked_sample_layout`、`checked_static_segment_plan`、active+expired、本端已知 CAL 管理区）、固定表可失败预留、Standard 不申请新表；`ReceiverDetails.number_of_channels = 1`（请求侧）。 |
| `port/server.rs` | `Server::new` 同样受检；`SharedServerState::force_update_connections` 把每个原 `ClientDetails.number_of_requests` 作为该回复连接的真实 channel 数（原来用全局 worst-case，使缩小 override 的 client 打开连接时 `IncompatibleNumberOfChannels`）；三处 receive 先取 `details.origin` 再核 `origin == header.client_id`；`shared_state` 等改 pub(crate)。 |
| `port/details/sender.rs` | `ReceiverDetails` / `Connection` 增 `number_of_channels`（每原连接真实数，须 ≤ Sender 上界），`retrieve_returned_samples` 按该连接真实数回收；新增 `ExactTargetError`、`resolve_connection`、`exact_target_error`、`deliver_one_observed`（精确目标一次 `try_send_observed`；Enqueued 才登记本地引用，真实挤出旧件才释放）、`strict_channel_status_by_id`。 |
| `port/details/receiver.rs` | `RetentionPolicy{Standard, StrictPinned}`；`Connection.pin_count`；`RoutePin` 原件（只由 `pin_connection` 真实 pin 成功构造，`unpin_connection` 按值消费、受检）；strict 下换代/消失但仍被 pin 或有数据/借用的连接进入 `retained`（原 expired 预算 E，Standard 为 `None`），存储满则 `ConnectionFailure::RetentionCapacityExceeded` 且原 key/index/tag 不变；strict create/update 失败不经 degradation Warn 吞成 Ok；`*_pinned` 系列按原件访问；固定表可失败分配助手。 |
| `port/details/data_segment.rs` | `checked_static_segment_plan`（M、K_max ≤ 2^24−2、G、M+G ≤ isize::MAX；`create_static_segment` 单源复用，溢出 typed `InternalError`）。 |
| `port/details/segment_state.rs`、`raw_sample.rs` | 仅测试引用计数只读查询；`RawSampleMut` 原始指针访问器（按字段转移用）。 |
| `port/{publisher,subscriber}.rs` | `ReceiverDetails.number_of_channels = 1`；`Receiver` 新字段 `retention: Standard, retained: None`；行为不变。 |
| `port/update_connections.rs` | `ConnectionFailure::RetentionCapacityExceeded`。 |
| `active_request.rs`、`response_mut.rs` | 保存 `client_port_id`（来自真实 `ChunkDetails.origin`），finish / hint / is_connected / send 先 `resolve_connection(hint, id)`；缺失时普通 API 沿“无连接 = 0 recipients”。 |
| `service/port_factory/{client,server}.rs` | 新增 strict 入口；`ClientCreateError` / `ServerCreateError` 新增 `CapacityOverflow` / `InsufficientMemory` / `ExceedsMaxSupportedChunks`。 |
| `service/static_config/{request_response,message_type_details}.rs` | chunk 数算式改 checked `Option`；`checked_sample_layout`。 |
| `port/pubsub_r2_tests.rs`（新增，cfg(test)） | 通用 pub/sub 活背压与真子进程死亡清理门。 |

库内 Clippy 单独检查 IOX 主包时，上游 1.85 MSRV 与 R0 已验证的 `try_update`（1.95 起提供）存在事实差异；仅在原 `Client::next_request_id` 上以 `#[clippy::msrv = "1.95"]` 标注，不放宽 lint、不改变原子操作、上游 manifest 或项目 nightly。

普通入口行为差异：普通 `Client` / `Server` / `Publisher` / `Subscriber` 的合法配置结果不变；只有原本溢出 / 回绕 / debug panic 的极端配置改为 typed 创建错误；server 回复连接改按每个 client 的真实 N 建立（修正上游缩小 override 后的连接失败）；Standard 路径不申请 strict 表。

### 私有测试入口（R2）

沿 R1 嵌套 workspace 与库内 Clippy 入口；新增：
`cargo test --locked --manifest-path 3rd-party/Cargo.toml --target-dir target/rr-vendor-tests -p iceoryx2 --lib -- --test-threads=1`
（strict 具名测试 + pubsub 三个父例，覆盖活端行为与死亡清理两组），以及显式故障夹具门
`… -p iceoryx2 --lib --features r2-test-fixtures -- --test-threads=1` 与其库内 Clippy（`cargo rustc --profile test --features r2-test-fixtures`）。
根生产图（`cargo metadata --locked`）中两包均未启用 `r2-test-fixtures`（见 `rr_r2/result.json`）。

## 后续变更约定

之后对 vendor 的任何修改（R2 起）须以本说明中的 `.crate` SHA256 为原件基线、以上两节为 R0 / R1
已知差异，另附 diff 与理由；不回写“原件身份”一节。

## R3 本方迁移所需公共类型重导出

`iceoryx2/src/strict/mod.rs` 重导出已有公共返回类型 `EnqueueIssue` 与
`StrictChannelStatus`，使本方无需新增 CAL 生产直接依赖。类型定义、布局与
严格状态机保持 R2 原件；本阶段验证记录位于 `target/post_e_h2_rt/s48/rr_r3`。

### R3 默认关闭的受限故障桥

`strict/client.rs` 在已有 `r2-test-fixtures` 图内加载新增 `strict/test_support.rs`；
`strict/mod.rs` 仅在该图重导出。桥专用于原 IPC slice client 的 L4/C8/M2/N10，
持一份真实 A loan，在同一请求队列保留 A 与三份真实 Sent 后，让 B 走原 strict
发送；既有 CAL forget/arm 入口只对受检原 A/B 一次启用。无预制回执、共享状态代写
或外部可写 offset。`port/details/{sender,segment_state}.rs` 仅把已有只读测试观测的
编译条件扩至该显式 feature，默认生产图不增加观测口。

`strict/server.rs` 同 feature 下新增 IPC slice 原 Active 的清理桥：按完整原 client ID
取出唯一原响应 sender，按值 abandon 后实际 `remove_sender`，不写状态字、不宣称
进程死亡。桥纯值 snapshot 不延长 owner 生命周期；调用方须先真 join server，再
释放 A/client/Node。A 的未知远端引用只由原端点退出处置，不能当作业务退休。
