# 10 · 验证与测试

本文件维护验证合同和工具入口；当前完成边界、阻塞和下一步只维护在 [S0 HAL 当前状态](s0_hal_status.md)。冻结指标以 [00_design.md](00_design.md) 为准，已登记偏离以 [CHANGELOG.md](CHANGELOG.md) 为准。软件、安装、运行、实际动作和停止分别验收，历史 PASS 不代表当前通过。

## 10.1 指标与适用范围

| 范围 | 验证合同 |
|---|---|
| HardRT | 冻结设计 §4.9 / §10.1：S0 目标 1 kHz、p99 <1.5 ms、1 小时最大延迟 <5 ms；初始化后分配及 `Arc::clone/drop` 均为 0 |
| S1 | 冻结设计 §10.1：目标 100 Hz、p99 <15 ms；policy host 超时不阻塞 coordinator |
| Transport | command topic drop count `=0`；sensor 允许丢旧帧但须计数；warmup 后 `unknown_layout_count=0`。已建立的同环境 p99 基线回退不超过 30% |
| 软件安全 | 冻结设计 §6.7；EStop 持久化、fault、最后安全输出及幂等请求不能因测试简化而绕过 |
| 硬件安全 | S0 死亡后实际 torque disable / zero / brake <500 ms 是 D15 / AC-E25 的独立硬门，仍为 OPEN/DEFERRED；详 [XLeRobot 安全边界](targets/xlerobot.md#24-feetech-sts3215-寄存器序列契约) |
| Schema | old/new reader-writer 双向兼容；deprecated 字段 id 不复用；schema id 唯一且单调；每个 schema 有 SizeTier；`layout_id` 保持 `u64` |

HardRT、SerialRT 与 SoftRT 分别评价，不把 HardRT 指标套为串口实测结果。XLeRobot 的 §10.1 RT 硬门豁免、SerialRT FIFO opt-in 及预算修正已登记于 CHANGELOG；本文件不更改这些决定。

SerialRT 的原测量目标包括 `read_state` p99 <8 ms、`write_command` p99 <4 ms、`safety_stop_fast` <10 ms、`confirm_torque_off` RTT <50 ms、latch 双写 fsync <200 ms。目标值、host write 完成、协议 ACK、真实读回和实际机械状态分开报告；端到端“<1 s 断扭”不能证明单次事务达到 <10 ms。实机串口基线必须绑定 Orin、USB-RS485、配置、电机身份与固件，不用开发机或 mock 结果替代。

`iceoryx2_rt_baseline` 在发布频率 ≥50 Hz 时的旧 p99 <100 µs / p999 <500 µs 判据已因速率错拍伪影撤销（CHANGELOG 的 F7 / PX-59）。工具仍可能按内置旧阈值输出 FAIL；该输出只作为 legacy 诊断，不作为该频率下的调度 gate。新判据未批准前不补写通过。

## 10.2 检查与工具入口

测试构建和运行使用同一个隔离 `CARGO_TARGET_DIR`；不得覆盖服务引用的 `target/release` 或冻结产物。按改动风险选择测试，脚本先用假命令验证路径、参数和退出码，不因文档整理启动硬件。

每次 Rust 提交前依次实际执行：

```sh
cargo fmt --all
CARGO_TARGET_DIR=target/verify-current cargo clippy --all --all-targets -- -D warnings
```

完整 CI 的 feature、平台、release 实件和环境变量以 [.github/workflows/ci.yml](../.github/workflows/ci.yml) 为准，仅推送`v`前缀SemVer标签触发；普通分支及PR不触发。CI 的 fmt、Clippy、测试和构建按既有顺序执行。当前workflow不自动创建Release或上传安装包，标签推送成功也不代表CI通过。macOS 只能证明适用的纯软件路径；Linux 调度、systemd 与 S0 Linux 路径需在现有 Linux 环境验证。

| 任务 | 实际入口 |
|---|---|
| Schema 静态约束与兼容性 | [schema_lint.rs](../crates/robot-msg/src/bin/schema_lint.rs)、[compat.rs](../crates/robot-msg/tests/compat.rs)、[schemas/old](../crates/robot-msg/schemas/old/)；旧 schema 为兼容性夹具 |
| transport 类型、借用、真后端 | [robot-transport/tests](../crates/robot-transport/tests/)；mock-only 保底为 `cargo test -p robot-transport --no-default-features --features mock` |
| RT 分配与生命周期 | [rt_guard.rs](../crates/robot-core/src/rt_guard.rs)、[rt_guard_soak.rs](../crates/robot-s0/tests/rt_guard_soak.rs)；负控应真实触发守卫，ignored 未运行不计通过 |
| 同进程 pub/sub | [hello_pubsub.rs](../crates/robot-tests/src/bin/hello_pubsub.rs) |
| IPC 调度诊断 | [iceoryx2_rt_baseline.rs](../crates/robot-tests/src/bin/iceoryx2_rt_baseline.rs)、[baseline.sh](../scripts/baseline.sh)；保留上节 F7 限制 |
| EStop 与进程恢复 | [clear_estop_e2e.rs](../crates/robot-tests/src/bin/clear_estop_e2e.rs)、[crash_restart_smoke.rs](../crates/robot-tests/src/bin/crash_restart_smoke.rs) |
| systemd 外层 watchdog | [freeze_systemd_smoke.rs](../crates/robot-tests/src/bin/freeze_systemd_smoke.rs)、[sd_watchdog_smoke.rs](../crates/robot-tests/src/bin/sd_watchdog_smoke.rs)；freeze hook 只能出现在测试构建 |
| S1 与网关闭环 | [scripted_backend_smoke.rs](../crates/robot-tests/src/bin/scripted_backend_smoke.rs)、[ik_backend_smoke.rs](../crates/robot-tests/src/bin/ik_backend_smoke.rs)、[pubsub_cleanup_smoke.rs](../crates/robot-tests/src/bin/pubsub_cleanup_smoke.rs)、[robotctl_smoke.rs](../crates/robot-tests/src/bin/robotctl_smoke.rs) |
| 通信故障与持久化反例 | [comm_fault_e2e.rs](../crates/robot-tests/src/bin/comm_fault_e2e.rs)、[comm_fault](../crates/robot-tests/src/comm_fault/)、[final_stop_transport.rs](../crates/robot-tests/tests/final_stop_transport.rs) |
| H2 原件完整性与报告 | [artifact_checks.py](../scripts/artifact_checks.py)、[comm_fault_report.py](../scripts/comm_fault_report.py)、[scripts/tests](../scripts/tests/) |
| 安装、服务验收与回滚 | [systemd 维护说明](../scripts/systemd/README.md)；文件存在、已安装、服务运行和物理验收分别记录 |
| 配置矩阵打包 | [build_package.py](../scripts/build_package.py)、[package-matrix.toml](../configs/package-matrix.toml)；原生目标平台、隔离缓存、包内提交及SHA核对 |

各 smoke 的 feature 守门和 binary 名以 [robot-tests/Cargo.toml](../crates/robot-tests/Cargo.toml) 为准。已发送的 effectful 请求必须取得真实完成或退休结果；不得以取消 future 伪造控制动作已退休。

## 10.3 验证记录与复现

- 记录源码提交、输入清单及 SHA、产物 SHA、工具链、features/profile、实际命令、退出码、配置及校准身份。测试数去重，子集不累加，ignored 与未运行项独立列出。
- 延迟比较使用同一测量边界和可比环境：机器、内核、CPU governor、调度类/优先级、每线程亲和性、iceoryx2 版本及实际时钟。`MONOTONIC` 与 `MONOTONIC_RAW` 不混算；host write、t0、t1、ACK 与物理终点分别留戳。
- 每轮使用新 run 和独占目录，保留原日志、原帧、读回、失败和现场回执。摘要只能引用原件，不能改写 FAIL、UNKNOWN、NOT_OBSERVED 或 NOT_RUN。
- 硬件前先备齐产物、固定解释器、脚本、进程检查与 PLAN_ONLY，再确认当轮现场条件。动作前通知；过期或已消费 GO 不复用，不以历史现场授权为当前事实。
- 停止必须有真实读回、持久化、子进程退出与锁归还证据；“进程退出”或“串口不可达”不能单独证明电机安全。未知结果不自动重试。
- 日志、原帧、性能报告和测试结果留在既有本地或Orin位置，不提交Git，不再维护evidence/perf资料目录。当前状态仅保留结论与边界；发布保留必要manifest、提交身份和校验值，历史流水账通过Git查询。

录制/回放的冻结字段、时钟与互操作要求见设计 §10.3；它们是实现约束，不代表已有 Recorder、MCAP 或 LeRobotDataset 导出能力，本轮不增加实现或排期。

## 10.4 通信保护合同

以下保留现行不变量索引，不是原 Spec 全文。完整路由矩阵、预算栈、状态机出口、marker 协议、AC-1–AC-24 和逐轮修订可按 [固定提交的通信 Spec](https://github.com/arxxyr/robot-system/blob/37b6f26ff8d895ed78f9cd352649985c5d7decee/team-swe/specs/post_e_comm_fault_hardening.md) 与 [Plan](https://github.com/arxxyr/robot-system/blob/37b6f26ff8d895ed78f9cd352649985c5d7decee/team-swe/specs/post_e_comm_fault_hardening.PLAN.md) 追溯；后续已登记修正以 CHANGELOG 为准。删除历史工作文档不撤销这些合同，也不批准任何新设计。

| 约束 | 必须保持的行为 |
|---|---|
| 所有权与 RT | 每总线唯一所有者、SerialRT writer、命令流、控制权及停止域；RT 不做 IPC、堆分配、`Arc::clone/drop`、格式化日志、文件/网络 IO 或 panic |
| 有界事务 | 传输按来源绝对期限和单次调用预算推进，不随重试或轮询续期；急停在真实事务边界抢占；Busy、未提交和在途请求均受原截止约束 |
| 安全优先级 | 保留独立单调 EStop/fault notice，先安全再有界高优先级控制，再普通工作；普通目标沿 latest 合并，不让诊断占满基础控制保留资源 |
| 真实退休 | Disable 抢占 Enable 后按原身份退休；迟到 Enable/Clear 回执不得覆盖新代次保护；通信错误首次按既有路由定级，不以错误计数容忍失步 |
| 故障与读回 | SerialRT EStop 不清粘滞 Fault；失步会话不能凭 stale RX 宣称 `TorqueOffConfirmed`；`AdapterUncertain`、`StopFailed` 与真实断扭确认分别报告 |
| RX 信任 | 不宣称协议具有事务身份或可靠 RX 重同步边界；重启不自动恢复信任。双 marker 一次性显式授权、两阶段严格写、持久屏障及非阻塞独占锁保留；缺失/损坏不是授权 |
| 持久化与恢复 | EStop/fault/最后安全输出保留 fsync 双写；ClearEStop 仅 maintainer+，持久化成功后仍须核当前权限与代次。失败义务、Stuck 与恢复抑制不可因重启或清文件消除 |
| 启动与存活 | required 模式的 READY 要求真实通知送达，NotConfigured 只在 standalone 模式接受；systemd 喂狗依据 worker 的真实推进和 lease，而非线程存在。启动监督预算不冒称覆盖 pre-main、process-wide SIGSTOP 或内核停摆 |
| 观测与资源 | 诊断丢弃可观测，序列和错误分类不伪造；队列与在途有界，退出回收全部已发请求和子进程；测试不得靠清理全局资源通过 |

H2 必须保留真实 t0 在途分类、完整事务边界、原样本数、原 P99 门和 RT 环境门。测试 idle 窗口只证明显式前置条件；不把它称为自然命中率。背景负载保持逐项节拍、容量、总等待与首错门；测量失败后停止新增恢复，已发恢复取得真实结果，最终 GetState 和 Disable 分别留证。测量四组通过但最终 Disable 失败仍是整体 FAIL，后续软件修复不回写历史结果。通信 H2 也不关闭硬件 D15。

## 10.5 调度与硬件安全

SerialRT未配置时使用普通优先级；当前XLeRobot两份总线TOML显式要求CPU4/5、FIFO70和锁页，unit提供相应权限。验证时按实际进程逐TID核调度类、优先级、精确CPU亲和性和稳定线程集合，不能凭配置存在宣称已生效。操作和回滚沿 [systemd维护说明](../scripts/systemd/README.md)，不从历史unit快照恢复保护状态。

D15 与受监督例外的现行边界见 [XLeRobot §2.4](targets/xlerobot.md#24-feetech-sts3215-寄存器序列契约)。systemd 重启、同进程 watchdog、启动 preflight 和软件停止均依赖仍存活的主机/通信链，不能替代 S0 死亡后的独立硬件安全输出。D15 保持 OPEN/DEFERRED，`deployable=false`。
