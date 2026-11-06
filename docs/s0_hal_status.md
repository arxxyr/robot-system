# S0 全身硬件统一抽象层：当前边界与下一步

更新：2026-11-06。对照 2026-10-26 架构图和当前源码；冻结设计仍以 [00_design.md](00_design.md) 为准。

当前已具备执行器侧的公共契约、总线内分组和混合目标，尚未完成全身硬件统一入口及相机采集。代码存在、离线验证通过、实机动作通过分别记录。

2026-11-05 现场轮位更正：底盘是左轮 ID7、前轮 ID8、右轮 ID9；此前 ID8 的“后轮”称呼有误。现行配置、S0 布局和混合驱动统一使用 `base_front_wheel`，轮速数组仍为前8/左7/右9，未改变 ID、方向或运动学。旧 `base.json` 的 `base_back_wheel` 仅在校准加载处兼容，要求 ID8 且不能同时存在新旧键；历史日志和原帧保持原文。

该修正在 Orin 最新 nightly `249c1e4e3` 上通过 fmt、两级 Clippy、32项定向测试和维护工具构建；77项文件引用核验通过。连接、主机预检和混合维护计划已恢复；“无 UART、GO 或动作”仅指这一软件准备阶段，详见[验证与准备记录](../.verify/front_wheel_name_2026-11-05/summary.json)。

随后原 `xlerobot-handoff.service` 完成全17电机交接，状态为 `success/active/exited`。混合 r1 因启动器的 uv 父进程被身份门拒绝，未进入 UART；改用既有 Python 直接启动后，r2 在启用前因右3位置2992高于上限2991而拒绝，同次右6位置1371也低于下限1384。混合周期为0，没有 TE1、位置或速度目标写入；最终仅发送全九 TE0，未完成停止读回。用户拒绝断电，原 `NEEDS_MANUAL_POWER_CUT` 保留，不能记录为已断电。

之后独立授权的右9单轮测试完成400次 raw +500、20秒和79组反馈，累计位置变化9951计数；零速反馈200→0→0，初始和最终停止检查通过。但用户明确报告“右轮轮胎本身完全没转”，因此本次实际动作验收未通过，反馈与实物观察的矛盾仍待排查。历史三轮通过结论不回写，详见[本次硬件摘要](../.verify/front_wheel_name_2026-11-05/hardware/summary.json)及[混合失败原件摘要](../.verify/front_wheel_name_2026-11-05/hardware/mixed-summary.json)。

新 GO 下按同一路径复测右9的单轮 r2，仍为400次 +500、20秒、79组反馈及累计9951计数，零速350→0→0；停止、位置和身份检查通过，`manual=false`。用户第二次仍报告“完全没有动，速度大一点”，并确认观察机器人自身右侧（面对时在用户左手边），因此不能归因为左右概念误解，原因仍 `UNKNOWN`，两次目视结论分别保留。当时拔插电源仅为计划；用户随后已明确报告“我已经断电重连了”，该回执不回填旧轮次，也不代替新的现场就绪和动作授权。

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

UTC2026-11-06 按用户“先把基础立牢”完成本轮六项定向修复：Scripted 与全选 Position 拒绝非法位置；校准锚点诊断保留 motor/raw；混合早期失败回执字段完整；观测报告保留主错误；健康采样失败明确不可用；终态日志记录实际入队。相邻的有限极值换算溢出也已修正，保留既有校准夹紧。原[定向评审](../.verify/mixed_failure_diagnostic_2026-11-06/review/summary.json)保留为修复前快照，当前结果见[基础修复验证](../.verify/foundation_errors_2026-11-06/summary.json)。

此前 Python 混合失败解析修复也已保留，原校准错误不再被400周期成功断言遮蔽。本轮 Orin 1121项Rust、30项维护脚本及真实Rust→Python早期失败验证通过，fmt→根/real-hw Clippy通过；早期入口验证使用过期GO，串口打开及新动作均为0。临时资源已清理，原冻结硬件产物未变，停止规则和电机参数未改。用户已报告电源断电重连；旧两轮“反馈变化、轮胎未动”的原因仍未知，不据软件通过宣称实机已闭环。

随后对照旧目视通过轮次与本次两轮右9原件：三轮累计均为9951，但仅统计至第79组约19.75秒；新两轮79组速度有48组不同、位置增量有57组不同，位置前后衔接，没有发现整段复制反馈的证据。Orin安装的SDK/pyserial四文件哈希与原绑定相同，实际调用到系统串口`open/read/write`；现有记录仍不能独立证明ID9就是现场观察的轮胎。历史三路机载相机均看不到轮胎，不能用房间背景静止代替轮动观察。核对结果及准备见[右轮对应关系诊断](../.verify/right_wheel_mapping_2026-11-06/summary.json)。

右9首次14读已正常完成，随后按用户要求暂停；网络恢复后，因暂停超过600秒，在新的现场回执下完成第二次手转前14读。两段首末使能和目标速度均为0，五组位置均2583、速度均0；各自108项身份检查通过、74个子进程回收，没有寄存器写入。它们是两次前置诊断，不是“手转前后”对照，也不代表整机停止或映射验收。原件分别见[暂停前采集](../.verify/right_wheel_mapping_2026-11-06/hardware-before/summary.json)和[恢复后前置采集](../.verify/right_wheel_mapping_2026-11-06/hardware-before-r2/summary.json)。

随后用户报告整个右轮盘轻转约四分之一圈、放手静止；因原现场时效已过，收到新的现场回执后才执行第二段14读。后段正常退出，五组位置仍全部2583、速度均0，首末使能和目标速度均0，108项身份检查通过、74个子进程回收，无寄存器写入。用户进一步说明没有看清轮盘是否回到原角度，因此本次映射对照为不可判定，不能把零差值归因为接线错误、机械损坏、假反馈或用户未操作；也不能外推固件在Mode1/TE0下的位置反馈机制。原帧及现场回执见[手转后诊断](../.verify/right_wheel_mapping_2026-11-06/hardware-after/summary.json)。

限定查阅原厂STS内存表、STS3215规格书、飞特SDK和原版XLeRobot后，尚未找到本机FW3.10/Mode1/Phase12/TE0组合下的外力手转位置更新保证；同样没有厂家依据证明它会冻结或采用速度积分。通用表的“当前绝对位置”、另一模式的校准流程和SDK缓存接口不能代替本机合同，详见[反馈语义核查](../.verify/right_wheel_mapping_2026-11-06/feedback-contract-review.json)。

1. 本次线程链路离线验证及相关回归已完成；保留精简日志与摘要，不重复叠加同类测试。
2. 右9手转对照已完成采集，资料核查也已收口，但映射及本机断扭反馈合同仍未确认。下一步只解决实物轮盘、驱动地址和实际传动是否对应；若再次以手转核对，必须能确认后段采样时轮盘仍处于不同角度，并保留上述固件语义限制，只拨外围自由滚子不能检验映射。当前不追加动作、不改变参数或停止判据，不因零差值就重跑20秒或增大速度。混合入口还需处理右3/右6原位超出校准范围的问题，不自动修正位置、参数或放宽限位。阻塞解决后才沿原维护入口验一次六臂原位保持＋右轮20秒＋全总线停止，动作前先通知。
3. 维护实机通过后再接默认 S0 配置/命令路径，并单独验收。相机随后沿既有 SoftRT 与 `ImageHeader + blob` 接一条采集流，先证明采集、发布、退出及不阻塞执行器，再做连续实机采集。

具体启动脚本、固定解释器、进程检查和 `PLAN_ONLY` 必须在询问现场状态前准备完成；确认后仅做必要实时预检，随即启动。硬件入口直接使用既有固定 Python，不用 `uv run` 增加一个会被身份门识别为维护进程的父进程。

现场缺席时止于可证的软件边界，不通过新增框架、RPC、插件或传感器空实现填充进度。

## 源码与已有证据

- [执行器接口](../crates/robot-core/src/adapter.rs)、[分组与混合目标](../crates/robot-s0/src/grouped_position.rs)、[混合raw驱动](../crates/robot-adapters/feetech/src/mixed_bus.rs)。
- [默认工厂](../crates/robot-adapters/feetech/src/factory.rs)、[IPC命令解析](../crates/robot-s0/src/ipc_thread.rs)、[daemon构造与线程接线](../crates/robot-s0/src/bin/s0_daemon.rs)。
- [SerialRT工作线程](../crates/robot-s0/src/serial_io_thread.rs)、[真实驱动离线夹具](../crates/robot-s0/src/serial_tick_feetech_tests.rs)。
- [SoftRT空轮询](../crates/robot-s0/src/io_thread.rs)、[当前状态发布](../crates/robot-s0/src/pub_thread.rs)、[图像消息](../crates/robot-msg/src/markers.rs)。
- [既有三轮实机原件](../.verify/sdk_wheel_stop_stability_2026-10-22_r1/hardware-validation/summary.json)、[混合维护软件准备](../.verify/s0_bus2_raw_maintenance_2026-10-23_r1/summary.json)、[既有SerialTick验证](../.verify/s0_bus2_raw_serial_tick_2026-10-25_r1/summary.json)。
