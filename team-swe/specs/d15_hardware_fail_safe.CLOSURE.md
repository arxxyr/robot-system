# 候选 E D15 硬件 fail-safe 闭环工作单

> 状态（2026-09-30）：**OPEN/DEFERRED，按用户决定暂缓实施与验收；非 Spec Approved，非硬件实现，非 PASS**。
> 代码证据基线：`59120e34bebd991accac41696425b1c743870a19`，2026-09-30 只读核对。
> 本工作单不修改冻结设计，不授权采购、接线、寄存器写入或硬件测试，不替代当次现场事实与独立 GO。
> 权威边界为[候选 E Spec](v0_5_dimos_replace.md) 的 D15 / D15-R 及其既有偏离登记；当前进度由 [HANDOFF](post_e_comm_fault_hardening.HANDOFF.md)统一维护。

用户确认当前**无独立自动断电保护**，决定“这部分可以记下暂时搁置，先通过”。
本工作单据此保留为后续恢复时的 CodeMap 与建议，不把搁置视作 AC-E25 通过或正式退役批准；
`deployable=false` 保持。受监督 demo 沿既有 D15-R 条件继续，下一步为右 H2 完整复测，原整体 FAIL 保留。
供电分域仍未知，本轮不再追问、选型、采购或接线；以下未知事实与方案建议留待恢复 D15 时处理。

## 1. 三条验收边界

| 项目 | 实际目标 | 当前边界 |
|---|---|---|
| 通信候选 H2 / AC-12 | S0 存活时，通信故障下抢占、收敛、真实 Disable 回执及完整收尾 | 右 H2 历史四组测量文件 PASS，但最终 Disable 失败，原 run/report 整体 FAIL 保留；后续超时修复及真实 tonic 回环仅为软件证据 |
| **候选 E D15 / AC-E25** | Enabled 真机 S0 被 `kill -9` 后，**<500 ms** 进入 torque disable / zero / brake | 正式退役硬门，OPEN/DEFERRED；H2 通过也不会自动关闭本项 |
| 通信规范 D15 | 观测面、mock transport、marker、notify、冻结与并发注入测试基线 | 与候选 E 的硬件 D15 同号不同义，不是进程死亡后的硬件保护证明 |

依据：[候选 E Spec](v0_5_dimos_replace.md) 的 AC-E25、D15 / D15-R，以及[通信 Spec](post_e_comm_fault_hardening.md) 的 D15「观测面与测试基线」。精确原文位置分别为该基线的候选 E Spec 第 766、903–995 行与通信 Spec 第 1595 行。

D15-R 已批内容是受监督 demo 例外及文档级集成契约：`arm / kick / request_safe / status+readback`；保护缺失、失联或读回失败应拒绝 Enabled。它没有选定外置硬件或独立监督进程，也没有批准假 no-op 实现。仓库既有能力核实结论是 STS3215 **无已文档化、可配置的 command watchdog**；历史失联持续带扭属于风险证据，不是本轮新测量。主机串口读超时不能解释为电机侧保护。

## 2. 当前代码证据

下表描述基线中真实存在的实现，尚无已验收的独立硬件保护接入证据。

| 符号 / 位置 | 已实现行为 | 对 D15 的限制 |
|---|---|---|
| [`ensure_safe_handoff` / `ensure_safe_arm`](../../crates/robot-s0/src/deploy/preflight.rs#L205) | 启动前安全写、逐项 readback；双总线覆盖 17 电机、20 条写 | 一次性串口事务，需要主机和通信链存活 |
| [`evaluate_guard`](../../crates/robot-s0/src/deploy/guard.rs#L415) | 身份校验、稳定锁、失败计数、本臂 preflight、latch provision，成功后 `exec s0_daemon` | guard 被 S0 替换，不留下独立保护进程 |
| [`verify_arm_config` 中的 D2 身份检查](../../crates/robot-s0/src/deploy/identity.rs#L263) | 必须显式 `auto_enable=false` | 防止重启自动上扭，不证明死亡时断扭 |
| [`IpcRuntime::start_enable`](../../crates/robot-s0/src/ipc_runtime.rs#L1027) / [`start_enable_owned`](../../crates/robot-s0/src/ipc_pending.rs#L1030) | 检查期限、业务 guard、pending、心跳等，再预约真实 Enable | 没有外部保护 armed、readback、代次及新鲜度门 |
| [`ControlAuthority::reserve_enable`](../../crates/robot-core/src/control_authority.rs#L1382) / [`commit_enable`](../../crates/robot-core/src/control_authority.rs#L1499) | 检查 Disabled、latch、shutdown、inhibit、代次和真实 TorqueOn；迟到成功不能覆盖新保护 | 现有权限条件不能冒充外部硬件能力 |
| [`Guards` 启动赋值](../../crates/robot-s0/src/bin/s0_daemon.rs#L1632) / [`ready_guards_pass`](../../crates/robot-s0/src/state_machine.rs#L196) | **`power_ok` 当前直接设为 `true`**，参与 Ready gate | 不是外部电源或保护装置 readback，不能用它宣称 D15 已有安全 gate |
| [`FeetechAdapter::enable` / `safety_stop`](../../crates/robot-adapters/feetech/src/adapter.rs#L640) | Goal=Present/readback 后上扭；紧停广播 Torque_Enable=0，另由读回确认 | 软件下发，不是 S0 死亡后自动动作的硬件机制 |

两套现有 watchdog 必须分清：

- [`robot_s0::watchdog::spawn_watchdog`](../../crates/robot-s0/src/watchdog.rs#L29)：同进程监视 **S1 心跳**，默认 200 ms 超时、10 ms 轮询，发 `HeartbeatTimeout` 给 IPC。
- [`s0_systemd_wd`](../../crates/robot-s0/src/bin/s0_daemon.rs#L1946)：同进程线程观察 IPC、executor、pub、io 四源；SerialRT 读 [`serial_liveness`](../../crates/robot-s0/src/hard_rt_ctrl.rs#L328)。全部推进才 `sd_notify(WATCHDOG=1)`。[算法](../../crates/robot-health/src/systemd.rs#L267)取 `WATCHDOG_USEC/2`，该源码基线的双臂 S0 unit 的 `WatchdogSec=2s`，故正常喂狗周期为 **1 s**；`RestartSec=2s` 是恢复配置。不能直接复用为 <500 ms 硬件保护节拍。

现有关闭链也不能覆盖 S0 死亡：[信号入口](../../crates/robot-s0/src/bin/s0_daemon.rs#L87)只处理 SIGINT/SIGTERM，[`finish_runtime_owned`](../../crates/robot-s0/src/bin/s0_daemon.rs#L1347)依赖仍存活的线程完成 ACK、持久化和 join；[`BootSupervisor`](../../crates/robot-s0/src/boot_supervisor.rs#L1)也是同进程线程，最终可 `_exit`，不是外部断电装置。SIGKILL 后这些路径都不再执行。[`freeze_systemd_smoke`](../../crates/robot-tests/src/bin/freeze_systemd_smoke.rs#L3)验证 mock worker 停滞触发 systemd 重启，没有测量真机安全输出。

## 3. 恢复 D15 时再确认的硬件事实

用户已确认无独立自动断电保护；以下细节仍未知，待恢复 D15 时再从现场或可追溯硬件资料确认，
本轮不追加询问或方案选型，不从历史日志或软件配置推断：

| 事实 | 必须得到的内容 |
|---|---|
| 电源域 | 双臂、夹爪、头部、底盘分别由哪些电源轨供电；Orin 与电机是否共电；一个保护输出实际覆盖哪些电机 |
| 硬件型号 | 电源、驱动、电机及候选保护装置的准确型号、硬件版本、固件版本、厂家资料；不把历史典型参数当成当前能力 |
| 接线与默认态 | 供电路径、控制线、反馈线、共地或隔离关系；断线、未供电、上电及复位时输出默认状态；保护装置是否依赖 Orin 供电 |
| 安全输出 | 实际采取断电、零输出、断扭或制动中的哪一种；失重下坠、惯性滑行、制动释放等机械后果及支撑措施 |
| 验证能力 | 能独立观测的电压、电流、使能反馈或制动状态；测量设备、时基、采样精度及故障起点观测方式 |

据此选择方案并确认电源覆盖、故障模型及安全等级。历史 (b) 外置硬件 watchdog 与 (c2) 独立软件监督方案的讨论不是采购或实施批准；后者依赖主机与串口，不能自动等同硬件级保护，须按原 Spec 处理偏离。

## 4. 未来接入位置建议

本节是建议，不是已有实现或已批准 API。具体模块、协议和资源所有权待硬件事实确认后确定。

1. **启动与交接**：在现有 handoff/guard 确保安全之后、允许首次 Enable 之前，核保护装置身份、arm 结果与真实 readback；失败不得继续上扭。必须明确 guard→S0 交接期间谁保持保护，以及两臂共享电源域时的责任。
2. **Enable 前与提交前**：在 `IpcRuntime::start_enable` 预约前核有效保护状态，并在真实 Enable 提交前复核代次与新鲜度。失效须沿既有 authority/Lower 责任链撤销，防止在途迟到成功重新放行；不能只增加启动时布尔值或借用恒真的 `power_ok`。
3. **运行期 kick**：由已选定的保护协议决定执行位置与周期，受实际控制进展约束；线程空转不得无限续命。不能直接沿用现有 1 s systemd 喂狗，也不能给 HardRT 线程加入文件、网络或分配操作。
4. **停止、故障与重启**：`request_safe` 和实际安全输出 readback 分开记录；保持“未确认即未知”。重启或保护装置复位后不得自动恢复危险输出，必须重新满足保护 gate 与原显式 Enable 条件。

## 5. 分阶段产出与验收

| 阶段 | 必须产出 | 可宣称边界 |
|---|---|---|
| 硬件事实 | 型号/固件、接线及电源域图、资料来源、安全输出及独立测量方案 | 事实确认，不是功能通过 |
| 方案决策 | 保护故障模型、端到端预算、所有权与交接、Enable gate、失效处理、机械风险及回滚；必要的偏离登记 | 方案与验收范围明确并完成独审后成为实施依据 |
| 软件实现 | 真实硬件实现；缺失/过期/错误 readback、在途撤销、重启及资源回收的确定性反例；原控制回归 | 仅软件通过，不预写硬件成功 |
| 隔离台架 | 当次事实与独立 GO；保护装置配置/readback；机械隔离下单次原始故障与安全输出证据 | 仅覆盖实际装置、电源域、条件和场景 |
| 系统验收与退役审查 | 原 AC-E25 结果、另行批准的扩充场景、证据复核、遗留项和完整回滚；明确撤销 demo 例外的条件 | D15 与正式退役分别裁决；H2、D15 均不得互相代替 |

**既批门：**候选 E AC-E25 的 Enabled 真机 `kill -9 s0_daemon`→实际安全态 **<500 ms**。软件发信号完成、进程退出、systemd 重启、串口不可达或主机 Ack，均不能单独证明实际安全输出已经完成；须在已批准测量方案中定义终点与适用范围。

**建议扩充矩阵，全部待另行批准：**

| 建议场景 | 需要证明的性质 |
|---|---|
| SIGSTOP / 控制 worker 卡死，但进程仍存在 | 无有效控制进展时不会继续维持危险输出 |
| kick/反馈线断开、保护通信失联 | 保护链自身失效能进入所定义安全态；不能沿用缓存健康值 |
| 保护装置断电、复位或反馈异常 | 默认输出安全，未知状态阻止 Enable |
| Orin 掉电或系统整体停摆 | 不依赖 Orin 存活仍能执行安全动作 |
| 单臂故障与共享电源域 | 实际受影响电机全集和另一臂行为符合已批准拓扑 |
| S0 / 保护装置重启与恢复供电 | 不自动重上扭，不接受旧代次或过期的健康回执 |

不能把上述建议写成原 AC-E25 已批准的覆盖，也不能预填通过、时延或重试次数。

## 6. 每次硬件测量的最小原始证据

每次使用新 run 标识与独占目录，保留失败原件；一次失败不自动重授权或重测。记录以下字段，未取得的值明确为 UNKNOWN / NOT_OBSERVED，不填估计成功值：

- 来源：源码提交、产物 SHA、配置与保护参数 SHA、测试命令、操作者与当次授权引用。
- 硬件：型号、序列号或可辨身份、固件、实际接线版本、电源域、受保护电机清单。
- 条件：负载、机械隔离/支撑、接触区、现场持续值守及独立断电手段的当次事实。
- 故障：具体注入方式、目标进程身份及实际故障发生/被观测时刻；命令发送时刻另列，不混为实际起点。
- 安全态：已定义安全输出、独立传感或测量通道、首次满足安全条件的观测时刻及持续状态；通信失败另列。
- 时间证据：各时间戳所属时基、同步方法、采样率/分辨率、精度和不确定度、原始波形或逐样本数据。若精度不足以判断 <500 ms，结论不能为 PASS。
- 原件：设备读回、journal/串口/测量日志、信号与真实进程回收记录、前后状态、原文件 SHA 清单及未执行项。
- 结论：实测间隔及计算方法、对应已批判据、PASS/FAIL/INCONCLUSIVE、覆盖范围与未覆盖故障；不得以聚合摘要覆盖失败原件。

本工作单完成不改变 **D15 OPEN/DEFERRED、右 H2 历史整体 FAIL、deployable=false**。硬件事实、方案批准、实现、软件验证、真机验收和正式退役是不同交付状态。
