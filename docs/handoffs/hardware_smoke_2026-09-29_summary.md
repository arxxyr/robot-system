# 2026-09-29 XLeRobot 硬件检查

本轮用户要求测试机械臂、头部、底盘，确认持续看护、可立即切断电机电源、双臂空载且有支撑、活动区清空；底盘在地面且周围有空地。随后用户明确确认“两个电源和 usb 都已经重新插拔”。

## 当前结果

**左右腕部各一次小幅往返功能探针 PASS；头部、底盘完成通信、静止状态检查及启动防护安全写，主动动作测试未完成。写指令路径仍有未解释的诊断告警。**

2026-09-29 19:43:04–19:43:36 UTC，在 Orin `ubuntu / loosqk / aarch64` 上执行 `run_id=20260929-01`。两侧串行执行，各消费一次 RX 授权、一次 ClearEStop、一次 `single-arm`，没有自动重试。

| 模块 | 总线 / 电机 ID | 本次动作覆盖 | 结果 |
| --- | --- | --- | --- |
| 左臂及夹爪 | `5A7C121427` / 1–6 | 六电机上扭保持，仅 wrist_roll 发往返轨迹 | PASS，实测最大出程 0.0383 rad，约 2.19° |
| 右臂及夹爪 | `5A7C122340` / 1–6 | 六电机上扭保持，仅 wrist_roll 发往返轨迹 | PASS，实测最大出程 0.0414 rad，约 2.37° |
| 头部 | `5A7C121427` / 7–8 | 通信和状态检查；guard 启动时发送断扭安全指令 | 在线正常，主动动作未测试 |
| 底盘 | `5A7C122340` / 7–9 | 通信和状态检查；guard 启动时发送零速及断扭安全指令 | 在线正常，主动动作未测试 |

两腕轨迹均以实测位置为锚点，目标偏移 +0.05 rad（2.86°），出程和回程各 0.5 秒。冻结 `supervised_demo` 客户端要求时间戳推进的反馈、最大出程至少 0.03 rad、回程距锚点不超过 0.02 rad，且任务 `Completed(Success)`、Disable 确认后才输出 PASS。本次保存了客户端判定与最大出程日志，未保存其内部完整逐帧 witness，不据此推导精确回程误差或轨迹跟踪性能。未做其他关节独立动作、夹爪开合、双臂同步或长时间测试。

右肘测试前 raw=2995、标定最大值 2991，复用了现有 adapter 的窄范围 clamp；本轮只允许该关节最多 4 tick 越界，其余关节没有放宽。结束后 raw 仍为 2995，因此本结果不证明肘部标定问题解决，也不证明达到夹限目标。

日志共 68 条结构化 WARN，每侧 34 条。其中每侧 17 条 `site=5 / kind=0 / a=63 / b=0` 来自 `FeetechAdapter::write_command` 的写路径错误记录，63 为未细分原始错误的 Unknown；当前日志不足以恢复原始 `FeetechError / AbortReason`。两侧周期摘要均 `dropped=0 / comm_level=0 / rx_trust=true`，没有进入故障或失去信任状态；授权撤销或中止可能解释这些记录，但尚未证实。保留功能探针与收尾的有限 PASS，不能宣称零错误或通信问题已解决；后续应先补齐原始错误分类证据，再决定是否重测。

## 结束状态

- 两侧均先确认 Disable，再通过既有 CLI 的 **iceoryx2 EStopEvent** 恢复锁存；不是把事件发布结果当成 RPC Ack。随后持续读取新鲜状态，确认 `Safe / TorqueOffConfirmed`，并确认 EStop 主备文件均为 `1\n`。
- 两侧结束后分别读回整条总线：共 **17 个电机 Torque_Enable=0、Present_Velocity=0**。机械臂和头部模式均为 0；底盘三轮模式均为 1。
- 六个临时 `Type=notify / Restart=no` 服务均退出成功，`systemd-run --wait` 六个等待进程均回收且退出码为 0；串口及相关控制进程互斥检查通过。S0 FIFO70、左 CPU4 / 右 CPU5 的启动条件已核对。
- 19:44:00 UTC 额外只读复查：六个正式服务均 inactive/dead、MainPID=0；六个临时 unit 均 not-found/inactive/dead；两侧 EStop latch、RX trust marker 主备均为 `1\n`。一次性授权已消费，禁止复用本次 GO 或输出目录重跑。
- 正式 `target/release` 四件 SHA 与测试前一致，未启动正式控制服务、未覆盖正式二进制、未修改正式配置或 EEPROM。运行的是既有 `ca14f0e` 冻结候选四件与单独构建的客户端，不代表当前本地 `c6e8b2d` 开发提交已完成真机动作回归。

## 头部和底盘的未完成项

**头部不能只换成历史 full 配置后启动。** 现有 D2 身份、guard 和 ClearEStop 恢复事务绑定 arm-only 安全域，头部动作入口需作为独立能力扩展接入。[D2 范围](../../team-swe/specs/v0_5_dimos_replace.md)明确未包含头部/底盘驱动。

**底盘真实速度执行链尚未实现。** 当前 Feetech adapter 忽略 Velocity/Stop 命令，上扭和失联策略仍采用位置语义。后续需要速度模式校验、零速确认后上扭、实际速度写入及超时/取消/Disable 收尾。当前三轮已经是模式 1，后续可设计不修改 EEPROM 的受监督测试；本轮 guard 只发送零速和断扭安全写，没有下发底盘运动指令，也没有运行旧上游底盘脚本。头部同样接收过 guard 断扭指令，因此本次整轮不能标为全程只读。

本轮不改变右臂 H2 的历史整体 FAIL，不关闭 D15，不构成全部硬件动作验收、生产替换或无人值守验证。下一步硬件动作覆盖需要先补齐头部/底盘的安全执行能力。

## 产物和前置验证

在 Orin 干净的 `ca14f0e6cf6df8ecc6baaaa8dff2dddb64abbd0a` 源码上，以 `nightly-2026-09-10` 和独立 `CARGO_TARGET_DIR=target/rr-r4db-arm/root` 构建客户端，未覆盖正式 `target/release`：

| 工具 | SHA-256 |
| --- | --- |
| robotctl | `704b25479560c503e3949c0621c79ad708186bffefd9a3cdf258ea666cad7285` |
| supervised_demo | `5cd987dc97265326a28aa5f34b3f02f563536704c99e7c49736dee56c3e21cf9` |
| 只读 feetech_poke（S49 快照，dev 构建） | `c4f83e7928a8d1aa8b134708688fdb49b6040eac25520463067029e61b6c7ef2` |

实际 S0/deploy/S1/gateway 来自 `target/post_e_h2_rt/s48/h2_arm_native/native_ca14f0e_20260923T082518Z/production`。四件及 `production.json` sidecar 已与既有 H 归档核对；完整路径和 SHA 固定在执行报告的 plan。正式路径四件仍是 9 月 8 日 C5 恢复产物，不能按文件名推断包含 S48 修复。

Orin `supervised_demo` release 单元测试 **36 通过**；最终监督脚本 **17 项纯 mock 测试在本机和 Orin 均通过**。早期 15 项日志保留，新增测试编写中曾出现 NameError，修复后最终 17 项通过。这些是软件验证，本次硬件结论另以真实运行记录为据。

现场确认后重新核对 USB：左序列号 `5A7C121427`、devnum 19；右 `5A7C122340`、devnum 20，boot ID `6cf7ee3e-a3c5-4c91-80bc-4ca924818b53`。GO 绑定这些身份、新鲜现场事实和 plan SHA `279b720cca9a4ffa893553711892c362766930f1035e5b90ab057652be19cbcb`，随后仅执行一次。

## 证据

- [真实动作机器摘要](../../.verify/hardware_smoke_2026-09-29/run_01_summary.json) 与 [真实动作证据包](../../.verify/hardware_smoke_2026-09-29/run_01_evidence.tar.gz)：含 plan/GO、运行前检查、268 步执行及六个服务日志、结束后独立快照、脚本与审查、逐文件 SHA 索引。
- 原始 `run_01/run.json` 的 Orin 与本地 SHA 均为 `483da756a459c3864da6e250da452e76fd740ddbcaa53c9af235b0f2acec08ab`。Orin 原件在 `/home/loosqk/repo/robot/robot-system/target/hardware_smoke_20260929/run_01/run.json`。
- [最初只读阶段摘要](../../.verify/hardware_smoke_2026-09-29/summary.json) 与 [只读阶段证据包](../../.verify/hardware_smoke_2026-09-29/evidence.tar.gz) 原样保留。该阶段为 19:12:18–19:12:19 UTC、17 电机的 102 项寄存器检查；其中 `motion_started=false / hardware_ready=false / awaitphysical` 只描述当时阶段，不是后续真实动作结果。证据包 SHA 保持 `ce5fdf02cc44f4bb47ca187972f8f1bc6b3e03433d653747ca20be068a880900`。

本地明细在 `target/hardware_smoke_20260929/`。没有新的硬件测试在后台运行。

## 同日后续排查：写指令诊断分类

用户继续要求“排查”，并明确要求“代码中明确分类”。已对原始 `ca14f0e` 产物对应代码和 `run_01` 日志独立核对：每侧前 16 条分别集中于 16,768 ns / 16,032 ns 内；最后一条分别在 OperatorDisable 物理门完成前 0.513344 ms / 0.043072 ms。告警不是已证实的 17 次独立串口超时。

已确认的缺陷是 `record_error` 把 `Aborted(Emergency/Timeout/Revoked)` 等非通信错误统一压成 Unknown，控制返回链则仍保留具体中止原因。代码确有 ReadyHold 先于 Motion 授权、Disable 撤销权限先于输出选择更新的短暂窗口，最终租约门可以在 transport 写之前拒绝调用；固定累加 deadline 的周期循环又可以快速补赶过期 tick。这与“前 16 条 + 下扭附近 1 条”高度吻合，但本次未归档 Enable DEBUG 提交时刻、每条原始 AbortReason 或 tick 序号，不能逐条确认为 Revoked，不能给旧日志补造原因。

修复采用明确的诊断枚举，穷尽匹配所有 `FeetechError` 变体，再统一映射固定编码：通信、设备状态、事务中止、参数、目标读回不一致及重复连接分别分类；参数进一步区分输入、数值编码、标定、标定文件和型号。新增错误变体必须显式分类。完整编码见 [日志规范](../logging.md#feetech-固定诊断事件的中止原因)，WARN、计数、16 字节事件、原始错误返回与控制安全语义不变。既有 kind6 使能失败附加摘要可能仍保留 Unknown，随后详细错误记录可区分原因，不宣称源码所有 Unknown 均被删除。

本机 fmt、feetech 全目标/全 feature Clippy r5、147 项库测试和 5 项独立集成测试通过。
测试覆盖真实 adapter 与权限租约的拒绝零写、许可后发送、撤销后拒绝，以及通信/设备/参数
反例和固定诊断路径无分配。初轮风格 lint、测试收集器首次 Mutex 惰性分配触发的 SIGABRT
原件保留；后者换固定原子槽后通过，未更改生产分配器。独立源码及本机证据审查通过。
本轮 Orin 回归尚未开始：本机 known_hosts 缺少跳板机及 Orin 条目，严格 SSH 拒绝连接，
已向用户询问信任记录是否被清理，未向 Orin 传输或正式安装。

随后在隔离的 Lima Ubuntu 24.04 ARM 虚拟机、nightly-2026-09-29 完成软件门：
实际 `cargo fmt --all` 后根 workspace Clippy 零警告；Feetech `real-hw` 测试二进制
分别 147、5、12、3 项通过，SerialRT convergence/rt_guard 42 项通过。
605 项编译输入前后 SHA 一致，清单为 `d34cb21d91acfe00505b979d47f12b991cd3b0eb485dea70808c18238a153b5e`。
首轮 CLI lint、第二轮格式错误、第三轮两处 Linux S0 lint 失败原件均保留；
三处 `map/unwrap_or_default` 等价改为 `map_or_else` 后第四轮通过，未改变控制逻辑。
该快照只覆盖诊断修复及等价 lint，不包含正在开发的 Rerun/transport 镜像。
本地 ARM 软件验证不等于 Orin 或硬件回归，本次排查未发送硬件指令。

本轮软件验证见[诊断分类摘要](../../.verify/hardware_smoke_2026-09-29/diagnosis_summary.json)
与[本机诊断证据包](../../.verify/hardware_smoke_2026-09-29/diagnosis_local_evidence.tar.gz)，
包含最终源码、测试日志、初次失败、独立审查和 SSH 拒绝记录；不覆盖前述硬件原件。
Linux 四轮日志、真实 wait 回执、来源清单及独审另见
[原生软件证据包](../../.verify/hardware_smoke_2026-09-29/diagnosis_linux_evidence.tar.gz)。

单独保留 OPEN：周期写遇到 Revoked 后会跳过该 tick 的状态读取，若输出选择与权限长期不一致，可能影响读侧进度；本次日志中动作已完成、读数恢复，不能据此证明持续饥饿，也不能宣称诊断分类已修复调度问题。右 H2、D15 与头部/底盘主动动作覆盖边界不变。
