# S0 HAL 当前状态

更新：2026-11-09。本页集中记录当前边界与下一步；架构以 [冻结设计](00_design.md) 为准，过往轮次从 [历史索引](handoffs/README.md) 按需查阅。

执行器契约、总线内分组和 raw 混合目标已有软件实现；默认混合入口与相机采集尚未接通，不能称全身 HAL 已完成。**D15 `OPEN/DEFERRED`，`deployable=false`。**

## 完成边界

| 部分 | 已有能力 | 未完成或限制 |
|---|---|---|
| 执行器契约 | `ManipulatorAdapter` 的连接、命令、状态、模式、启停和限位；`SerialRtAdapter` 停止读回 | `BaseAdapter`、`SensorAdapter` 仍是设计契约；RPC、CLI、上层聚合不属本轮 HAL |
| 总线内分组 | 左臂1–6＋头7/8的 Position 分组；右臂1–6位置＋三轮 raw 速度混合目标 | 17电机由两个 daemon 各管一条总线；各自唯一串口所有者、SerialRT writer、命令流、控制权和停止域，无独立组会话 |
| 混合接线 | 显式维护入口复用真实 FeetechAdapter | 默认 factory/wire 未接混合，wire 仍为 `JointSelection::All`；未标定速度、未测量力矩不提供 SI 值 |
| 软件链路 | 内存串口已覆盖命令槽→真实 SerialRT worker→SerialTick→FeetechAdapter→快照，以及保持、全九停止与退出 | 不证明真实调度、UART、机械动作或默认入口；混合维护实机未验收 |
| 发布与观测 | `RtSnapshot`→`JointState`/`Heartbeat`；观测桥只读 | Pub 无相机接线；画面不等于物理动作或安全确认 |
| 相机 | `ImageHeader + blob` 消息、传输与观测测试基础已有 | SoftRT IO 仍为空轮询；缺 `SensorAdapter` 实现、采集、帧资源所有权及发布接线；图像保留独立帧接口，不阻塞执行器 |

## 轮位与阻塞

轮位唯一来源为 [xlerobot_bus2_full.toml](../configs/xlerobot_bus2_full.toml) 同一硬件项的 `joints`/`motor_ids`：同下标对应逻辑关节和物理 ID，当前前/左/右为 **`[8,9,7]`**。仅支持既有 ID7/8/9 的排列，不向驱动、profile、运动学或脚本复制映射。

轮校准按物理 ID 取原值；`base_left9_right7.json` 只按 ID 改标签，数值和电机参数未改。维护入口 `--bus-config` 读取同份 TOML；改配置须重建计划，旧 GO 对新映射无效。`mixed-hold-wheel9` 固定驱动物理 ID9，当前为左轮；历史全 Position 配置不能直接作为混合部署入口。

2026-11-09 分别仅 ID9、仅 ID7 的20秒测试及现场确认，证明其对应自身左、右轮，结束后三轮完全停止。配置收敛提交 `36e663f` 在 Orin 通过319项不同 Rust、153项 Python 测试及 fmt/Clippy；此软件候选未部署、未做新实机动作。单轮通过不代替混合、默认 S0 或整车联动验收；原 run 因固件合同 `DEFERRED` 的整体 FAIL 保留。

2026-11-09右臂18项只读再次确认：右3肘关节 `PP2992 > 2991`、右6夹爪 `PP1371 < 1384`，其余四臂关节在范围内；六电机采样时 TE/PV均0。原帧、身份与资源收尾通过，仅为 `DIAGNOSTIC_CAPTURED / OUTSIDE`，不等于混合动作或整机停止验收。下一步先核对肘部和夹爪的实际姿态与标定端点，不能 clamp、自动挪位置或放宽限位来冒充原位保持。历史混合失败与 `NEEDS_MANUAL_POWER_CUT` 保留；本次不回填旧轮次。左腕 `I=1`、头部 `P=32` 仍是临时已测候选，左1偶发根因 `UNKNOWN`。

## 实机合同与近期顺序

raw 经验命令合同仅覆盖 Model777/FW3.10/Mode1/Phase12 的 `0/+500`，无 SI 换算，不放行名义 profile。三轮停止快照14读要求首末 TE/GV0、PV仅0/±50、1秒五组 PP 跨度≤1且包含快照位置，总预算1.5秒，原字保留。20秒动作、21秒启用上限、500毫秒零速观察及各阶段原判据不放宽，不向其他电机或未批准阶段推广。TE1零速前两组沿原0..1000及−50观察，末组只接受0/±50；SDK初始/断扭后仅±50同连接触发一次新14读，0沿原路径，断扭后保留原1秒观察，不续GO/21秒预算，失败不重试。见[SDK停止复核合同](../.verify/sdk_wheel_stop_stability_2026-10-22_r1/summary.json)。

1. 右臂范围只读已完成，右3/右6仍越界；用户随后确认肘关节与夹爪均靠近端点。该观察尚不能确定标定误差，不修改范围；如需物理调整，须另行现场指令，之后才复用 `--bus2-arm-range-only` 做必要复核。不自动重读或串联动作；已消费 GO 不重用，本次顺序采样不能替代混合启用时的新鲜锚点。
2. 条件满足后，只验六臂固定原位＋ID9 `+500/20秒`、另两轮零速，再执行既有零速、断扭和全总线停止检查；原时效、失败粘性及停止规则保持。
3. 维护实机通过后，按实际授权接默认 S0 入口并独立验收；随后沿 SoftRT 与 `ImageHeader + blob` 接单条相机流，验证采集、发布、退出、不阻塞执行器及连续采集。

当前不扩展 S2、模型推理、其他机器人、CAN-FD、新 HAL crate、插件、多相机同步或通用会话框架。

## 按需证据与源码

- 当前映射：[配置软件验证](../.verify/wheel_config_mapping_2026-11-09/summary.json)、[ID9左轮实机](../.verify/right_wheel_mapping_2026-11-09/summary.json)、[ID7右轮实机](../.verify/wheel_id7_mapping_2026-11-09/summary.json)。
- 右臂范围：[软件及准备](../.verify/arm_range_readonly_2026-11-09/summary.json)、[本次18项原帧与越界结果](../.verify/arm_range_readonly_2026-11-09/hardware/summary.json)。
- 混合与停止：[维护软件准备](../.verify/s0_bus2_raw_maintenance_2026-10-23_r1/summary.json)、[工作线程离线验证](../.verify/s0_hal_worker_2026-11-03/summary.json)、[历史混合失败](../.verify/front_wheel_name_2026-11-05/hardware/mixed-summary.json)、[历史反馈与目视矛盾](../.verify/front_wheel_name_2026-11-05/hardware/summary.json)。后续结果不回写旧失败或未知归因。
- 基础修复：[非法位置、早期失败回执与观测错误处理](../.verify/foundation_errors_2026-11-06/summary.json)。
- 执行器：[接口](../crates/robot-core/src/adapter.rs)、[分组](../crates/robot-s0/src/grouped_position.rs)、[混合驱动](../crates/robot-adapters/feetech/src/mixed_bus.rs)、[工厂](../crates/robot-adapters/feetech/src/factory.rs)、[命令解析](../crates/robot-s0/src/ipc_thread.rs)、[daemon](../crates/robot-s0/src/bin/s0_daemon.rs)。
- 线程与图像：[SerialRT](../crates/robot-s0/src/serial_io_thread.rs)、[驱动夹具](../crates/robot-s0/src/serial_tick_feetech_tests.rs)、[SoftRT IO](../crates/robot-s0/src/io_thread.rs)、[发布](../crates/robot-s0/src/pub_thread.rs)、[图像消息](../crates/robot-msg/src/markers.rs)。
