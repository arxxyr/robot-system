# S0 HAL 当前状态

更新：2026-11-12（UTC）。架构以[冻结设计](00_design.md)为准。

**通用机器人控制架构的 S0 阶段：首个 XLeRobot 后端已通过受监督 raw 最小闭环。** 默认S0维护入口分别完成：左臂六关节＋头部两关节保持20秒并采头/左腕RGB；右臂六关节保持＋前轮ID8 raw `+500/20秒`并采右腕RGB。三路画面和双腕映射已确认，两侧动作观察、断扭、EStop持久化、原STOP51、位置/身份及清场检查均通过，`manual=false`。

**S0受监督功能、systemd手动启停及真实重启后的双侧保护待机自启均已通过，软件版本为0.0.1。D15为`OPEN/DEFERRED`、`deployable=false`。** 动作验收仍为左右分别执行；此前重启验收中的双侧同时启动仅为不上扭故障待机，不覆盖两总线同时动作、无人值守、深度/IMU或标定后的SI速度。

**相机修复候选`3a6d96e`已安装并通过左后右首次启动复验。** 两侧各观察20秒、`NRestarts=0`，头部301帧、左腕101帧、右腕102帧，像素、时间推进及首末画面核对通过；实际通讯线程CPU4/5、FIFO70、锁页保持。旧实例停止、左右复验收尾的17电机STOP51与三轮位置复核均通过；现场确认无自行移动、结束静止。原件见[本轮验收](evidence/s0_camera_candidate_2026-11-12/acceptance/summary.json)。

启动短暂失流按WARN清槽并保留采集器恢复，原启动期限与运行期失流合同不变；[43项软件测试和旧版负控](evidence/s0_camera_startup_2026-11-12/software-summary.json)已验证恢复分支。本次实机没有触发失流WARN，因此只证明首次启动成功，不能宣称USB `-71`根因或长时稳定性已解决。[原失败事件](evidence/s0_camera_startup_2026-11-12/index.json)及[安装准备证据](evidence/s0_camera_candidate_2026-11-12/summary.json)保留。

**最新代码候选`dcd781e`已通过Orin全量软件及打包验证，尚未安装或实机复验。** 全workspace/all-features为3059通过、23 ignored；另行补测停止传输6项、观测器6项、RT soak 1项通过，剩余10项为文档示例、诊断及子进程夹具。三组构建矩阵、release、依赖检查、453项Python测试（零跳过）和独立包校验通过。测试修复仅涉及子进程证据标记及RT夹具预分配；伪终端补测使用同源ELF独立副本，未放宽原单链接检查。原失败保留在[软件与候选证据](evidence/s0_dev_full_2026-11-12/summary.json)。包为`v0.0.1+dcd781e`候选，不是新发布标签；正式发布仍需完成本轮左后右启动复验，包、发布提交与标签一致，不能沿用旧ELF的实机结论。

正式链路为`robot-s0-preflight → guard → robot-s0`，两侧服务为`robot-s0-bus1/bus2`，配置为`xlerobot-bus1/bus2.toml`。此前实机验收候选从`f9e4b92`构建，启用`xlerobot-real,rt_guard`；左侧保持原验收，右侧仅替换为`776692a`的观测器后复验通过。两侧各观察20秒，保持`auto_enable=false`和双EStop锁存；实际通讯线程分别CPU4/5、FIFO70，其余线程CPU0–3、普通调度，进程锁页已核对。

| 既有手动启动验收 | 状态/心跳 | RGB | 停止与清场 |
|---|---|---|---|
| 左侧 | 各987条，零拒帧 | 头部301、左腕101帧 | 原STOP51、三轮位置稳定性及33项资源检查通过 |
| 右侧复验 | 状态989、心跳990条，零拒帧 | 右腕102帧 | 原STOP51、三轮位置稳定性及33项资源检查通过 |

录制已解码并目视首末画面；左侧头部视角变化由用户确认为现场调整。右侧旧报告因观测器拒收NaN保持FAIL，不回填；修复仅允许状态速度/力矩中的NaN表示未知，不补零、不改S0或停止判据。新原件见[右侧复验](evidence/s0_right_recheck_2026-11-12/summary.json)，左侧及旧右失败见[原启动验收](evidence/s0_rt_prepare_2026-11-12/summary.json)。该轮结束后三服务均inactive/dead、PID0、disabled，相关进程、相机、UART及锁已清场；旧GO已消费，禁止重放。当前自启状态见下文。

结案版本仅将根Cargo及16个自有包锁版本由0.1.2统一为0.0.1，配置矩阵按现有入口构建；标签为`v0.0.1+<7位结案SHA>`。结案包单独验证，不自动替换已验安装件；不能把原候选的实机结果写成新ELF已运行。回滚继续保留`/var/lib/robot/deploy-backup-s0-rt-f9e4b92`及其冻结安装器，不回滚保护状态。

## 已有边界

| 层/部分 | 当前能力与限制 |
|---|---|
| 上层接口与 S1 | RPC网关属于上层接口，调用S1；不属于S0，也不是S0启动依赖。S1沿既有控制权与命令接口，Idle/Scripted当前仅发送轮raw0，非零raw验收使用维护入口 |
| S0 HAL | `robot-core`定义契约，`robot-s0`组织设备、分组与生命周期；两个daemon各管一条总线，每条总线唯一串口所有者、SerialRT writer、命令流、控制权及停止域 |
| 硬件适配器 | 沿既有`ManipulatorAdapter`/`SerialRtAdapter`封装Feetech协议；左总线臂1–6＋头7/8位置分组，右总线臂1–6位置＋三轮raw速度 |
| RGB | `[[sensor]]`按`hardware_id`归属；左实例采头/左腕，右实例采右腕；独立`SensorAdapter`、BGR帧槽和topic，经SoftRT采集，状态/心跳优先；保留1秒帧龄，dev在启动短暂失流时清槽并等待恢复，运行期仍按原失流回收；尚不支持低于1Hz，时间戳为读取完成时刻 |
| 状态 | 保留`JointState`/`Heartbeat`；未标定速度和未测力矩为NaN，原寄存器值用于诊断及停止，NaN不作为静止证据 |
| 调度与内存 | 两份总线TOML分别配置CPU4/5、FIFO70，配置即要求严格绑核、锁页和`rt_guard`；命令槽有界取定长值，通讯帧缓冲复用，SerialRT首次停止、连接、周期及错误路径覆盖分配守卫。Orin软件验证及本轮实际服务前后CPU/FIFO/VmLck读取通过；未据此证明硬实时截止保证 |

S0↔S1主线发布已完成软件优化：状态/命令/心跳移交body，S0去掉三个临时数组，S1混合保持在编码时清零轮位，Scripted驻留复用目标，试算帧头只取长度；通用发布层将帧头、正文直接分段写入SHM，不再分配完整wire数组。观测队列接管原缓冲、按容量计预算，镜像仍写入独立SHM。原消息字节、失败边界、跨锁目标快照和独立状态缓存保留；未宣称端到端零拷贝或时延提升。见[小复制收尾验证](evidence/publish_small_copies_2026-11-12/software-summary.json)、[分段发布验证](evidence/publish_wire_parts_2026-11-11/software-summary.json)及[状态/发布验证](evidence/publish_owned_state_2026-11-12/software-summary.json)。RPC缓存共享快照属于旁路，保留[原验证](evidence/s1_rpc_copy_2026-11-12/software-summary.json)；本轮未修改RPC、相机或RT命令槽代码，未安装或启动实机。

轮位唯一来源为[xlerobot-bus2.toml](../configs/xlerobot-bus2.toml)的同下标`joints`/`motor_ids`，当前**前8、左9、右7**，只支持既有ID7/8/9排列；校准数值按物理ID保留。改配置必须重建计划，旧GO失效，不在代码复制映射。

位置边界容差由各总线TOML的`calibration_range_tolerances_raw`逐项配置：当前位置关节20、三轮0，只扩展软件校准边界，仍限raw 0..4095；不改原校准或用clamp冒充保持。右6仅单程开合到位门为±5raw，保持及其他关节仍按原±4raw；停止判据不变。

## 原硬件合同

- raw经验命令仅覆盖 **Model777/FW3.10/Mode1/Phase12、`0/+500`**；无SI换算，不放行名义profile或其他电机/阶段。
- 三轮停止快照14读：首末`TE/GV=0`、`PV`仅0/±50；1秒五组`PP`跨度≤1且包含快照位置，总预算1.5秒，原字保留。SDK初始/断扭后仅遇±50时，同连接触发一次新14读；0走原路径，断扭后仍观察1秒，失败不重试。见[停止复核原合同](evidence/sdk_wheel_stop_stability_2026-10-22_r1/summary.json)。
- 动作20秒从首次非零发送尝试起算，启用上限21秒、零速观察500毫秒；TE1零速前两组沿原0..1000及−50观察，末组仅0/±50。每帧来源及首帧冻结锚点期限均最多200毫秒，不续GO或21秒预算，不跨`MONOTONIC`/`MONOTONIC_RAW`混算。
- 沿原PLAN/GO、独占锁与停止入口；确认双latch持久化、子进程退出及锁归还后才做外层STOP51，manual失败状态保持粘性。左右组合GO均已消费，旧启动入口禁止重放。

## 未闭环项

- 左1偶发根因仍为`UNKNOWN`；左腕`I=1`、头部`P=32`仅为临时已测候选。
- 右6单程开合仅限`GP1600/1397`、绝对增量≤256，GP写即动作起点；放宽至±5raw后尚未重做合回动作，不改速度、PID或硬件限位。
- D15硬件fail-safe及下列roadmap尚未完成；历史失败和未观察结果保留在原件，不被新容差或后续通过覆盖。
- 开机默认保护待机：已消费的RX授权保持Dirty；无新授权时沿原逻辑进入`CommFault/BootUntrusted + AdapterUncertain`，保持Disabled和EStop锁存。该状态允许服务常驻，但不代表healthy Safe、可信断扭或正常关节采样；停止时有界退出码1仍保留，物理停止由原独立STOP51复核。自启验收不额外要求无故障，也不自动续授或放宽停止判据。

## Roadmap

1. **S0当前安装与收尾**：已安装根为`/home/loosqk/robot-checks/s0-camera-candidate-20261112`，本次升级备份为`/var/lib/robot/deploy-backup-s0-camera-3a6d96e`。三服务保持enabled；两侧S0已停止、PID0，保留Dirty停止的`failed/exit-code 1`，preflight保持`active/exited`。双份RX/EStop均为1，进程、UART、相机和四锁均已清场，旧GO已消费、看护已结束。本次未重启Orin；此前`df6c1bb`的[真实重启验收](evidence/s0_startup_deadline_2026-11-12/reboot-observe/report.json)与本次新候选首次启动分别记账。

   保留原升级及自启备份，不回滚保护状态。USB断帧根因与长期稳定性仍待实际复现诊断；不为此自动重测。原[启动失败](evidence/s0_autostart_2026-11-12_r2/reboot-observe/report.json)、日志与原帧保留在[证据选集](evidence/README.md)及Orin原路径。日常仅同步dev；正式发布时包内提交、master与新SemVer标签一致，既有标签不覆盖。

   最新`dcd781e`候选已在`/home/loosqk/robot-checks/s0-dev-full-20261112`准备，43项入口假测试及67项只读预检通过，未安装、未启动或打开设备。下一步待本轮现场确认后，复核原STOP51并沿既有安装器备份安装，左后右各20秒保护待机复验；保持不上扭、故障锁存和原停止检查，不重跑handoff或重启Orin。回滚脚本仍引用上一安装件，备份目标为`/var/lib/robot/deploy-backup-s0-dev-dcd781e`，该备份尚未创建。
2. **下一阶段S1 scripted**：取得控制权→原位保持→停止释放，轮raw0；这条服务链的实机验收另行开展，不再追加为S0结案条件。
3. **后续按实际需求推进**：硬件fail-safe、底盘SI标定、深度/IMU及第二种硬件后端；分别立验证目标，不提前扩展RPC、插件或新框架。

## 按需追溯

- 组合验收：[软件与准备](evidence/s0_camera_combined_2026-11-10/summary.json)、[左侧实机](evidence/s0_camera_combined_2026-11-10/left-hardware-summary.json)、[右侧实机](evidence/s0_camera_combined_2026-11-10/right-hardware-summary.json)；保留原帧、现场回执与失败原件。
- 独有遗留：[右6合回原结果](evidence/right_gripper_close_2026-11-10/summary.json)、[历史混合失败](evidence/front_wheel_name_2026-11-05/hardware/mixed-summary.json)、[早期失败与观测错误](evidence/foundation_errors_2026-11-06/summary.json)。
- 旧交接、阶段计划和重复清单从工作树删除，历史按需查Git，不另建归档副本。
