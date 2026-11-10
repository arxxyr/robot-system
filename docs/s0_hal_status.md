# S0 HAL 当前状态

更新：2026-11-10（UTC）。本页集中记录当前边界与下一步；架构以 [冻结设计](00_design.md) 为准，过往轮次从 [历史索引](handoffs/README.md) 按需查阅。

执行器默认混合接线和多路独立 RGB 的软件实现已完成；三路真实相机已在模拟执行器的默认S0中完成共同10秒采集、发布和正常退出。两条执行器总线的默认S0实机验收、双腕有效画面及左右配置、真实执行器与相机组合仍待完成，不能称全身HAL已收尾。**D15 `OPEN/DEFERRED`，`deployable=false`。**

## 完成边界

| 部分 | 已有能力 | 未完成或限制 |
|---|---|---|
| 执行器契约 | 既有 `ManipulatorAdapter`/`SerialRtAdapter` 的连接、命令、状态、启停、限位和停止读回 | 底盘沿同总线的显式 raw 速度槽；尚无标定后的 SI 速度合同，不另建 `BaseAdapter` 实现 |
| 总线内分组 | 左臂1–6＋头7/8 Position 分组；右臂1–6位置＋三轮 raw 速度 | 17电机由两个 daemon 各管一条总线；各自保留唯一串口所有者、命令流、控制权和停止域 |
| 混合接线 | TOML 显式选择原 raw factory；新增混合 wire 经 IPC→命令槽→真实 SerialRT→Feetech 内存串口验证 | 旧 Position wire 不携带轮 raw；S1 的 Idle/Scripted 当前仅发送轮 raw0；默认 S0 非零轮动实机尚未验收 |
| 状态反馈 | `JointState`/`Heartbeat` 保留；混合实例未标定速度、未测力矩为 NaN，不把 raw/占位0当SI量 | 内部寄存器原值仍用于诊断和停止；NaN 不作为静止证据 |
| 相机契约 | `[[sensor]]` 按 `hardware_id` 归属；每路 `SensorAdapter`、BGR 帧槽和图像 topic 独立，复用原 SoftRT/`ImageHeader + blob` | 正式配置仅有已核实头部；双腕设备对应待确认；不含深度、IMU或多相机同步 |
| 相机生命周期 | 全部配置相机取得合法新帧才就绪；一路失流只回收该路；Pub轮转且每轮最多一张，状态和心跳优先；三路真实RGB＋模拟执行器的采集、发布和退出已验证 | TERM宽限按两帧周期加100ms计算；顺序回收可能短暂延迟其他相机；保留1秒帧龄/失流检查，未支持低于1Hz；时间戳为读取完成时刻；真实执行器组合未验 |

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
4. 相机只使用 `[[sensor]]`。腕部两只 `USB2.0_CAM1` 序列号相同，用稳定USB端口2.1/2.2区分；原版候选左2.2、右2.1尚未写入正式配置。[三路真实相机＋模拟执行器复验](../.verify/s0_camera_stop_2026-11-10/summary.json)共同观察10秒通过，头部约14.57 FPS、两腕约5.02 FPS，最大帧龄28.02ms；daemon/观察器均退出0，三个设备释放。三张画面仍几乎全黑，内容及左右映射待照明后核对。首次试验因原100ms回收宽限不足而失败，现仅修正相机退出等待；原失败、[设备身份与原帧](../.verify/s0_wrist_camera_2026-11-10/orin/capture/result.json)保留。
5. 默认 S0 已沿原 SerialRT 连接补齐 raw 混合零目标后的三轮 TE/GV/PV 原字观测，尝试间隔至少20毫秒；周期PP/PV另保留原字，至少50毫秒输出一组且不增加事务。日志不提交安全状态，格式见[日志说明](logging.md#raw-混合零目标的设备观测)。已有 `supervised_demo --s0-mixed-hold` 通过原IPC发送固定前轮20秒混合目标，并检查79组运动原字、3组零速原字及Disable/TorqueOff；六臂使用发送器冻结的新鲜位置，报告不冒充adapter内部Enable锚点。
6. 新发送模式的每帧来源期限最多200毫秒，首帧额外受冻结锚点200毫秒期限约束，均受21秒启用上限限制；旧锚点可能使首帧提前到期并安全拒绝试验，不延长锚点授权。20秒从首次非零发送attempt起算，不把IPC发送时间当成设备切换时刻。现有维护入口新增 `--s0-mixed-hold`：原PLAN/GO、初始四锁与停止检查之后，仅将右双锁交给默认S0；发送器完成前轮20秒、零速及Disable，再通过原事件收紧EStop。外层确认双latch、真实退出与锁归还后才执行原STOP51；旧直接adapter入口保持原合同。
7. 新入口只按同份右总线TOML选择前轮与六臂固定保持，显式复用既有右侧保护目录；不启动相机，不迁移配置身份或保护路径。右latch文件由既有writer合法rename为0640，原文件只读句柄保持到收尾，防止inode重用被误判；其他保护约束不变。sender发布EStop不冒充已持久化，子进程未确认收尾的manual状态保持粘性。新增读取的真实总线耗时及默认S0实机动作仍未验证；完整软件验证和入口准备后再通知并确认现场，不复用此前GO。

当前维护接入验证：Orin 62项Rust、201项维护测试及2项实际进程＋PTY测试通过，另3项既有可选夹具跳过；fmt、根目录及发送器特性Clippy通过。正常场景完成400次发送、79组运动反馈、3组零速观察、Disable与最终EStop；启用NACK场景无非零发送，保留故障及人工断电标记，不用旧状态回填。PTY的整机STOP51由软件模型提供，不代替真实UART停止读回。26项只读接入预检、PLAN_ONLY和固定解释器启动脚本已准备好，尚未生成GO或执行实机动作。[当前验证与准备](../.verify/s0_maintenance_bridge_2026-11-10/summary.json)保留初次失败；[此前默认S0验证](../.verify/s0_mixed_supervised_2026-11-10/summary.json)继续有效。下一步仅本范围实机验收，动作前先通知并确认现场。

相机回收修复另经Orin 46项定向测试及fmt/Clippy通过；新版S0已重新绑定同范围混合维护入口，26项只读预检与PLAN通过，仍未创建GO。当前待执行脚本为Orin `/home/loosqk/robot-checks/s0-maintenance-camera-stop-20261110/launch.sh`，范围仍为右臂原位保持＋前轮20秒，不启动相机。

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
- raw零目标设备观测：[软件验证](../.verify/s0_wheel_observation_2026-11-10/summary.json)，覆盖原字、配置轮序、限频、读失败/急停的整总线收敛，以及Pub状态/心跳继续发布；不代替真实总线或停止验收。
- 线程与图像：[SerialRT](../crates/robot-s0/src/serial_io_thread.rs)、[驱动夹具](../crates/robot-s0/src/serial_tick_feetech_tests.rs)、[SoftRT IO](../crates/robot-s0/src/io_thread.rs)、[发布](../crates/robot-s0/src/pub_thread.rs)、[图像消息](../crates/robot-msg/src/markers.rs)。
