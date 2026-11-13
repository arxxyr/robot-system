# S0 HAL 当前状态

更新：2026-11-12。架构以[冻结设计](00_design.md)为准。

## 当前完成范围

通用机器人控制架构的 S0 阶段已完成首个 XLeRobot 后端的受监督 raw 最小闭环，初版为 **0.0.1**。**0.1.0 已在 Orin 安装并运行**，包括只读Web/WebSocket接入和RX交接修复。`D15=OPEN/DEFERRED`、`deployable=false`；不代表无人值守部署完成。下一步回到S1，不扩展S0。

- 左臂六关节＋头部两关节原位保持20秒，同时采集头部、左腕RGB；右臂六关节保持＋前轮ID8 raw `+500/20秒`，同时采集右腕RGB。两侧分别完成动作观察、停止及清场。
- 正式链路为 `robot-s0-preflight → guard → robot-s0`；两侧服务为 `robot-s0-bus1/bus2`。手动启停及此前真实重启后的双侧保护待机自启已验证，默认不上扭、故障保持锁存。
- 0.1.0按左→右→左→右完成四段各20秒可信读取：均为`fresh / Safe / TorqueOffConfirmed / fault_code=0`，源时间戳前进；实际CPU4/5、FIFO70、锁页通过。首次左右各沿原入口授权一次，正常退出0并保留下次RX授权；再次启动无需重复授权，运行时消费为1，EStop主备始终为1。
- 安装前及首次左右正常退出后的三次原整机停止检查均通过。部署、正常重启和Web验收已通过，本轮未上扭、未解除急停、未重启Orin；现场确认全程正常、无自行移动，目前整机静止。
- 0.0.1发布前已完成全量软件与打包验证；0.1.0修复和当前S1开发按变更范围验证，结果见下文。原失败不因后续通过而改写；测试结果、原帧和性能报告不提交Git。

## 分层与配置

| 部分 | 当前实现 |
|---|---|
| 上层接口与S1 | RPC网关属于上层：任务调用S1，状态/使能/急停直接接S0，不是S0启动依赖。S1 Idle/Scripted当前仅发送轮raw0；非零raw验收走维护入口 |
| S0 HAL | `robot-core`定义契约，`robot-s0`组织设备、分组和生命周期；每条总线一个daemon、串口所有者、SerialRT writer、命令流及停止域 |
| 硬件适配器 | 复用`ManipulatorAdapter`/`SerialRtAdapter`；左总线臂1–6＋头7/8位置模式，右总线臂1–6位置模式＋三轮raw速度 |
| RGB | 独立`SensorAdapter`与SoftRT采集；左实例采头/左腕，右实例采右腕。启动短暂失流清槽、WARN并等待恢复，运行期仍沿原1秒失流合同；尚不支持低于1Hz、深度或IMU |
| 状态 | 保留`JointState`/`Heartbeat`；未标定速度和未测力矩为NaN，不作为静止证据 |
| RT | CPU/FIFO来自总线TOML；严格绑核、锁页和`rt_guard`。定长命令槽、复用通讯缓冲；不据此宣称硬实时截止保证 |

轮位唯一来源为 [xlerobot-bus2.toml](../configs/xlerobot-bus2.toml) 同下标的`joints`/`motor_ids`：**前8、左9、右7**。校准按物理ID保留；改配置须重新生成计划，旧GO失效。

`calibration_range_tolerances_raw`逐电机配置：位置关节20、三轮0，只扩展软件校准边界，仍限raw 0..4095，不改原校准。右6单程开合到位门为±5raw；保持及其他关节仍按原±4raw，不用clamp伪装原位保持。

## 原硬件合同与未闭环项

- raw经验命令仅覆盖Model777/FW3.10/Mode1/Phase12、`0/+500`，未完成SI速度标定。
- 三轮停止复核：首末`TE/GV=0`、`PV`仅0/±50；固定1秒五组`PP`跨度≤1且包含快照位置，总预算1.5秒，原字保留。SDK初始/断扭后仅遇±50时，同连接触发一次复核；失败不重试。
- 动作20秒从首次非零发送尝试起算，启用上限21秒、零速观察500毫秒；TE1零速前两组沿原0..1000及−50观察，末组仅0/±50。来源与首帧锚点最多200毫秒，不续GO或动作预算。
- 沿原PLAN/GO、独占锁及停止入口；RX/EStop持久化、进程退出和锁归还后做STOP51。失败状态保持粘性，不删保护文件绕过。
- 无新授权时沿原逻辑进入`CommFault/BootUntrusted + AdapterUncertain`，保持Disabled/EStop锁存。停止时保留Dirty有界退出码1，实际静止由独立停止检查和现场观察确认。
- 左1偶发根因仍未知；左腕`I=1`、头部`P=32`只为临时已测候选。右6仅覆盖`GP1600/1397`、增量≤256，±5raw放宽后未追加合回复测。
- 相机启动恢复分支已有软件验证；实机复验未触发失流WARN，USB `-71`根因及长期稳定性仍未闭环。两总线同时动作、硬件失活fail-safe、深度/IMU及第二种后端不在本次完成范围。

## 安装、发布与下一步

正式0.1.0安装根为`/home/loosqk/robot-checks/deploy-0.1.0-20261112`，S0候选在`candidate`，两份网关在`status/candidate`，均从发布提交`bc5603f100e50b57b8e28a889a800bf1062821b5`原生构建。S0备份为`/var/lib/robot/deploy-backup-s0-0.1.0-bc5603f`，原状态层备份为安装根下`status/backup`。S0、网关、校准、保护目录及旧Web产物仍留原位；本次相机开发未更新master或发布标签。

相机Web单独更新到开发提交`209ad4ceb9393c8ad73148e254f529799992a337`的Orin原生release候选，部署根为`/home/loosqk/robot-checks/web-camera-preview-20261112/deploy-209ad4c`，当前unit引用其`candidate/bin/robot-status`及`candidate/configs/xlerobot-status.toml`。仅替换`robot-status.service`，复用原文件事务函数；660个编译输入与提交匹配，候选和旧unit完成备份、清单校验及PLAN_ONLY。旧Web unit位于该根`backup/robot-status.service`，仅恢复Web的入口为`robot-camera-rollback-web.sh`；脚本与其引用的原验证源码目录需保留。原三服务状态层回滚会涉及网关，不用于本次单Web回退。

两侧S0、两份网关和状态页均`active/running`、`NRestarts=0`。两侧S0保持`auto_enable=false`、EStop锁存和可信只读；S0三服务保持enabled，preflight为`active/exited`。三个上层服务尚未配置开机自启。本轮验证了正常停止后的再次启动，未追加整机重启。

发布标签为[v0.1.0+bc5603f](https://github.com/arxxyr/robot-system/tree/v0.1.0%2Bbc5603f)，包内提交、两个远程master及标签指向上述同一提交，已有标签保留。S0包SHA256为`696971bbd7243a4552795a5c51a46fb78a9b283c20ac0f7bd97f584e9ee485e4`；Web沿原独立入口部署，不混入S0包。此次GitHub Actions因账户付款或额度限制未启动，不能记作CI通过；Orin软件验证与打包已完成。后续开发仅推双远程dev。

## 通讯与控制TODO

方案见[对外通讯与控制](control_plan.md)。Web服务部署在Orin，浏览器使用WebSocket，内部复用现有gRPC，Nginx只作外网入口；接入管理交给用户已有的外部系统，项目Web不另加登录或访问token。有效关节反馈已恢复，下一步为S1的左侧最小控制闭环；控制与相机预览不追加为S0结案条件。后续实机仍先准备脚本和预检，再通知并确认现场。

- [x] **1. 状态链路（软件）**：现有`robot-rpc`提供可选独立程序`robot-status`，固定轮询原`GetState`并展示一页只读状态；身份、8/9关节及物理ID从原TOML派生。缺帧、陈旧、错配和扭矩未确认明确显示；三轮不伪装为位置关节。网关对齐bus1/bus2且不拉起S0/S1，没有控制按钮或动作路由。
- [x] **1.1 WebSocket只读软件**：推送改为`/api/ws`，移除Web令牌输入、Bearer校验和旧SSE入口，保留`/api/state`单次查询。沿同机gRPC、源时间戳及最新快照处理；连接、消息、慢写和退出有界。独立Web服务配置CPU0–3及CPU/内存/OS任务预算；S0/S1和内部控制RPC保持原实现。
- [x] **1.2 Orin真实状态部署与外网接入**：0.1.0的两侧S0及三个上层只读服务已运行；持续可信反馈、正常停止后的再次启动、Web资源限额和两个域名HTTPS/WSS均通过。页面保留匹配身份/layout的故障元数据，位置与源时间戳仍严格校验。默认不上扭、EStop保持；Nginx未改动。
- [ ] **2. 左侧S1与总线使能/断扭闭环**：bus1身份、8关节布局及服务依赖已对齐。启动锚点和常驻缓存已补强制layout及有限位置检查；现有`supervised_demo`新增仅左侧`hold`入口，固定使用本次S1锚点保持20秒、±4raw并严格确认断扭。Orin软件、模拟闭环和只读预检通过，执行候选位于`/home/loosqk/robot-checks/s1-bus1-hold-20261112`，待当次现场确认。尚未启动实机S1或使能；正式Web网关保持只读，清除急停独立处理，不把取消当断扭。
- [ ] **3. 左侧单关节定位**：补单关节请求语义，在S1提交时按同一新鲜快照补齐其余目标，复用scripted。校验模式、目标范围和时长；完成消息与实测到位分开，位置误差和稳定窗口须明确。取消、超时或失败后的回拉行为未明确处理并完成离线验证前，不开放实机位置动作。
- [ ] **4. 停止与异常收尾**：验证停用、急停、缺帧及超时，网络结果未知不自动重发；核对取消/失败返回启动锚点的现有行为，不将其包装为“停止当前位置”。必要语义变更先登记，保留原停止和锁存合同。
- [ ] **5. 右侧复用**：左侧通过后对齐bus2的9槽位，复用相同控制路径验证一个位置关节；三轮raw0，不开放轮位置控制、单电机使能或双总线同时动作。
- [x] **6. 低帧率相机预览**：独立Web进程从既有RGB观测镜像取得头部、左腕、右腕最新帧，归还共享内存后再编码JPEG；默认每路1Hz、320×240，间隔、尺寸、质量及陈旧阈值可配置。网页提供独立查看/暂停，慢端无逐帧队列，源时间不因重复收包续鲜，过期或错误隐藏图像；状态WebSocket保持独立。已仅更新Web并完成三路真实画面和外网验证，不再次打开相机或改采集配置；本轮30秒观测通过，长期USB稳定性及负载峰值仍未闭环。

相机预览开发使用独立worktree、隔离Orin源码与产物目录，nightly版本同下。fmt、全workspace/all-targets及`status-web`专项Clippy零警告；RPC串行334项通过/1项既有手动夹具忽略，包含9项新增相机测试和真实软件镜像→JPEG→HTTP/关闭回收。首次并行运行有1项原有subscriber生命周期测试在30秒超时，单独复查及串行全套通过，不回填首轮结果。Chrome模拟三路图像通过暂停、故障恢复、慢端单请求、重复源时间不续鲜及390px布局检查；安装脚本15项通过，本轮单Web事务另有10项离线检查通过。

真实画面30秒验证：三路各30/30次JPEG成功，源时间均前进29次、无回退；两总线各60次状态均fresh/Safe/TorqueOffConfirmed/故障0。S0 PID仍为`200279/200384`，网关为`200612/200632`，运行标识、重启数、unit及引用配置未变；仅Web换为PID`269237`，重启数0。Web采样最大内存7.53MiB、平均CPU约0.96%单核、4个OS任务，线程均在CPU0–3，无新增OOM事件，原半核/128–256MiB/32任务预算保持；这是观测窗口数据，不是长期峰值。Chrome经SSH转发和`robot.yang.cafe:30080`外网共核57帧，均320×240，17行状态继续更新，暂停/恢复及390px布局通过，业务API无失败。原帧和结果留在该部署根的`verify-live`及本机临时路径，不提交Git；Nginx未改动。

0.1.0软件验证使用Orin nightly `1.102.0-nightly (8eb778d04 2026-11-12)`：fmt、workspace和专项Clippy零警告；RPC 324项通过/1项忽略，S0库762项、程序13项、RX定向与daemon/mock回归通过。部署脚本沿现有安装器验证，实际本轮退出码0；完整报告和原帧只留在部署目录，不提交Git。

本轮S1在独立worktree及Orin隔离目录验证：fmt、全workspace/all-targets及保持工具两组特性Clippy零警告；S1共347项、工具79项、维护/模拟脚本13项通过。真实新S1与mock S0完成固定20秒保持、独立10帧断扭观察、重新锁存及全部进程退出；14项只读预检已生成PLAN_ONLY。资源回收测试修正join后立即读取`/proc`的观察竞态，保留256轮和30秒上限；编译负载下超时的原记录保留，编译结束后独立复验25.99秒通过，FD数量4→4。正式0.1.0产物未替换，实机动作仍待验。

两个外网入口：[robot.yang.cafe:30080](https://robot.yang.cafe:30080/)和[robot.yang.coffee:30080](https://robot.yang.coffee:30080/)，均使用同源`/api/ws`。本轮真实Chrome取得HTTPS 200、WSS 101，展示17行，两总线均为fresh、Safe、TorqueOffConfirmed、故障码0，源时间戳推进；未发送客户端业务消息，无页面异常。页面的`control_link/output_mode`仍为Unspecified，不据此声称控制接口已接入。Nginx未改动。

Web实际cgroup限制为CPU半核、MemoryHigh 128MiB、MemoryMax 256MiB、TasksMax 32，线程限制在CPU0–3，无OOM；网关同样绑定CPU0–3。升级状态层期间两侧S0 PID和重启次数不变。Web故障元数据与位置有效性分开，缺帧仍显示未知或陈旧。

正式S0模板启用`--resume-rx-on-clean-exit`：仅本轮可信、无故障、原关闭和断扭通过且设备线程全退出后，保留下次RX启动授权。维护入口默认不启用；Dirty、异常退出和真实故障仍走原恢复流程，不能自动清除。EStop独立保持，启动不会自动上扭。此次修复解决已确认的授权交接阻塞，不说明最早Dirty的成因或USB长期稳定性已解决。

后续硬件fail-safe、底盘SI标定、深度/IMU和第二种后端按需求分别推进，不作为本轮S1最小闭环的前置扩展。
