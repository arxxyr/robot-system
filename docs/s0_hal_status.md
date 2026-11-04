# S0 全身硬件统一抽象层：当前边界与下一步

更新：2026-11-03。对照 2026-10-26 架构图和当前源码；冻结设计仍以 [00_design.md](00_design.md) 为准。

当前已具备执行器侧的公共契约、总线内分组和混合目标，尚未完成全身硬件统一入口及相机采集。代码存在、离线验证通过、实机动作通过分别记录。

## 对照架构图

| 图中部分 | 已完成 | 尚缺 / 适用边界 |
|---|---|---|
| S0 执行器契约 | `ManipulatorAdapter` 提供连接、命令、状态、模式、启停及限位；`SerialRtAdapter` 要求停止读回能力 | `BaseAdapter`、`SensorAdapter` 目前只有设计契约，未成为当前生产接口；不能称全身 HAL 已完成 |
| 两条总线及分组 | 左总线：左臂1–6、头7/8的 Position 分组；右总线：右臂1–6位置＋左7/前8/右9 raw轮速的完整混合目标 | 是总线内能力。每个 `s0_daemon --hardware-id` 选择一个硬件配置、一个 adapter 和 executor；现有双总线采用两个实例，图中 S0 框是职责归纳 |
| 唯一执行者与安全链 | 命令槽、SerialRT worker、控制权、超时、持久故障、停止与读回链已实现；每条总线共用控制权和停止域 | 不增加独立分组会话；D15 独立硬件保护仍 `OPEN/DEFERRED`，`deployable=false` |
| Feetech 混合驱动 | 六臂 Position＋三轮 raw Velocity；实证的 Model777/FW3.10/Mode1/Phase12 只允许 raw `0/+500`；原位保持拒绝校准越界 | raw不是已标定的物理速度。默认 factory 仍构造普通 adapter，wire固定 `JointSelection::All`；默认 S0/S1 混合入口未接通 |
| 已有执行器验证 | 三轮分别完成受监督20秒运行、零速、断扭及停止检查；混合维护入口软件通过；真实 FeetechAdapter 已经过内存串口的 `SerialTick` 验证 | 单轮结果不能代替六臂保持＋轮动的混合实机验收，也不能代替默认入口或整车联动验收 |
| 状态发布 | 执行线程写 `RtSnapshot`，Pub读取并发布 `JointState`、`Heartbeat`；观测桥只读 | 当前 Pub 没有相机图像发布接线 |
| 相机 / SoftRT | `ImageHeader + blob` 消息及传输、观测测试基础已有；IO线程有启停骨架 | `io_thread` 仍为空轮询，缺少 `SensorAdapter` 实现、真实采集、帧资源所有权与S0发布接线；此前拍照不算接入HAL |
| S1 / RPC / S2 | 已有上层命令和观测入口，可作为后续接入方 | RPC 在 S0 之外；S2、策略、其他机器人与CAN-FD不属于当前增量 |

## 本次最小离线增量

先验证现有实际工作线程：`LatestCommandSlot → spawn_serial_io_thread → SerialTick → FeetechAdapter → RtSnapshotPublisher`。复用内存串口、独立黄金请求和 `MockClock`，通过握手推进周期；所有设备字节均在内存中。

一个完整用例覆盖启动收敛、真实 pending Enable 和控制权提交、混合目标投递与反馈快照、保持时三轮零速、全九电机停止读回，以及睡眠中请求退出后不再执行IO或发布快照。此前三个 `SerialTick` 用例继续保留，避免再造同类驱动或扩大失败矩阵。

新增用例及原三个真实驱动用例通过，已有工作线程回归16项通过，共20个S0方法；另1项既有符号解码回归通过。验证环境为已有 Lima Linux ARM64 虚拟机，浮动 nightly `708c94ef0`；实际 `cargo fmt --all` 后根 Clippy 零警告。Orin 的两条现有SSH路径本轮不可达，未进行Orin复验。

本轮只补测试夹具与文档，并将旧解码函数的 `map_or_else` 改为等价 `map_or` 以通过新版Clippy；首轮lint失败保留。该用例不经过默认 factory/wire，也不测量真实串口、线程实时性、机械动作或全部 S0 worker 的联合运行。日志与最终结果见[本轮软件验证](../.verify/s0_hal_worker_2026-11-03/summary.json)。

## 接下来的顺序

1. 本次线程链路离线验证及相关回归已完成；保留精简日志与摘要，不重复叠加同类测试。
2. 恢复现场后先通知，沿已准备的维护入口验证一次六臂原位保持＋右轮20秒＋全总线停止。校准越界先拒绝；不自动修正位置或电机参数。
3. 维护实机通过后再接默认 S0 配置/命令路径，并单独验收。相机随后沿既有 SoftRT 与 `ImageHeader + blob` 接一条采集流，先证明采集、发布、退出及不阻塞执行器，再做连续实机采集。

现场缺席时止于可证的软件边界，不通过新增框架、RPC、插件或传感器空实现填充进度。

## 源码与已有证据

- [执行器接口](../crates/robot-core/src/adapter.rs)、[分组与混合目标](../crates/robot-s0/src/grouped_position.rs)、[混合raw驱动](../crates/robot-adapters/feetech/src/mixed_bus.rs)。
- [默认工厂](../crates/robot-adapters/feetech/src/factory.rs)、[IPC命令解析](../crates/robot-s0/src/ipc_thread.rs)、[daemon构造与线程接线](../crates/robot-s0/src/bin/s0_daemon.rs)。
- [SerialRT工作线程](../crates/robot-s0/src/serial_io_thread.rs)、[真实驱动离线夹具](../crates/robot-s0/src/serial_tick_feetech_tests.rs)。
- [SoftRT空轮询](../crates/robot-s0/src/io_thread.rs)、[当前状态发布](../crates/robot-s0/src/pub_thread.rs)、[图像消息](../crates/robot-msg/src/markers.rs)。
- [既有三轮实机原件](../.verify/sdk_wheel_stop_stability_2026-10-22_r1/hardware-validation/summary.json)、[混合维护软件准备](../.verify/s0_bus2_raw_maintenance_2026-10-23_r1/summary.json)、[既有SerialTick验证](../.verify/s0_bus2_raw_serial_tick_2026-10-25_r1/summary.json)。
