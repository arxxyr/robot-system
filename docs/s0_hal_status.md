# S0 HAL 当前状态

更新：2026-11-10（UTC）。本页集中记录当前边界与下一步；架构以 [冻结设计](00_design.md) 为准，过往轮次从 [历史索引](handoffs/README.md) 按需查阅。

执行器默认混合接线和多路独立 RGB 的软件实现已完成；此前头部 D435i 单路的真实 S0 采集/发布/退出已通过。两条执行器总线的默认 S0 实机验收、双腕相机配置及三路真实并行验收尚待完成，不能称全身 HAL 已收尾。**D15 `OPEN/DEFERRED`，`deployable=false`。**

## 完成边界

| 部分 | 已有能力 | 未完成或限制 |
|---|---|---|
| 执行器契约 | 既有 `ManipulatorAdapter`/`SerialRtAdapter` 的连接、命令、状态、启停、限位和停止读回 | 底盘沿同总线的显式 raw 速度槽；尚无标定后的 SI 速度合同，不另建 `BaseAdapter` 实现 |
| 总线内分组 | 左臂1–6＋头7/8 Position 分组；右臂1–6位置＋三轮 raw 速度 | 17电机由两个 daemon 各管一条总线；各自保留唯一串口所有者、命令流、控制权和停止域 |
| 混合接线 | TOML 显式选择原 raw factory；新增混合 wire 经 IPC→命令槽→真实 SerialRT→Feetech 内存串口验证 | 旧 Position wire 不携带轮 raw；S1 的 Idle/Scripted 当前仅发送轮 raw0；默认 S0 非零轮动实机尚未验收 |
| 状态反馈 | `JointState`/`Heartbeat` 保留；混合实例未标定速度、未测力矩为 NaN，不把 raw/占位0当SI量 | 内部寄存器原值仍用于诊断和停止；NaN 不作为静止证据 |
| 相机契约 | `[[sensor]]` 按 `hardware_id` 归属；每路 `SensorAdapter`、BGR 帧槽和图像 topic 独立，复用原 SoftRT/`ImageHeader + blob` | 正式配置仅有已核实头部；双腕设备对应待确认；不含深度、IMU或多相机同步 |
| 相机生命周期 | 全部配置相机取得合法新帧才就绪；一路失流只回收该路；Pub轮转且每轮最多一张，状态和心跳优先；关闭时回收全部helper并保留首个清理错误 | 软件组合使用模拟执行器和协议图像；顺序回收可能短暂延迟其他相机；保留1秒帧龄/失流检查，未支持低于1Hz；时间戳仍为读取完成时刻，非曝光时刻 |

## 轮位与阻塞

轮位唯一来源为 [xlerobot_bus2_full.toml](../configs/xlerobot_bus2_full.toml) 同一硬件项的 `joints`/`motor_ids`：同下标对应逻辑关节和物理 ID，当前前/左/右为 **`[8,9,7]`**。仅支持既有 ID7/8/9 的排列，不向驱动、profile、运动学或脚本复制映射。

轮校准按物理 ID 取原值；`base_left9_right7.json` 只按 ID 改标签，数值和电机参数未改。维护入口 `--bus-config` 读取同份 TOML；改配置须重建计划，旧 GO 对新映射无效。混合测试使用 `--bus2-mixed-raw-hold --mixed-wheel-id 8` 选择当前前轮；所选ID、目标和动作范围绑定计划及GO。省略选轮参数仍为物理ID9，旧 `mixed-hold-wheel9` 入口保留。历史全 Position 配置不能直接作为混合部署入口。

2026-11-09 分别仅 ID9、仅 ID7 的20秒测试及现场确认，证明其对应自身左、右轮，结束后三轮完全停止。配置收敛提交 `36e663f` 在 Orin 通过319项不同 Rust、153项 Python 测试及 fmt/Clippy；此软件候选未部署、未做新实机动作。单轮通过不代替混合、默认 S0 或整车联动验收；原 run 因固件合同 `DEFERRED` 的整体 FAIL 保留。

右3最近只读为 `PP2992 > 原上限2991`，用户确认靠近端点；历史严格边界下的越界结论保留。用户已批准逐电机配置校准边界容差，本机位置关节当前设20，仍禁止clamp后冒充原位保持。右6夹爪张开到 `GP1600` 已通过；随后合回 `GP1397`，反馈 `1596→1402`，现场确认明显合拢、结束静止，断扭、整机停止及身份检查通过。合回偏差+5未通过原800毫秒/±4要求，原 `FAIL` 保留；用户随后接受5计数偏差，现仅右6单程开合容差改为±5，其他关节仍为±4，未追加实机动作。开合复用同一流程，仅两固定目标，原校准范围保留 `[1384,2847]`，起点按配置容差检查、绝对增量≤256，GP写即动作起点，不改速度/PID/电机硬件限位。历史混合失败与 `NEEDS_MANUAL_POWER_CUT` 不回填。左腕 `I=1`、头部 `P=32` 仍是临时已测候选，左1偶发根因 `UNKNOWN`。

位置边界容差在各总线TOML的 `calibration_range_tolerances_raw` 中逐项配置，与 `joints/motor_ids` 同下标。左总线八个位置关节为 `[20,20,20,20,20,20,20,20]`；右总线六臂及三轮为 `[20,20,20,20,20,20,0,0,0]`。每项可独立改成0或其他整数，省略整个字段为全0；只扩展软件校准边界，仍限raw 0..4095。原校准文件、到位精度和停止判据不变，改配置须重新生成维护计划。该配置已通过Orin软件验证并用于下述维护测试，尚未部署到控制服务。

2026-11-10 使用容差配置完成六右臂原位保持＋前轮ID8 `+500/20秒`：400周期，零速反馈450→0→0，原位、断扭、17电机停止及身份检查均通过，现场确认前轮持续转动、右臂保持原位、结束后三轮全部停止。此次受监督维护路径通过，不代替关节往返精度、默认S0入口或整车联动验收。此前同日左9混合测试的反馈与停止通过，但用户未观察，仍为 `NOT_OBSERVED`；未重复左轮。两次原帧分别保存在 `.verify/mixed_front_2026-11-10/hardware/`、`.verify/mixed_tolerance_preparation_2026-11-10/hardware/` 及 Orin 对应目录，历史失败不回写。

本轮软件验证：Orin 共967个不同 Rust 测试方法、9项 Python 测试通过，fmt→根目录/真实硬件特性 Clippy 通过；首次筛选未命中的4项混合工作线程测试已按实际模块名补跑。首次相机因 Python 启动钩子向二进制流打印SDK版本而拒绝就绪，已用隔离启动修正并实机复验；失败原件保留。源码尚未安装到正式控制服务。

## 实机合同与近期顺序

raw 经验命令合同仅覆盖 Model777/FW3.10/Mode1/Phase12 的 `0/+500`，无 SI 换算，不放行名义 profile。三轮停止快照14读要求首末 TE/GV0、PV仅0/±50、1秒五组 PP 跨度≤1且包含快照位置，总预算1.5秒，原字保留。20秒动作、21秒启用上限、500毫秒零速观察及各阶段原判据不放宽，不向其他电机或未批准阶段推广。TE1零速前两组沿原0..1000及−50观察，末组只接受0/±50；SDK初始/断扭后仅±50同连接触发一次新14读，0沿原路径，断扭后保留原1秒观察，不续GO/21秒预算，失败不重试。见[SDK停止复核合同](../.verify/sdk_wheel_stop_stability_2026-10-22_r1/summary.json)。

1. 前轮混合维护的实际动作与停止已通过；本轮已接默认 factory/wire 和 S1 的混合保持路径，两份实机配置均显式 `auto_enable=false`。
2. 头部相机实时 USB 型号为 **D435i**，序列号 `043323051347`；RGB 对应配置中的稳定 by-path（当前枚举 `/dev/video8`），采集目标为640×480、15 FPS。真实输出 FPS/帧龄须由订阅端测量，驱动报告值不代替发布实测。
3. 此前单路真实 D435i＋模拟执行器的 S0 连续收到145帧（约10秒，14.47 FPS），最大帧龄20.86毫秒、心跳最大间隔21.93毫秒；画面已检查，daemon/订阅器退出码均0，相机设备释放。FPS不是收尾门槛，不继续调优。该结果不替代新版多路或真实执行器并行测试。
4. 当前相机唯一配置为 `[[sensor]]`，旧 `[camera]` 明确拒绝。Orin只读枚举可见两个 `USB2.0_CAM1` 节点，稳定路径分别为 `platform-3610000.usb-usb-0:2.1:1.0-video-index0` 和 `platform-3610000.usb-usb-0:2.2:1.0-video-index0`；实际左右腕对应和采集格式待确认，暂不猜填配置。
5. 后续准备默认 S0 的执行器实机验收。现有20秒维护入口直接调用 adapter，不能仅替换二进制冒充 daemon 验收；仍缺有界非零混合发送入口及沿唯一写者完成零速阶段 TE/GV/PV 读回。准备完成后再通知动作、确认现场，不复用此前 GO。

当前不扩展 S2、模型推理、其他机器人、CAN-FD、新 HAL crate、插件、多相机同步或通用会话框架。

## 按需证据与源码

- 当前映射：[配置软件验证](../.verify/wheel_config_mapping_2026-11-09/summary.json)、[ID9左轮实机](../.verify/right_wheel_mapping_2026-11-09/summary.json)、[ID7右轮实机](../.verify/wheel_id7_mapping_2026-11-09/summary.json)。
- 右臂范围：[软件及准备](../.verify/arm_range_readonly_2026-11-09/summary.json)、[本次18项原帧与越界结果](../.verify/arm_range_readonly_2026-11-09/hardware/summary.json)。
- 单右夹爪：[本次合回与停止结果](../.verify/right_gripper_close_2026-11-10/summary.json)、[此前张开结果](../.verify/right_gripper_release_2026-11-10/hardware-r3/summary.json)、[最初失败原件](../.verify/right_gripper_release_2026-11-10/hardware/summary.json)。
- 混合与停止：[维护软件准备](../.verify/s0_bus2_raw_maintenance_2026-10-23_r1/summary.json)、[工作线程离线验证](../.verify/s0_hal_worker_2026-11-03/summary.json)、[历史混合失败](../.verify/front_wheel_name_2026-11-05/hardware/mixed-summary.json)、[历史反馈与目视矛盾](../.verify/front_wheel_name_2026-11-05/hardware/summary.json)。后续结果不回写旧失败或未知归因。
- 基础修复：[非法位置、早期失败回执与观测错误处理](../.verify/foundation_errors_2026-11-06/summary.json)。
- 执行器：[接口](../crates/robot-core/src/adapter.rs)、[分组](../crates/robot-s0/src/grouped_position.rs)、[混合驱动](../crates/robot-adapters/feetech/src/mixed_bus.rs)、[工厂](../crates/robot-adapters/feetech/src/factory.rs)、[命令解析](../crates/robot-s0/src/ipc_thread.rs)、[daemon](../crates/robot-s0/src/bin/s0_daemon.rs)。
- 本轮软件与相机证据：`.verify/s0_body_hal_2026-11-10/`，Orin `/home/loosqk/robot-checks/s0-body-hal-20261110/`；原失败日志保留。
- 默认 daemon 相机/执行器组合：[测试](../crates/robot-s0/tests/comm_fault_startup/camera_actuator.rs)、[验证摘要](../.verify/s0_camera_actuator_2026-11-10/summary.json)。Orin 4项相机集成测试及 fmt→根目录/测试特性 Clippy 通过；仅软件接线，无实机动作。
- 多路相机软件：[验证摘要](../.verify/s0_multi_camera_2026-11-10/summary.json)。Orin 90项不同测试通过，含三路协议图像与模拟执行器组合；fmt→根目录及S0全部特性Clippy通过。早期lint/断言编译失败原件保留；未开相机、未访问UART、未部署。
- 线程与图像：[SerialRT](../crates/robot-s0/src/serial_io_thread.rs)、[驱动夹具](../crates/robot-s0/src/serial_tick_feetech_tests.rs)、[SoftRT IO](../crates/robot-s0/src/io_thread.rs)、[发布](../crates/robot-s0/src/pub_thread.rs)、[图像消息](../crates/robot-msg/src/markers.rs)。
