# S0 HAL 当前状态

更新：2026-11-11（America/Los_Angeles；实机验收为 UTC 2026-11-11）。架构以[冻结设计](00_design.md)为准。

**通用机器人控制架构的 S0 阶段：首个 XLeRobot 后端已通过受监督 raw 最小闭环。** 默认S0维护入口分别完成：左臂六关节＋头部两关节保持20秒并采头/左腕RGB；右臂六关节保持＋前轮ID8 raw `+500/20秒`并采右腕RGB。三路画面和双腕映射已确认，两侧动作观察、断扭、EStop持久化、原STOP51、位置/身份及清场检查均通过，`manual=false`。

**D15 `OPEN/DEFERRED`，`deployable=false`；源码及配置尚未安装到正式控制服务。** 此验收为左右分别执行，不覆盖两总线同时联动、无人值守、深度/IMU或标定后的SI速度。

部署候选沿原handoff→guard→S0，绑定已验S0和同批full配置，保留`xlerobot_left/right`保护目录。安装、备份、回滚入口已准备，Orin上31项测试及只读预检通过；配置矩阵打包纳入0.0.1，38项测试通过，按TOML选择现有程序、feature和文件并记录版本、提交及SHA。候选尚未安装或启动，发布包仍需冻结部署配置；详见[本轮软件与产物记录](../.verify/s0_install_prepare_2026-11-11/summary.json)及[原候选记录](../.verify/s0_deploy_prepare_2026-11-11/summary.json)。

## 已有边界

| 层/部分 | 当前能力与限制 |
|---|---|
| S1 | 沿既有控制权与命令接口；Idle/Scripted当前仅发送轮raw0，非零raw验收使用维护入口 |
| S0 HAL | `robot-core`定义契约，`robot-s0`组织设备、分组与生命周期；两个daemon各管一条总线，每条总线唯一串口所有者、SerialRT writer、命令流、控制权及停止域 |
| 硬件适配器 | 沿既有`ManipulatorAdapter`/`SerialRtAdapter`封装Feetech协议；左总线臂1–6＋头7/8位置分组，右总线臂1–6位置＋三轮raw速度 |
| RGB | `[[sensor]]`按`hardware_id`归属；左实例采头/左腕，右实例采右腕；独立`SensorAdapter`、BGR帧槽和topic，经SoftRT采集，状态/心跳优先；保留1秒帧龄/失流检查，尚不支持低于1Hz，时间戳为读取完成时刻 |
| 状态 | 保留`JointState`/`Heartbeat`；未标定速度和未测力矩为NaN，原寄存器值用于诊断及停止，NaN不作为静止证据 |

轮位唯一来源为[xlerobot_bus2_full.toml](../configs/xlerobot_bus2_full.toml)的同下标`joints`/`motor_ids`，当前**前8、左9、右7**，只支持既有ID7/8/9排列；校准数值按物理ID保留。改配置必须重建计划，旧GO失效，不在代码复制映射。

位置边界容差由各总线TOML的`calibration_range_tolerances_raw`逐项配置：当前位置关节20、三轮0，只扩展软件校准边界，仍限raw 0..4095；不改原校准或用clamp冒充保持。右6仅单程开合到位门为±5raw，保持及其他关节仍按原±4raw；停止判据不变。

## 原硬件合同

- raw经验命令仅覆盖 **Model777/FW3.10/Mode1/Phase12、`0/+500`**；无SI换算，不放行名义profile或其他电机/阶段。
- 三轮停止快照14读：首末`TE/GV=0`、`PV`仅0/±50；1秒五组`PP`跨度≤1且包含快照位置，总预算1.5秒，原字保留。SDK初始/断扭后仅遇±50时，同连接触发一次新14读；0走原路径，断扭后仍观察1秒，失败不重试。见[停止复核原合同](../.verify/sdk_wheel_stop_stability_2026-10-22_r1/summary.json)。
- 动作20秒从首次非零发送尝试起算，启用上限21秒、零速观察500毫秒；TE1零速前两组沿原0..1000及−50观察，末组仅0/±50。每帧来源及首帧冻结锚点期限均最多200毫秒，不续GO或21秒预算，不跨`MONOTONIC`/`MONOTONIC_RAW`混算。
- 沿原PLAN/GO、独占锁与停止入口；确认双latch持久化、子进程退出及锁归还后才做外层STOP51，manual失败状态保持粘性。左右组合GO均已消费，旧启动入口禁止重放。

## 保留的遗留事实

- 左1偶发根因仍为`UNKNOWN`；左腕`I=1`、头部`P=32`仅为临时已测候选。
- 右3历史`PP2992 > 原上限2991`仍是严格边界下的越界记录；后续配置容差不回写该结论。
- 右6合回`GP1397`、反馈到1402，未通过原800毫秒/±4要求的`FAIL`保留；用户随后接受5计数偏差，仅调整单程开合门，未重做该动作。开合仍仅限`GP1600/1397`、绝对增量≤256，GP写即动作起点，不改速度、PID或硬件限位。
- 历史混合失败、`NEEDS_MANUAL_POWER_CUT`、历史左9混合未目视的`NOT_OBSERVED`及相机黑帧/启动/回收失败均保留；失流夹具GV500残留仍判STOP51 FAIL。现场回执单独记录，原run/sender结果不回写；右臂逐拍raw未留存，不由前后位置相同推断全程偏差0。

## 下一步

1. **S0结案验收**：安装候选，按左、右顺序验证正式systemd启动、状态与图像输出、退出后的停止和资源释放。启动前通知并确认当轮现场条件；保持`auto_enable=false`，不迁移保护状态，启动仍会进行UART停止收敛写入。
2. **结案发布**：上述验收通过后，将根Cargo版本和锁文件中自有包版本从`0.1.2`统一为`0.0.1`；结案提交采用`v0.0.1+<7位提交SHA>`标签发布。配置矩阵打包纳入该版本；当前不提前改版或打tag，D15继续延期。
3. **下一阶段S1 scripted**：取得控制权→原位保持→停止释放，轮raw0；这条服务链的实机验收另行开展，不再追加为S0结案条件。

## 按需追溯

- 组合验收：[软件与准备](../.verify/s0_camera_combined_2026-11-10/summary.json)、[左侧实机](../.verify/s0_camera_combined_2026-11-10/left-hardware-summary.json)、[右侧实机](../.verify/s0_camera_combined_2026-11-10/right-hardware-summary.json)；保留原帧、现场回执与失败原件。
- 独有遗留：[右6合回原结果](../.verify/right_gripper_close_2026-11-10/summary.json)、[历史混合失败](../.verify/front_wheel_name_2026-11-05/hardware/mixed-summary.json)、[早期失败与观测错误](../.verify/foundation_errors_2026-11-06/summary.json)。
- 更早的软件统计、运行明细及源码入口见[精简前状态（abbb753e）](https://github.com/arxxyr/robot-system/blob/abbb753e/docs/s0_hal_status.md)及[历史索引](handoffs/README.md)，不另建归档副本。
