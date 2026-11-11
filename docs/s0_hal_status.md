# S0 HAL 当前状态

更新：2026-11-11（America/Los_Angeles；实机验收为 UTC 2026-11-11）。架构以[冻结设计](00_design.md)为准。

**通用机器人控制架构的 S0 阶段：首个 XLeRobot 后端已通过受监督 raw 最小闭环。** 默认S0维护入口分别完成：左臂六关节＋头部两关节保持20秒并采头/左腕RGB；右臂六关节保持＋前轮ID8 raw `+500/20秒`并采右腕RGB。三路画面和双腕映射已确认，两侧动作观察、断扭、EStop持久化、原STOP51、位置/身份及清场检查均通过，`manual=false`。

**D15 `OPEN/DEFERRED`，`deployable=false`；实时调度与零分配的软件验证已通过，RT候选已安装，43项PLAN_ONLY和启动脚本就绪，正式启动等待当轮现场确认。** 已有实机验收为左右分别执行，不覆盖两总线同时联动、无人值守、深度/IMU或标定后的SI速度。

当前链路为`robot-s0-preflight → guard → robot-s0`，两侧服务为`robot-s0-bus1/bus2`，配置为`xlerobot-bus1/bus2.toml`。候选从`f9e4b92`构建，启用`xlerobot-real,rt_guard`，使用Rerun 0.43.0；软件验证、六路径备份、安装、reload和只读复核通过。三份旧unit已撤，新三份均为inactive/dead、PID0、disabled；12个保护文件未变，没有启动服务或访问UART。新预检不继承旧handoff完成态。

RT候选的回滚使用`/var/lib/robot/deploy-backup-s0-rt-f9e4b92`及其冻结安装器，不回滚保护状态。准备、软件与安装原件见[当前准备记录](../.verify/s0_rt_prepare_2026-11-12/summary.json)；本轮计划SHA为`7bf0addb…52f0b`，尚未生成GO。旧普通候选的计划不再使用，所需回滚原件保留。

## 已有边界

| 层/部分 | 当前能力与限制 |
|---|---|
| 上层接口与 S1 | RPC网关属于上层接口，调用S1；不属于S0，也不是S0启动依赖。S1沿既有控制权与命令接口，Idle/Scripted当前仅发送轮raw0，非零raw验收使用维护入口 |
| S0 HAL | `robot-core`定义契约，`robot-s0`组织设备、分组与生命周期；两个daemon各管一条总线，每条总线唯一串口所有者、SerialRT writer、命令流、控制权及停止域 |
| 硬件适配器 | 沿既有`ManipulatorAdapter`/`SerialRtAdapter`封装Feetech协议；左总线臂1–6＋头7/8位置分组，右总线臂1–6位置＋三轮raw速度 |
| RGB | `[[sensor]]`按`hardware_id`归属；左实例采头/左腕，右实例采右腕；独立`SensorAdapter`、BGR帧槽和topic，经SoftRT采集，状态/心跳优先；保留1秒帧龄/失流检查，尚不支持低于1Hz，时间戳为读取完成时刻 |
| 状态 | 保留`JointState`/`Heartbeat`；未标定速度和未测力矩为NaN，原寄存器值用于诊断及停止，NaN不作为静止证据 |
| 调度与内存 | 两份总线TOML分别配置CPU4/5、FIFO70，配置即要求严格绑核、锁页和`rt_guard`；命令槽有界取定长值，通讯帧缓冲复用，SerialRT首次停止、连接、周期及错误路径覆盖分配守卫。Orin软件验证通过；实际服务的CPU/FIFO/VmLck仍待正式启动读取，未据此证明硬实时截止保证 |

轮位唯一来源为[xlerobot-bus2.toml](../configs/xlerobot-bus2.toml)的同下标`joints`/`motor_ids`，当前**前8、左9、右7**，只支持既有ID7/8/9排列；校准数值按物理ID保留。改配置必须重建计划，旧GO失效，不在代码复制映射。

位置边界容差由各总线TOML的`calibration_range_tolerances_raw`逐项配置：当前位置关节20、三轮0，只扩展软件校准边界，仍限raw 0..4095；不改原校准或用clamp冒充保持。右6仅单程开合到位门为±5raw，保持及其他关节仍按原±4raw；停止判据不变。

## 原硬件合同

- raw经验命令仅覆盖 **Model777/FW3.10/Mode1/Phase12、`0/+500`**；无SI换算，不放行名义profile或其他电机/阶段。
- 三轮停止快照14读：首末`TE/GV=0`、`PV`仅0/±50；1秒五组`PP`跨度≤1且包含快照位置，总预算1.5秒，原字保留。SDK初始/断扭后仅遇±50时，同连接触发一次新14读；0走原路径，断扭后仍观察1秒，失败不重试。见[停止复核原合同](../.verify/sdk_wheel_stop_stability_2026-10-22_r1/summary.json)。
- 动作20秒从首次非零发送尝试起算，启用上限21秒、零速观察500毫秒；TE1零速前两组沿原0..1000及−50观察，末组仅0/±50。每帧来源及首帧冻结锚点期限均最多200毫秒，不续GO或21秒预算，不跨`MONOTONIC`/`MONOTONIC_RAW`混算。
- 沿原PLAN/GO、独占锁与停止入口；确认双latch持久化、子进程退出及锁归还后才做外层STOP51，manual失败状态保持粘性。左右组合GO均已消费，旧启动入口禁止重放。

## 未闭环项

- 左1偶发根因仍为`UNKNOWN`；左腕`I=1`、头部`P=32`仅为临时已测候选。
- 右6单程开合仅限`GP1600/1397`、绝对增量≤256，GP写即动作起点；放宽至±5raw后尚未重做合回动作，不改速度、PID或硬件限位。
- 新RT配置、产物和unit已冻结安装；实际线程属性与完整服务生命周期尚待现场验收，安装状态不能替代运行证据。
- 正式服务启动验收、D15硬件fail-safe及下列roadmap尚未完成；历史失败和未观察结果保留在原件，不被新容差或后续通过覆盖。

## Roadmap

1. **S0结案验收**：使用已准备脚本，按左、右顺序验证正式systemd启动、实际线程属性、状态与图像输出、退出后的停止和资源释放。启动前通知并确认当轮现场条件；保持`auto_enable=false`和EStop锁存，不启用关节或轮子；启动仍会进行UART停止收敛写入。
2. **结案发布**：上述验收通过后，将根Cargo版本和锁文件中自有包版本从`0.1.2`统一为`0.0.1`；结案提交采用`v0.0.1+<7位提交SHA>`标签发布。配置矩阵打包纳入该版本；当前不提前改版或打tag，D15继续延期。
3. **下一阶段S1 scripted**：取得控制权→原位保持→停止释放，轮raw0；这条服务链的实机验收另行开展，不再追加为S0结案条件。
4. **后续按实际需求推进**：硬件fail-safe、底盘SI标定、深度/IMU及第二种硬件后端；分别立验证目标，不提前扩展RPC、插件或新框架。

## 按需追溯

- 组合验收：[软件与准备](../.verify/s0_camera_combined_2026-11-10/summary.json)、[左侧实机](../.verify/s0_camera_combined_2026-11-10/left-hardware-summary.json)、[右侧实机](../.verify/s0_camera_combined_2026-11-10/right-hardware-summary.json)；保留原帧、现场回执与失败原件。
- 独有遗留：[右6合回原结果](../.verify/right_gripper_close_2026-11-10/summary.json)、[历史混合失败](../.verify/front_wheel_name_2026-11-05/hardware/mixed-summary.json)、[早期失败与观测错误](../.verify/foundation_errors_2026-11-06/summary.json)。
- 旧交接、阶段计划和重复清单从工作树删除，历史按需查Git，不另建归档副本。
