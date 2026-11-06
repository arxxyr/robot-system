# S0 全身硬件统一抽象层：当前边界与下一步

更新：2026-11-05。对照 2026-10-26 架构图和当前源码；冻结设计仍以 [00_design.md](00_design.md) 为准。

当前已具备执行器侧的公共契约、总线内分组和混合目标，尚未完成全身硬件统一入口及相机采集。代码存在、离线验证通过、实机动作通过分别记录。

2026-11-05 现场轮位更正：底盘是左轮 ID7、前轮 ID8、右轮 ID9；此前 ID8 的“后轮”称呼有误。现行配置、S0 布局和混合驱动统一使用 `base_front_wheel`，轮速数组仍为前8/左7/右9，未改变 ID、方向或运动学。旧 `base.json` 的 `base_back_wheel` 仅在校准加载处兼容，要求 ID8 且不能同时存在新旧键；历史日志和原帧保持原文。

该修正在 Orin 最新 nightly `249c1e4e3` 上通过 fmt、两级 Clippy、32项定向测试和维护工具构建；77项文件引用核验通过。连接、主机预检和混合维护计划已恢复；“无 UART、GO 或动作”仅指这一软件准备阶段，详见[验证与准备记录](../.verify/front_wheel_name_2026-11-05/summary.json)。

随后原 `xlerobot-handoff.service` 完成全17电机交接，状态为 `success/active/exited`。混合 r1 因启动器的 uv 父进程被身份门拒绝，未进入 UART；改用既有 Python 直接启动后，r2 在启用前因右3位置2992高于上限2991而拒绝，同次右6位置1371也低于下限1384。混合周期为0，没有 TE1、位置或速度目标写入；最终仅发送全九 TE0，未完成停止读回。用户拒绝断电，原 `NEEDS_MANUAL_POWER_CUT` 保留，不能记录为已断电。

之后独立授权的右9单轮测试完成400次 raw +500、20秒和79组反馈，累计位置变化9951计数；零速反馈200→0→0，初始和最终停止检查通过。但用户明确报告“右轮轮胎本身完全没转”，因此本次实际动作验收未通过，反馈与实物观察的矛盾仍待排查。历史三轮通过结论不回写，详见[本次硬件摘要](../.verify/front_wheel_name_2026-11-05/hardware/summary.json)及[混合失败原件摘要](../.verify/front_wheel_name_2026-11-05/hardware/mixed-summary.json)。

新 GO 下按同一路径复测右9的单轮 r2，仍为400次 +500、20秒、79组反馈及累计9951计数，零速350→0→0；停止、位置和身份检查通过，`manual=false`。用户第二次仍报告“完全没有动，速度大一点”，并确认观察机器人自身右侧（面对时在用户左手边），因此不能归因为左右概念误解，原因仍 `UNKNOWN`，两次目视结论分别保留。用户随后提出拔插电源，只表示计划，实际断电和恢复尚未确认；暂不追加动作或改变速度合同。

## 对照架构图

| 图中部分 | 已完成 | 尚缺 / 适用边界 |
|---|---|---|
| S0 执行器契约 | `ManipulatorAdapter` 提供连接、命令、状态、模式、启停及限位；`SerialRtAdapter` 要求停止读回能力 | `BaseAdapter`、`SensorAdapter` 目前只有设计契约，未成为当前生产接口；不能称全身 HAL 已完成 |
| 两条总线及分组 | 左总线：左臂1–6、头7/8的 Position 分组；右总线：右臂1–6位置＋左7/前8/右9 raw轮速的完整混合目标 | 是总线内能力。每个 `s0_daemon --hardware-id` 选择一个硬件配置、一个 adapter 和 executor；现有双总线采用两个实例，图中 S0 框是职责归纳 |
| 唯一执行者与安全链 | 命令槽、SerialRT worker、控制权、超时、持久故障、停止与读回链已实现；每条总线共用控制权和停止域 | 不增加独立分组会话；D15 独立硬件保护仍 `OPEN/DEFERRED`，`deployable=false` |
| Feetech 混合驱动 | 六臂 Position＋三轮 raw Velocity；实证的 Model777/FW3.10/Mode1/Phase12 只允许 raw `0/+500`；原位保持拒绝校准越界 | raw不是已标定的物理速度。默认 factory 仍构造普通 adapter，wire固定 `JointSelection::All`；默认 S0/S1 混合入口未接通 |
| 已有执行器验证 | 历史三轮分别完成受监督20秒运行、零速、断扭及停止检查；混合维护入口软件通过；真实 FeetechAdapter 已经过内存串口的 `SerialTick` 验证 | 本次混合在启用前被校准范围拒绝；独立右9反馈与轮胎未转的观察不符，实际动作未验收。单轮结果不能代替混合、默认入口或整车联动验收 |
| 状态发布 | 执行线程写 `RtSnapshot`，Pub读取并发布 `JointState`、`Heartbeat`；观测桥只读 | 当前 Pub 没有相机图像发布接线 |
| 相机 / SoftRT | `ImageHeader + blob` 消息及传输、观测测试基础已有；IO线程有启停骨架 | `io_thread` 仍为空轮询，缺少 `SensorAdapter` 实现、真实采集、帧资源所有权与S0发布接线；此前拍照不算接入HAL |
| S1 / RPC / S2 | 已有上层命令和观测入口，可作为后续接入方 | RPC 在 S0 之外；S2、策略、其他机器人与CAN-FD不属于当前增量 |

## 此前最小离线增量（2026-11-03）

先验证现有实际工作线程：`LatestCommandSlot → spawn_serial_io_thread → SerialTick → FeetechAdapter → RtSnapshotPublisher`。复用内存串口、独立黄金请求和 `MockClock`，通过握手推进周期；所有设备字节均在内存中。

一个完整用例覆盖启动收敛、真实 pending Enable 和控制权提交、混合目标投递与反馈快照、保持时三轮零速、全九电机停止读回，以及睡眠中请求退出后不再执行IO或发布快照。此前三个 `SerialTick` 用例继续保留，避免再造同类驱动或扩大失败矩阵。

新增用例及原三个真实驱动用例通过，已有工作线程回归16项通过，共20个S0方法；另1项既有符号解码回归通过。验证环境为已有 Lima Linux ARM64 虚拟机，浮动 nightly `708c94ef0`；实际 `cargo fmt --all` 后根 Clippy 零警告。当时 Orin 的两条SSH路径不可达，该轮未进行Orin复验。

本轮只补测试夹具与文档，并将旧解码函数的 `map_or_else` 改为等价 `map_or` 以通过新版Clippy；首轮lint失败保留。该用例不经过默认 factory/wire，也不测量真实串口、线程实时性、机械动作或全部 S0 worker 的联合运行。日志与最终结果见[本轮软件验证](../.verify/s0_hal_worker_2026-11-03/summary.json)。

## 接下来的顺序

UTC2026-11-06 错误传播评审确认一项需优先处理的 P1：Scripted 接受非有限位置，全选 Position 路径缺少有限性检查，NaN 可被 Feetech 编为 raw2047。Orin 真实 S1 和 Feetech 内存串口各一项复现确认缺陷，未访问实机。另有四项 P2 诊断信息丢失/状态表达问题及一项 P3 日志误报，详见[定向评审](../.verify/mixed_failure_diagnostic_2026-11-06/review/summary.json)。这些发现尚未修复，建议先修 P1，再继续 S0 实机集成。

本轮只修了已定位的 Python 混合失败解析：原校准错误不再被400周期成功断言遮蔽，成功验收和停止规则未变。本机、Orin 各30项相关测试及原生 fmt→Clippy 通过。用户已报告电源断电重连；未据此追加动作，旧两轮“反馈变化、轮胎未动”的原因仍未知。见[修复验证](../.verify/mixed_failure_diagnostic_2026-11-06/summary.json)。

1. 本次线程链路离线验证及相关回归已完成；保留精简日志与摘要，不重复叠加同类测试。
2. 先对照原版和本次原帧，核对右9反馈与轮胎未转的矛盾；混合入口还需处理右3/右6原位超出校准范围的问题，不自动修正位置、参数或放宽限位。阻塞解决后才沿原维护入口验一次六臂原位保持＋右轮20秒＋全总线停止，动作前先通知。
3. 维护实机通过后再接默认 S0 配置/命令路径，并单独验收。相机随后沿既有 SoftRT 与 `ImageHeader + blob` 接一条采集流，先证明采集、发布、退出及不阻塞执行器，再做连续实机采集。

具体启动脚本、固定解释器、进程检查和 `PLAN_ONLY` 必须在询问现场状态前准备完成；确认后仅做必要实时预检，随即启动。硬件入口直接使用既有固定 Python，不用 `uv run` 增加一个会被身份门识别为维护进程的父进程。

现场缺席时止于可证的软件边界，不通过新增框架、RPC、插件或传感器空实现填充进度。

## 源码与已有证据

- [执行器接口](../crates/robot-core/src/adapter.rs)、[分组与混合目标](../crates/robot-s0/src/grouped_position.rs)、[混合raw驱动](../crates/robot-adapters/feetech/src/mixed_bus.rs)。
- [默认工厂](../crates/robot-adapters/feetech/src/factory.rs)、[IPC命令解析](../crates/robot-s0/src/ipc_thread.rs)、[daemon构造与线程接线](../crates/robot-s0/src/bin/s0_daemon.rs)。
- [SerialRT工作线程](../crates/robot-s0/src/serial_io_thread.rs)、[真实驱动离线夹具](../crates/robot-s0/src/serial_tick_feetech_tests.rs)。
- [SoftRT空轮询](../crates/robot-s0/src/io_thread.rs)、[当前状态发布](../crates/robot-s0/src/pub_thread.rs)、[图像消息](../crates/robot-msg/src/markers.rs)。
- [既有三轮实机原件](../.verify/sdk_wheel_stop_stability_2026-10-22_r1/hardware-validation/summary.json)、[混合维护软件准备](../.verify/s0_bus2_raw_maintenance_2026-10-23_r1/summary.json)、[既有SerialTick验证](../.verify/s0_bus2_raw_serial_tick_2026-10-25_r1/summary.json)。
