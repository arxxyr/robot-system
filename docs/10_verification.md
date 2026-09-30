# 10 · 验证与测试（活文档）

> `docs/00_design.md` §10 的落地版本。历史表格保留各阶段的实测与待验证标签；
> 当前 post-E 候选按下方验收入口推进，不以历史 PASS 替代本轮结果。
>
> - 权威语义来源：`docs/00_design.md` §10 / §6 / §4.9。
> - 偏离 / 调整语义走 `docs/CHANGELOG.md`，不在本文件改。
> - 本文件每次更新 → 在底部《变更记录》追加一行。

---

## 2026-09-30 · Orin 原生 Rerun 软件验证（已验收）

严格 SSH 已恢复；在独立目录验证 `a6957cc` 的 620 项源码快照，未替换正式 ca14 安装件。
工具链使用更新后的浮动 `nightly`，实际为 `2026-09-30 / rustc 1.101.0-nightly (5c543b0b8)`；
日期只作本次溯源，不默认固定仓库工具链。

已通过 fmt、根 Clippy、17 项 viz 单测、9 项镜像测试（另 1 项 ignored 子夹具不计通过）、
6 项显式 SHM 集成，以及 `release-tools` 独立优化构建和 help。
独立构建与首轮六项所用的集成桥 SHA 不同；原测试可执行文件已针对最终桥
`0097228bb6cf42c8f580752620837017c6e79e65e5f0160c0817b95d087b90eb`
补跑同六项，严格 `6 passed / 0 failed / 0 ignored`，直接 child 实际 wait=0/reaped，
源码、进程边界与测试/桥产物前后 SHA 均核验通过。合计 32 项不同测试加同六项复验，
不是 38 项不同测试。最终桥正常记录 1000 帧、10 类、11 实例，gap/drop/reject 均为 0，
官方同版 RRD verify 报告 `1 file verified without error`。本轮 SIGINT 夹具报告 0 帧，
通过只证明其生命周期收尾；下节历史 80 帧实时 GUI 结果不计入本次 Orin 验收。
Orin 定向软件已验收，独审与 root 验收通过。
正式快照比对通过：ca14 源 clean、四安装件 SHA 未变、六服务 inactive/dead/MainPID0，
未发现控制进程；这不推断物理电源、扭矩或支撑状态。
本轮没有重跑完整 workspace、Lima 的 609 项 RT 库测试或 13 项退出专项，
不将下节 9/29 的结果归入本次 Orin 实测。

本轮[摘要](../.verify/robot_viz_orin_2026-09-30/summary.json)与
[原件包](../.verify/robot_viz_orin_2026-09-30/evidence.tar.gz)已归档。
未执行新硬件动作或正式部署；右 H2 整体 FAIL、D15 OPEN、`deployable=false` 不变，
历史失败保留。

## 2026-09-29 · Rerun 观测桥（软件验收完成）

独立 `robot-viz` 已实现，正常录制与真实 Viewer 文件/实时画面已验证，最终 Linux 根回归通过。
完成状态以本节和[证据摘要](../.verify/robot_viz_2026-09-29/summary.json)为准。
范围是十类 pub/sub 消息的软件观测；右 H2、D15、正式安装和退役保持原状态。

| 验收项 | 实现与实测证据 | 当前状态 |
| --- | --- | --- |
| V1 独立执行 | 独立二进制、mimalloc、`release-tools` unwind；不依赖 S0/S1/adapter，不访问串口或下发控制命令 | 源码与依赖审查、Linux ARM 优化构建通过 |
| V2 观测隔离 | 主发布成功后非阻塞送入有界镜像；原 QoS、结果和消费者计数保持；真实 SHM 测试覆盖慢/暂停观察者与 worker 回收 | macOS/Linux 专项通过；不宣称抵抗任意内核/SDK 永久卡死 |
| V3 pub/sub 覆盖 | JointLayout/State/Command、Twist、Image/PointCloud/Tensor Header、Heartbeat、EStopEvent、ActionEventEnvelope 十类均进入真实 RRD | 两平台六项显式集成通过；注册表中 RR 与嵌套 schema 不冒充可旁听 topic |
| V4 数据可信度 | 配置/广播布局交叉校验，坏 frame/blob、非有限值、长度、时间及身份异常拒绝；原始 body stamp 新鲜度；Unknown 不显示安全确认 | 两平台 17 项单测及负例集成通过 |
| V5 有界与丢失 | 64 topic、每 topic 8 实例；原实例与观测序号/丢弃计数；单发布者保留样本顺序，超载允许丢失；退休清理缓存 | 真 SHM 与容量退休测试通过；跨发布者公平性与完整事件审计不保证，见下文 |
| V6 真实输出 | Rerun 0.38.1 官方文件校验、同版 Metal Viewer 实看关节曲线/事件/红绿图像/两点云/四值 Tensor，并移动整数时间游标 | 文件及实时 GUI 通过；[默认画面](../.verify/robot_viz_2026-09-29/default.png)、[实时画面](../.verify/robot_viz_2026-09-29/live.png) |
| V7 故障与生命周期 | 晚启动/重新连接、双观察者、无效目录、黑洞 Viewer、真实连接中断、父进程 SIGINT 与资源回收 | 显式集成和独立实时检查通过；收到数据后 SIGINT 为 80 帧/10 类、worker 正常回收 |
| V8 质量门 | macOS 定向 Clippy、Linux 根 fmt/Clippy、转换/SHM、依赖审计、CI 静态及独立审查；失败原件保留 | 根 fmt/Clippy、完整串行 workspace、609 项 S0+rt_guard、13 项退出与优化构建通过；独审通过 |

最终正常文件记录 1000 帧、十类消息、11 个发布者实例，本次 gap/drop/reject 均为 0；
实时 GUI SIGINT 记录 80 帧和 67 个序号缺口，真实连接中断以错误退出并回收 worker。
这些是单次结果，不构成观测无丢失保证。底层接收从首连接扫描，持续积压可使其他发布者
长期不可见；从未观察到的实例不计入 gap。集成夹具通过 A→B→A→B 分段发布并从官方
RRD 读回每段新序号确认两实例，不声称修好了底层调度公平性。

镜像队列、worker 和活跃 SHM 预约均有限额；512 MiB 是 worker 的估算预约上限，
被外部持有的 loan 可晚于预约释放，不能视为进程或系统常驻内存的绝对上限。
CPU Tensor 支持值统一无损转 f64、来源 dtype 留元数据；GPU 预览超界明确标记。
固定状态和标题使用 ASCII，原中文事件仍可能受 Viewer 默认字体限制。
图像/点云/Tensor 为真实软件夹具数据，未接生产相机或外参，不补造 FK/物理单位。

首次截图因官方 Viewer 异步加载得到错误页、初版 Tensor 自动色阶不合适、Linux r1
新 nightly trybuild 两行上下文差异、r2 多发布者夹具受首连接饥饿、r3 旧生命周期测试
只允许两个 Node、r4 S1 对称节点断言及退出专项 11 通过/2 失败等原件全部保留。分别修正加载确认、逐帧色阶、golden 上下文、
夹具真实读回握手及 RPC/S1 三类 Node 生命周期断言，未放宽生产数据校验。
新生命周期 oracle 保留 `nofile=128`、256 轮、30 秒监督和最终 FD 不增长，
逐级核对 Node `1→2→3→2→1→0`、目录删除、worker join 和资源归零。S1 通过真实工厂的正式/镜像双路
事件和 `/proc/self/task` 的线程集合检查退出，不新增生产 API。

退出专项修订只涉及测试：子进程显式 `NO_COLOR=1` 并清除 JSON 格式覆盖；
两项快照耗尽测试仍限 15 秒等待真实关闭 ticket，随后按 `shutdown_deadline_ns + 1s`
的绝对时刻监督父进程回收，不从观察时刻重置预算。原生产 10 秒收尾、READY 4 秒、
1000/8000 次耗尽迭代、退出后 9.75–10.1 秒实测判据均不变。旧父测试把名义 4 秒耗尽
等待与收尾合在 15 秒内，VM 实际约 6.2 秒才触发，使父截止先于程序原期限；保留的
失败页没有完整退出结果，不因这个解释回填通过。真实耗尽时刻在 join 后才导出，
失败页对应字段为 0 不能解释为没有发生故障。修订后的 13 项原生退出测试已全部通过；RPC 与 S1 两处 256 轮生命周期测试也分别通过。

桥自身正常 `--save` 文件官方 verify 通过；两个独立接收服务器 `--save` 在 SIGINT 后
缺 footer，官方 verify 实际失败，原件保留，不把传输成功当完整文件通过。
文件阶段 Viewer logs 为空；后续实时接入初期 stderr 有一次 `Timeline robot_time not found`，
随后时间线与画面正常，不宣称全程零警告。首 GUI SIGINT 检查因证据日志重名而在启动桥前
失败，新目录重跑通过；这项前置失败也保留。

最终 Linux r6 manifest SHA 为 `abb7029420438ed69d12b0aab541cea9558f027485af68c196d4342359f430d2`，
620 项输入前后核验一致。r3 可视化/transport 专项、r4 RT/优化构建、r6 全仓/退出分别保留
原回执；r3 后仅修三处 Linux 测试，生产输入未变，后续 CLAUDE 只写回当前状态。
全仓使用明确绑定的测试 peer 与串行执行，ignored 不计通过，不关闭既有 S1 并行超时根因。
测试构建的既有 unused-dependency 警告和依赖审计的重复版本警告保留；根 Clippy 零警告。
证据[归档索引](../.verify/robot_viz_2026-09-29/archive.json)与[原件包](../.verify/robot_viz_2026-09-29/evidence.tar.gz)
包含失败、各轮源码、实际 Viewer 画面与真实退出回执；[示例记录](../.verify/robot_viz_2026-09-29/example.rrd)
可用同版 Viewer 打开。

在该 9/29 阶段，Orin 严格 SSH 因本机信任条目缺失拒绝连接，改用隔离 Lima Ubuntu 24.04 ARM、
`nightly-2026-09-29` 执行 Linux 软件门。未更新 Orin 安装件，未执行新硬件动作；
CI 只完成配置/actionlint 检查，尚未 push 或运行远端工作流。使用入口见
[观测工具命令](02_start_here.md#rerun-观测工具当前开发入口)。

## 2026-09-29 · 腕部硬件探针与诊断类型分类

ca14冻结件左右腕部各一次小幅往返和收尾通过；头部/底盘只有状态检查及guard安全写，
未测主动运动。原run_01不因后续诊断修复改写，详[本轮记录](handoffs/hardware_smoke_2026-09-29_summary.md)。

针对每侧17条Unknown记录，新增私有`FeetechDiagnostic`及`ParameterReason`，穷尽原始错误后统一编码。
本机fmt、feetech全目标/全feature Clippy r5、147项库测试及5项独立集成测试通过。
集成测试使用真实ControlAuthority、FeetechAdapter和BusLease，验证Enable提交前及Disable后
拒绝周期写且transport写次数为0、获得许可后正常发送；同时验证急停/超时/撤销编码与
通信错误、设备状态及参数错误保持区分。无分配检查采用独立测试分配器和原子诊断收集器，
只覆盖经公开Safety入口注入错误的固定诊断路径，不证明周期执行整链无分配。

初轮Clippy风格错误、测试Mutex首次惰性分配触发64字节SIGABRT保留；修复测试设施后5项通过。
独立源码及本机证据审查通过。**该诊断阶段Orin检查未开始**：known_hosts两主机条目缺失导致
严格SSH拒绝，未向Orin传输源码。隔离Lima Ubuntu 24.04 ARM、nightly-2026-09-29 的第四轮
实际fmt→根workspace Clippy通过；Feetech real-hw测试147+5+12+3项、SerialRT convergence/rt_guard
42项通过，605项输入前后SHA一致。前三轮CLI lint、格式、S0 lint失败保留，三处写法等价修正。
该诊断快照不含Rerun/观测镜像，不替代其验收。未正式安装或执行新硬件动作。
历史34条逐条归因、写拒绝后的读公平性/过期tick、右H2、D15保持OPEN。

## 2026-09-29 · 最终 Disable 延迟回执软件回归

`robot_tests::comm_fault_rpc` 是 Linux H2 工具与跨平台回环测试的同一调用入口：GetState 确认状态3后发送非dry-run Disable，逐项核对Ack身份、dry-run及非零完成戳。业务期限2000ms、gateway grace 500ms、普通传输2s与最终传输3s保持原值。

显式 `final_stop_transport` 集成测试共6项、9场景：2100ms合法Ack成功，同2100ms普通连接与3300ms最终连接被tonic超时拒绝；服务端错误、三种错误Ack、两种状态失败保持拒绝。每个场景先关闭并join服务任务，再确认active requests为0。客户端RAW时间只作记录，不与Tokio时钟的精确边界比较。模拟服务不验证真实gateway grace、RR/Lower退休或硬件断扭。

本机macOS定向Clippy、2项新单测和6项集成测试通过；全workspace仍受既有S0 Linux接口限制，扩大Python测试同样遇到Linux专用接口缺失。4处新nightly lint已等价修复，S0平台语义未改。

Orin独立目录的603项来源逐SHA核验，最终fmt和实际重查的根Clippy零警告；测量工具三项feature闭包实际编译检查通过。lib 4项（含新2项）+工具182项、回环6测试/9场景、66Python及工具dev构建通过；延迟正例实测2101190176ns。绑定本次S1故障测试peer等三件后，完整workspace串行通过；原有ignored不计通过。正式源ca14f0e仍clean，四安装件、六停止服务与无控制进程的前后快照一致。

首默认workspace缺fault peer的两例FAIL、另轮S1默认并行284/2超时、macOS限制及旧mtime缓存门均保留。14项隔离串行和完整串行通过不关闭原并发根因；最终根Clippy以刷新mtime后实际重查为准。完整命令、计数、源码和原件见[本轮摘要](../.verify/post_e_comm_fault_hardening/S49_final_stop_transport.json)与HANDOFF。

历史右H2整体FAIL、D15 OPEN、`deployable=false`保持；本轮未执行硬件重测或正式安装，未重读现场marker/latch，物理现状UNKNOWN。

## 2026-09-23 · 右H2真实长测：测量四组PASS，整体FAIL

runtime `1ac4548`、实际build `5952498`；四组各100预热+1000正式样本的原P99门逐组通过，SLA-B P99依次为10.168864、6.811968、10.145344、6.914688ms。最终非dry-run Disable约2.001秒返回`Cancelled: Timeout expired`，因此原run/report均FAIL，右H2未通过。EStop/Disarm、GetState安全态3及有效final_trace不替代DisableAck或物理断电。
authorize/launch各1，工具/driver/SSH exit1真实回收；独立只读清场PASS、六服务inactive/PID0、右双marker与EStop latch均`1\n`、安装件不变。清场时电源UNKNOWN；用户后续先确认断电、再报告“已通电，电源和 USB 都重插”，本次交接时物理电源现状未知，操作时刻未知。支撑/接触区未重新确认，重插不授权新H2；D15 OPEN、deployable=false。首USB枚举瞬态FAIL、漏传alignment的零启动前置FAIL及其内部cleanup FAIL保留。软件修复`ca14f0e`已提交推送、Orin源码已ff-only对齐；生产4/测量3原生构建、mock AC-12与定向单测仅软件PASS，首次Endpoint 0匹配不计通过，未做2.1秒延迟回执端到端、正式安装或硬件重测。证据见HANDOFF及H包；用户2026-09-25仅授权handoff提交推送，后续开发暂停。

## 2026-09-23 · 右H1/H3现场测量（已验收归档）

运行3790fd9/build5952498；H1、H3各100预热+1000正式通过，H1 P99/max=1.147712/1.171040ms，H3=0.256192/0.879104ms，H3正式全部≤10ms。
Enable0/drop0/truncated0；两工具exit0/reaped，内部五次与退出后清场通过。driver独立PID未捕获，远端exec+SSH真实wait边界如实保留。
首readonly命名冲突及setsid前置拒绝FAIL保留，原件/独审入口见HANDOFF。本轮不提供右H2结论，未正式安装，D15 OPEN；H3只证明主机调用完成，电源UNKNOWN。

## 2026-09-22 · 右H13原生Python兼容验证（仅软件，已归档）

运行HEAD `9afdc92`；首Orin uv Python3.12.13缺`os.pidfd_open`，22方法3FAIL/3ERROR原件保留。
`right_h13_native/implementation_r3/`仅在两项pidfd API均可调用时使用pidfd，否则走已审自有未回收child路径；资源错误仍FAIL。
本地27方法、独立5方法与新版Orin27方法/默认PLAN_ONLY入口通过；真实wait/reap、前后状态相同、收养child为空。
未重跑Rust矩阵或访问硬件；实际build仍5952498、571项e05a，右H1/H3/H2 NOT_RUN，未安装/部署。
旧round6 FAIL与早轮回执缺项继续保留，不称单轮全过。原件及来源见HANDOFF。

## 2026-09-22 · 右H13监督与原件oracle（仅本地软件准备）

开发最终22方法、独立12方法PASS，覆盖纯原件反例与无害子进程监督；没有新右硬件结果。
round6夹具目录缺失FAIL保留；早两轮无独立PID/wait/源码快照回执，不回填、不称单轮全过。
既有三Python/Rust与5952498产物未改；右H1/H3/H2 NOT_RUN，未安装/部署、D15 OPEN。
本阶段证据已归档，完整身份及边界见HANDOFF和`right_h13_preparation/`。

## 2026-09-22 · 双臂工具与W1测试的新原生软件验证

source/build `5952498`，571输入aggregate `e05a2ca…ce0ff`；6条release recipe、生产4/测量3、7help，
Orin64Python及默认并行latch10、唯一AC12 mock四组各2通过，18份completion真实回收、122原件逐SHA/bytes核验。
final_trace严格一致及双child停止、非dry-run收尾、实际UID清场和来源/安装状态不变通过独审。
本地合并态64+10及fmt/Clippy已通过；旧57项未重跑，复核初次Mock字段误用失败保留。
该原生阶段已归档并收口4441f59，入口`h2_arm_native/`，完整来源/边界见HANDOFF。未安装/重跑硬件，右未测、D15 OPEN。

## 2026-09-22 · 本次左H2完整通过，已收口10cff7a

runtime c5daa67/build fb600fd；四组各100预热+1000正式，4400 attempts、290158 events，
6834条InjectedDelay无短于请求；背景66986唯一新写/恢复8800。原SLA-A/B P99门均通过，
第三组SLA-B max52.502592ms仍保留，不把P99通过说成最大值保证。完整四组数字见HANDOFF。
工具/driver exit0真实reap；独立final_stop与final_trace、实际UID/串口/六服务清场、双Dirty均通过。
独审/root已验收，旧三次测量FAIL/一次前置拒绝保留；36远端原件与61本地owner材料纳入。
值守解除、电源UNKNOWN；右未测、未部署、D15 OPEN。0047920/2e5b515现已本地集成并验证，
合并态64Python和完整最终SHA默认并行10项及fresh fmt/Clippy通过；不作为此前左H2的runtime/build。
新版Orin5952498软件绑定已通过，见上节；其证据已增量归入H包，W1历史具体根因仍OPEN_UNCONFIRMED。
左H2收口时证据包为65097成员/65096索引，新增376件；完整SHA见H.json，包后审件下包承接。

## 2026-09-22 · 注入延时与尾窗检查软件验证

源码`fb600fd`，571项新aggregate `52b1832…58d17`；完整来源与阶段证据见HANDOFF。
本地定向7延时+2 sticky+5 lease+43 SLA＝57通过，默认及相关feature Clippy零警告；
开发期两轮Clippy/编译失败保留。Orin六recipe原生生产4/测量3、7help、同57项及唯一AC12 mock
四组各2通过，共20次completion；final_trace与非dry-run收尾、清场/来源/安装状态核验通过。
controller错猜无`000_`前缀路径的原FAIL保留，只读final_validation另exit0补充；独审/root验收已通过。
普通/FIFO70@CPU4时钟探针各4000项，均无短延时/补等；时基不同已确认，原1426ns根因未定。
仅测试hook及验收工具变更；原SLA/样本判据、旧H2 FAIL保留，无硬件动作、不自动H2/push。

## 2026-09-22 · 左H2一次真实测量FAIL及收尾

用户新现场确认“本次全部就绪”后，runtime b4ff720/build4191bf0执行一次左H2；
571编译输入、三Python、正式四件/配置/标定/unit身份均核，authorize/launch各1。
真RT门通过；topic-idle 100预热+1000正式PASS（SLA-A P99=25440ns，SLA-B P99=10649408ns）。
topic-inflight 100预热+639正式后invalid1：attempt739/seq13681请求3895634ns、
实测3894208ns，短1426ns，严格b>=a判据拒绝；第三/四组未执行。原report/run均FAIL，
不以数值差小豁免，时基/等待实现候选根因仍待验证。
独立ExternalEStop/Disarm/GetState3及非dry-run DisableAck成功；工具与driver自然
exit1/reap，外层signals[]。driver及fresh清场related/unreadable为空，串口fuser无占用，
六服务停止，双marker Dirty。未自动重试/安装，收尾后没有新物理断电确认。
完整原件见H包local/s48/h2_run；失败证据收口后暂停检查，不push，D15 OPEN、deployable=false。

## 2026-09-22 · 左H2前置拒绝后的软件修复验收

现场确认后一次driver exit1/reap，authorize/launch均0，未访问电机。TRUST 0755误配与
原清场两个未知PID失败保留；目录仅改0750，inode/属主/原文件身份内容不变。
1b6db3e统一真实身份/RT/TRUST前置与有限固定sudo只读PID补证；4191bf0接受实际
PID1/2/42的合法零启动计数，负值/缺字段/格式错误/身份变化继续拒绝。冻结设计、
571编译输入与测量预算未改。原生Astra/medium开发及独审，root验收。
最终37驱动+13产物测试共50项通过，Orin新checkout真实sudo集成通过；本机早期
需密码的集成FAIL仍保留，夹具不冒充sudo。每次提交前fmt→Clippy零警告。
最终实际HEAD4191bf0双端clean；原六release recipe冻结生产4后测量3，七help与
十五份自然完成/清场通过，安装四件/配置/标定/七unit/安全状态文件不变。
首native通过但共享预检因零启动计数FAIL；末轮重新绑定与共享只读复检通过，
各轮原件分别保留。证据见H包local/s48/h2_new，不称单轮全过或新H2通过。
未安装、未重试H2；收口commit后暂停检查，不push。D15 OPEN、deployable=false。

## 2026-09-15 · RR R2 严格请求原件与真实故障验证

基线0513a8c，原生开发、独立源码/证据及root本地验收完成，正式round1连续26门全部exit0；
43项编译/来源输入首末一致，62份真实测试产物记录非零与SHA。
默认IOX42/0/1 = 新31主例+3 pubsub父例+原8例；feature45/0/1增加3真实故障例。
ignored为父门专用子进程入口。CAL默认和feature分别45/0/0，模型6/0/0；
workspace2207/0/9、S0hook656/0/0，重叠分组不合计覆盖，原ignored保留。
真实prepare拒绝保原状态、post-push issue保原Pending、两种completion失败Drop顺序、
Unknown/收包错误保残件、不同N、原完整ID/绝对key和字段析构顺序均有非零门。
先cargo fmt再根Clippy，两组额外root及两vendor默认/feature库内cfg(test) Clippy均零警告；
CAL no_std、同源模型两负对照、build/timings、tree/deny/metadata均通过。
首summary误读Cargo包ID后只修解析，原FAIL与原日志保留，源码/锁/runner未变。
根14/嵌套2/模型1成员保持；normal-only56包、normal+build80包均与R1身份一致，
fixture/Loom不入生产图。证据入口rr_r2/result.json、handoff.md、两项独审及root_acceptance.md。
未跑缺失tests_common的上游独立集成目标；strict清理不冒SIGKILL，真死亡仅pubsub。
R3本方迁移、R4双环境、Orin/硬件与P-E未执行，D15 OPEN、deployable=false。

## 2026-09-14 · RR R1 CAL 与同源内存模型验收

基线cbed745；cc-claude开发自测、独立Codex分审CAL与模型/依赖图，root验收。
正式round1的21门全部exit0；fmt及三组根Clippy、真实CAL库内test目标Clippy、
CAL无std check/Clippy、模型fmt/Clippy、build/timings/tree/deny/三metadata均通过。

| 实跑范围 | 通过 / 失败 / 忽略 |
|---|---|
| CAL原语与真实连接（process_local、POSIX） | 45 / 0 / 0 |
| IOX既有库内单测 | 8 / 0 / 0 |
| 同源Loom测试（含两个负对照与能力边界） | 6 / 0 / 0 |
| 根workspace（含文档测试） | 2207 / 0 / 9 |
| S0 rt_guard,test-comm-fault-hook库内 | 656 / 0 / 0 |

分组有重叠，不相加为独立覆盖。正式库内Clippy复用同源码缓存，实际--test driver
编译证据在dev/cal_clippy_rustc_wrapper_probe.log；没有用生产lib检查冒测试lint。
模型正向完整排列6/7/453/1538；负对照各完成2次后第3次命中所需尾不可见反例。
两vendor运行时闭包在根/嵌套均80包，name/version/source/checksum一致；根14成员，
Loom和测试运行器不进生产图。原开发失败保留，不以正式通过回填。
未运行file后端、缺失tests_common的上游集成套件或ARM/Orin；R2原pin/复用资格、
R3本方迁移与R4矩阵仍待完成。证据H包local/s48/rr_r1；D15 OPEN、deployable=false。

## 2026-09-14 · RR R0 本地源码与构建准备验收

两个iceoryx2 0.9.3发布包接入本地patch，原checksum/168原件及同commit许可已核；
根仍14个成员，其余依赖版本不变。唯一源码兼容修正为fetch_update→try_update，
原内存序和闭包不变，解决path依赖在当前nightly下触发deprecated编译拒绝。
cc-claude完成开发自测，独立Codex及root验收通过；最终fmt、三Clippy、transport、
RPC lib、build七门全0，transport99/0/3、RPC304/0/0（通过/失败/原忽略）。
同图tree/deny引用首轮成功，原三编译失败与三个nonmember选择探针101保留。
证据见H包local/s48/rr_r0/root_acceptance.md及independent_review.md。
R0只验证来源与已有本方基线，未执行vendor私有测试；CAL/IOX修正、模型及迁移仍待后续。
用户恢复持续目标，阶段验收后提交并继续；本轮无Orin/硬件，D15 OPEN、deployable=false。

## 2026-09-10 · W4b 固定五例SIGKILL与同目录恢复（本地独立验收通过）

W4b在原测试feature内增加五个真实文件成功步骤后的SIGKILL，原Child真reap后
复用原持久目录重启。空marker拒启动；合法marker下的1/0或0/0保守恢复1/1；
无marker的0/0由原loader加载false。首轮证据完整集合/SHA不变，真实初始INFO和
原只读FlatBuffer回复验证重启权限；首Clear无回复，不以杀进程宣称业务退休。
基线`382ca44`，376源/129输入首末一致，8改3新已由非作者独审。正式16门满足
期望（14个exit0、两个参数拒绝exit2）；fmt与三组Clippy零警告。默认S0 605/0/0、
完整hook858/0/1、工具89/0/0，分组不相加，原60秒soak仍ignored。
新5/5、旧W4a2/2与原All72/72；37+13必须具名、4实际产物、979软件文件均已核。
原开发失败完整保留，诊断PID锁与日志级别夹具补正已独审；未修改正式源或冻结输入。
完整裁决及原件见H包`local/s48/w4b/review.md`。无依赖/冻结设计变更。
这是同内核进程中断，不等于断电；未覆盖完整磁盘/监督/双环境或Orin。RR修正与
P-E仍待完成，wire4历史FAIL、D15 OPEN、deployable=false保持。按用户要求本包提交后暂停。

## 2026-09-10 · W4a 文件故障恢复验收

W4a已独立源码/运行证据及root验收，APPROVED_SOFTWARE_SCOPE。基线`1338f9d`；
373源/187输入首末一致，正式round1的15门满足原期望：13个exit0、两个参数拒绝exit2。
默认S0 605/0/0、完整hook850/0/1（含26个compile-fail文档测试）、工具79/0/0；
重叠分组不相加，原60秒soak仍ignored。13新共享/3工具/7旧锚点/9原Trace具名均通过。
4真实产物、796软件文件逐件核验；两新增场景2/2、旧All72/72逐名逐序一致。
双CSV共57/68条W4事件，真实同inode+EBADF、同事务/原目标修复、唯一unlink；
原FlatBuffers回复直接核验首FsyncFailed/Started/retired，双1/1、latchtrue且真clean exit0。
原finalizer模块前缀误写exit1保留；独立审核后的r2严格核9原名并真实exit0，未重写原门。
开发失败、两处RR错误出口修正与最终源码审链均归H包`local/s48/w4a/`，root裁决为review.md。
无Orin、systemd、完整SIGKILL或P-E；RR修正另包，wire4历史FAIL/D15 OPEN/deployable=false保持。

## 2026-09-10 · W3生产总装验收

W3共享权限、高/普通双队列、唯一文件writer、RPC原责任账本与受监督关闭链
已接生产调用方，并获独立源码/证据及root本地验收，APPROVED_SOFTWARE_SCOPE。
基线W3a `757ac55`；正式round4连续31门全部退出0，367项来源`2872511c…cc01c5`
与228项冻结输入首末一致。完整软件All72/72；workspace2151/0/1、doc56/0/8、
S0特性837/0/1（通过/失败/忽略，重叠分组不累加）；七份真实产物及timings已核验。
通信不可信/断扭失败保持原RR/Slot到真实退休；客户端超时不释放责任，正常
AdapterError仍按原期限真回复。原fresh Disable竞争与闭连最后回复确定性窗口
修复已回归；闭连跨原子可见性证明仍留W4，不外推为弱内存严格排空。
三轮正式失败和开发失败原样保留，完整裁决见H包`local/s48/w3/review.md`。
W4纯D/逐步SIGKILL/双环境及P-E尚未完成；本轮未访问Orin或硬件，wire4历史FAIL、
左H2两次FAIL、D15 OPEN、deployable=false保持。冻结设计文档未修改。

## 2026-09-10 · W3a 协调与持久化健康验收

W3a Clear协调与持久化健康原语已独立源码/证据及root验收，APPROVED_SOFTWARE_SCOPE。
base `9317e1e`，round11十门全部通过，362来源`bcb93c1a…759a1a`一致，恰三改+三新增。
唯一许可/RR按值持有，完整匹配后才处理完成；真实退休再交唯一Reply，共享八次接受重试。
逻辑时间在实际提交后单次采样；物理完成与退休分开，断链/异常不伪造退休，当前health与sticky分离。
core58/doc19+3ignored、协调23/公开18/health4、S0doc25；workspace2098通过/1既有ignored，分组不累加。
非法Refused和不可达屏障在源码评审中修复；round10七项换行预期失败修后完整重跑，原轮证据保留。
详H包`local/s48/w3a/review.md`。未生产总装或Orin验证，wire4历史FAIL、D15 OPEN、deployable=false保持。

## 2026-09-10 · P-A1 原语验收

P-A1权限视图、总线租约与IPC责任原语已独立源码/证据及root验收，APPROVED_SOFTWARE_SCOPE；
base `e560691`，round8九门全部通过，完整359来源`28ba20ef…4962a2`一致，恰四改+两新增。
固定域原子槽、真实BusLease与完整pending记录已落；16个Lower waiter逐条锁存，超期不腾容量。
三方域核验后撤公告再真实退休；竞争拒绝完整返原，转cleanup保留旧seq，不能提前释放RR责任。
slot5/serial2/公开BusLease5/guard5/pending11、core doc17通过/3既有ignored；分组计数不累加。
S0文档compile_fail未在九门执行；没有本包workspace test/timing声明。原fmt/Clippy/夹具失败轮保留，
详证据`local/s48/pa1/review.md`。未撤销启动/commit拒绝两路及生产总装留W3，未访问Orin；
wire4历史FAIL、D15 OPEN、deployable=false保持。下一包W3a，完成观测时钟增补见PLAN P.6.14.13。

## 2026-09-10 · P-G1 原语验收

P-G1已独立源码/证据及root验收，APPROVED_SOFTWARE_SCOPE；base `129ef7d`，round4十门全通过。
完整357来源`609ee3f5…33fa60`与前后清单一致，恰四新RPC模块+lib四注册；无依赖或生产调用迁移。
cache14/queue16/credit8/classify6/doc8、RPC299、workspace2026通过/1既有ignored，聚焦计数不累加。
按总量预留缓存并保留未知在途责任；私有受检份额和唯一队列身份，票据按实际队列与入队序号取消；
五类真实重入析构子进程均exit0且被父回收，payload在锁外析构。真实P-M字段区分缺失退休、合法Unknown与非法effect。
原fmt中止和Clippy失败轮保留；详证据`local/s48/pg1/review.md`。下一包P-A1权限视图/生命周期；
生产权限、RR退休与唯一writer总装仍待W3。未访问Orin，wire4历史FAIL、D15 OPEN、deployable=false保持。

## 2026-09-09 · P-S1/P-L1 原语验收

P-S1/P-L1 双队列、准入和心跳原语已 **APPROVED_SOFTWARE_SCOPE**：base `ac92f70`，
round6十一门通过，完整353项来源`961aed49…2355`一致，恰3改+2新增。
admission14、ingress9、watchdog8、doc20、公开5、真实guard6通过；S0 lib542，
workspace1982通过/1既有ignored。聚焦门与workspace计数不相加，doc含4个新增compile_fail。
独立源码/证据及root范围核验通过，证据`local/s48/ps1/review.md`。准入先核总量、类、
peer槽与份额，再受检推进序号；拒绝零变更，错账本返原票据，Drop保留占用。
高队列Lower先于Restore且类内FIFO，普通队列容量按受检L/C派生；真实三来源载荷验证。
分配守卫正例在scope内搬移预分配载荷，反例必须出现唯一scope标记、真实SIGABRT并被父回收。
心跳仅戳相等为Valid，过期判定保持饱和减法；生产接线须经from_rr受检分类，裸公开enum
不作为权限凭证。旧中止/Clippy失败轮和raw日志保留；无依赖或生产调用迁移、无Orin验证，
wire4历史FAIL、D15 OPEN与deployable=false保持。

## 2026-09-09 · P-T 显式RR原语验收

P-T 显式RR容量与发送分类原语已 **APPROVED_SOFTWARE_SCOPE**：base `0d3caca`，
round6九门通过，完整351项来源`4e9c698e…767e8`一致，恰6改+1新增。
RR20、真实TOML1、roundtrip6通过；workspace1950通过/1既有ignored，doc0通过/2既有ignored。
独立源码/证据及root范围核验通过，证据`local/s48/pt/review.md`与同轮真实timing。
新增精确L4/C8 profile、创建前受检容量、可信peer和未投递/不确定发送分类，旧API保持。
原计划的global_config诊断缺口经真实同名双配置L16/L2反例先红后修，两个公开工厂
现在均从node.config读actual2，服务身份不变；原绿round4与失败轮均保留。
仅增加已锁定iceoryx2-cal=0.9.3的dev边供纯错误枚举测试，生产依赖图前后相同。
未切换生产调用者、未验证Orin或电机；wire4历史FAIL、D15 OPEN与deployable=false保持。

## 2026-09-08 依赖升级分包

全部 27 项外部直接依赖的最终版本与验证记录见
[依赖升级记录](handoffs/dependency_upgrade_2026-09-08.md)。该分包基于 C4，最终锁定后
使用 `--locked` 执行静态检查、测试与构建；C5 与硬件验收仍按各自条件独立收口。
依赖分包已独立 **Approved** 并提交 `0001858`：完整 tests **1643 PASS、2 项原有
ignored**；lib / bins **1461 PASS 包含在 1643 内，不相加**。原有 ignored 未计为通过，
完整执行证据见 [report.json](../.verify/dependency_upgrade_2026-09-08/report.json)。

## 历史候选基线：post-E 通信故障硬化（2026-09-08）

以下为2026-09-08至09-09当时基线；当前软件、硬件与授权状态以页首及HANDOFF为准。

权威验收条件见 [Spec 的 AC-1–AC-24](../team-swe/specs/post_e_comm_fault_hardening.md)，
函数与命令映射见 [正式 Plan](../team-swe/specs/post_e_comm_fault_hardening.PLAN.md)，
执行状态见 [HANDOFF](../team-swe/specs/post_e_comm_fault_hardening.HANDOFF.md)。

C5 提交 `85efefb` 的源码 `e6d2b30699379ee0e2222649efb4e79093978c612e0d6a67bc83dce9720d2598`
（308 输入）已独立 **APPROVED_SOFTWARE_SCOPE**；汇总状态为
**SOFTWARE_ACCEPTED_HARDWARE_PENDING**，`deployable=false`。

| 软件验收范围 | 本地与 Orin 当前结果 |
|---|---|
| 完整 tests | 各 1737 PASS = 1528 lib/bins + 208 integration + 1 doc；11 ignored = 2 integration + 9 doc |
| 进程与资源 | 各 67 软件场景、八套 smoke PASS；A1 原 1000 轮与精确 Node 集合 15→15 通过 |
| RT guard | debug/release 的真实分配 SIGABRT 检查与完整 60 秒 soak，四门 PASS；既有 ignored 单独执行，不重复计入全量 |
| 构建与独立质量门 | fresh、单 feature、schema、deny、ELF、timings 和库存检查通过；库存 23 targets / 210 cases / 2 ignored |
| 正式产物恢复 | 两端各四个无测试 hook 产物已核验：xlerobot_deploy、s0_daemon、s1_daemon、robot_rpc_gateway；Orin 为 AArch64 |
| 退出状态 | 临时 systemd unit 和验证进程已清理；Orin 六项原服务 inactive/PID0，保持停止 |

当前资源观测保持原 NOFILE=1024：Orin 完整 S1 FD 为 81→154→81，gateway 为
94→103→94，Node 集合 15→15，registry 与 cleanup warning 均为 0。它们是本轮
验收进程观测，不是正式机器人常态基线；隔离调用点 FD=4 也不能冒充 daemon 基线。

S1 私有 guard 按 publisher → Node 顺序析构；A1 通过上游 Node ID 解码精确归属
两个存活 PID，baseline 每端非空，原 2 秒 grace 后不得新增目录，空目录与同数量
替换均失败。完整实现约束、具名反例及复现命令见 PLAN P.6，软件门不靠清理全局资源通过。

用户已授权更新 Orin 原仓库、停止六项开发服务，并在调试 / RT 检查需要权限时
直接 `sudo`。实际构建 manifest 保留 features/profile/source/commit/SHA，不反填
后续提交身份；无 hook 正式产物已恢复，但不自动启动服务或宣称可部署。

| 现场条目 | 当前边界 |
|---|---|
| H1 / AC-2(d) | 左臂 PASS：100 warmup + 1000 正式完整 ping，零错误；P50/P99/max = 1.081312/1.153664/1.165824 ms（特性记录，无时延通过阈值）。右臂未测 |
| H2 / AC-12 | 左两次FAIL：首次采样边界缺失已由S-45修复；第二次topic/idle为100预热+138正式，缓存满且背景节拍失败。GetState确认state3、Disable失败，现场随后断电；S-46本地通过/Orin wire4满FAIL，S-48双队列优先级设计中；右未测；原样本/RR/载荷/P99不变 |
| H3 / AC-13 | 左臂 PASS：100 warmup + 1000 正式广播，完整六 ID 断扭帧、一写零读；主机调用最大 0.491552 ms，正式样本全部 ≤10 ms。右臂未测 |
| D15 | OPEN：软件收敛不保证进程死亡或三次广播均失败时的物理断扭 |

用户现已确认可以现场配合，逐臂物理准备完成后执行。H2 准备时发现验收工具未传
PX-56 的 FIFO70/左右 CPU4/5 参数；S-44 / PLAN P.6.10 已实现，两端必要门通过。
新增硬门在首次使能前逐 TID 核 S0、工具与 gateway 的实际调度和精确 CPU 掩码，
错误先留证再收尾；工具的 `taskset -c 0-3` 必须早于 Tokio runtime 创建。
这项修复不改变 C5 的软件批准，也不构成任何 H2 实测通过声明。
左臂 H1/H3 使用真实 by-id 总线；marker 与 latch 未改，六项常驻服务保持停止。
H3 证明主机 adapter/write 返回边界，不据此宣称 UART 已发完或机械已断扭。
S-44 两端各 57 项工具测试、Clippy、真实 mock AC-12 通过；Orin 两次 mock S0
分别实测串口 FIFO70@CPU4/5，其余全部 OTHER0@CPU0–3，均正常退出。
随后左 H2 实测三进程门 PASS：工具/S0/gateway 各 6/11/14 TID，串口 FIFO70@4，
其余 OTHER0@0–3。测量于第 36 次尝试拒绝不完整 epoch，正式 0，不能写 H2 PASS；
S-45 已在真实事务边界 arm/disarm，并为 idle 显式建立有界无在途前提。
`final_stop` 状态 3、进程全退、串口无占用，marker `1/1`，正式四 SHA 未变。
S-44 软件包与左 H1/H3 已独立 **APPROVED**，完整证据入口为
[H.json](../.verify/post_e_comm_fault_hardening/H.json) 与
[H_evidence.tar.gz](../.verify/post_e_comm_fault_hardening/H_evidence.tar.gz)。

S-45 源码 `c07a9886…514b1`（309 输入）已独立 **APPROVED_SOFTWARE_SCOPE**，两端必要软件门通过。两端均为 core 79、S0 lib 394 / bins 5、startup 8、SerialRT 5、工具 60 项
通过，三份实际 Cargo JSON 产物和完整 67 场景通过；Orin 四项 RT guard 通过。
AC12 两入口 × idle/inflight 四格各2完整 mock 样本，AC17 三 Fault 通过；不作为
物理 H2 或 Orin P99 证据。Orin 首轮工具混读误拒和复验时旧 harness 路径失败均
保留原始 FAIL；后者仅重编测试程序、源码 SHA 未变，原8项预算不变。

2026-09-09左H2第二次实测来源`ce61623`/`c07a9886…514b1`，RT门通过，首组238项
有效采样（100预热+138正式）无失配/无dropped。4096成功幂等写恰为3620背景+
476真实恢复；3621背景请求中1响应失败、11启动迟到至少50ms。三次前导RPC
201.565/275.143/172.031ms造成串行后继迟到，平均20/s不作通过。final_stop的
GetState成功确认state3，随后Disable失败；全部进程退出、串口释放、六服务停用。
现场后续确认左臂断电，独立补充审计与原快照分开。S-46本地五阶段已独立ACK，Orin原67通过但wire4满导致长测FAIL：生产默认4096/TTL300及wire4不改，工具固定due/最多64，
H2容量8192、背景单项完整2s与finish2.5s界。本地360.229s/8281唯一写通过；Orin
882唯一写即ExceedsMaxActiveRequests、收尾成功，8192未运行。用户明确基础控制
高优先级、普通关节目标走普通侧；S-48按两个有界队列、每轮先高后普通设计，
S-47容量子方案暂停合入。新验收须从真实publish/call之前计时，证明普通满载和
持久化屏障未释放时安全降低仍推进；旧AC-12取样后t0/P99不改，不外推入口等待。
双队列总装及新增入口时延验收尚未完成，不能以本地或短mock替代Orin长测。

S-48 A1纯文件事务本地门及独立评审已通过：来源`b61a1534…b6c20e`/313输入、
base `ce61623`，28个模块测试入口（含1个子进程入口与16处实际进程退出）、
7项独立公开API、24项原latch回归、3项compile-fail文档测试，fmt/Clippy零警告。
验证marker先于旧loader、完整身份与不可复制凭证、strict写序、特殊文件拒绝、
有界临时名碰撞及unlink后错误。逐步骤hook在操作成功后中断；删除后的目录
同步错误用例是目录open失败，不是假称fsync syscall报错或物理掉电验证。
A1未接生产启动/IPC，尚无Orin新版本验收；证据为H包`local/s48/a1/second/`。

S-48 A2已评审提交`dee67fa`：来源`439af44e…8a8d3`/317输入、base `9fe20ea`，
第六轮13道门PASS，模块48、文档反例13、公开默认16、debug/release guard各21、
rt_guard-only18及A1回归28/7/24/3。完整证据为H包`local/s48/a2/cc/sixth/`，
root源码审查与独立摘要复算分别见同目录上两级的review.md与root_evidence_receipt.json。

A3a将权限模块归位core、s0重导出同一模块，保留原语语义与唯一domain计数器。
来源`6e9571ab…b7084`/320输入（含3个已删除旧路径）、base `dee67fa`，第二轮
18道门全部PASS：fmt、全workspace与core/s0带特性Clippy、core无默认特性check、
core全库126（含47项迁入）、13项文档反例、s0真实inhibit对照1、公开默认18、
debug/release guard各23、rt_guard-only20、A1回归28/7/24/3；不同配置不合并计数。
七个生产feature图无test-comm-fault-hook，反向阳性对照确认core转发可见；首轮
向下图阳性对照FAIL保留。workspace/all-targets热缓存构建8.72s与真实timing报告
留存，不据此宣称冷构建性能或RT时延。root核搬迁等价及非A3a输入不变，证据为
H包`local/s48/a3a/cc/second/`、`review.md`和`root_evidence_receipt.json`。
A2/A3a均未接生产或运行Orin；snapshot并发修复由下述A3b完成，双队列与单writer仍待总装。

A3b快照修复已独立 **APPROVED_SOFTWARE_SCOPE**：来源`c359e632…d9c5bb`/328输入，
base `fdac02c`；第六轮16门与补跑6门全部PASS。预分配原子载荷、固定codec、&mut
单写及非回绕版本替换并发volatile载荷；耗尽退出worker，daemon监督executor/IPC。
无watchdog的mock回归核真实publish调用前时间晚于READY，并在实际join后导出。
workspace all-targets为1740 PASS/1 ignored，doctest17/8 ignored，transport hook
77/3 ignored，S0 guard+hook517/1 ignored，release guard25/0；配置重叠不相加。
既定debug/release真实rt_entry分配负对照、各60秒soak及parent_pub_then_child_s1_open
ignored专项另跑通过。原模块Miri三seed均报数据竞争，修复后同驱动三seed均无UB；
范围只含同步本体及夹具codec，生产codec由workspace与真实allocation guard另核。
原67场景、7生产feature组合及阳性对照、两份本次实际timing均通过。独立审计复核
22门、328源码输入及1411证据文件；A3b外来源未变，S46改动不混入此分包。
证据在H包`local/s48/a3b/cc/sixth/`、`sixth_supplement/`、`review.md`及
`independent_evidence_receipt.json`。首四轮FAIL、第五轮旧源码PASS和评审修订均保留。
本包未接权限权威/双队列/单writer，未做Orin验收，D15 OPEN不变。

A3c/W0已独立 **APPROVED_SOFTWARE_SCOPE**：来源`80294b56…145f73`/331输入，
base `d67752d`；round10七门严格串行PASS，fmt-all、全workspace及hook Clippy零警告，
模块42/公开10/旧latch24/compile-fail4，均无失败或ignored。独立源码复核确认
create取得File即Created、首个metadata失败返活fd、目录sync后Durable提升不可失败；
0→1续段测试核primary五步→backup五步。原16个真实进程退出点保留并增加身份采样点；
聚合父测试日志与冻结测试源码共同证明，不称17份独立子进程日志或物理掉电实测。
故障注入返回syscall步骤EIO，不声称存储设备产生真实EIO。四个文档反例核不可复制/
重复消费错误目标。完整来源前后及当前一致，W0外含S46未变，round1–9/旧评审保留。
round5清单自指问题仅修清单，12件快照与旧回执逐件一致；历史缺来源绑定不回填。
证据见H包`local/s48/a3c/review.md`、`independent_source_review_round10.md`、
`independent_evidence_receipt_round10.json`与`cc/round10/`。fd-less凭证不宣称抗inode
ABA；W1锁、生产恢复/IPC/双队列及writer监督仍未接线，未运行Orin，D15 OPEN。

W1启动锁/恢复已独立 **APPROVED_SOFTWARE_SCOPE**：base `8624f27`，最终round3
来源`7a3480a4…6749b`/335输入，15个冻结文件。十四门退出0：fmt、workspace及专项
Clippy；锁单元13/公开验收10/daemon11/deploy68/五个hook目标48/W0模块42/
旧latch24/doc4/CLI6。计数含harness子入口，不当作独立场景数；权限IO本轮euid1000，
EINTR为注入；正常/ready等待有deadline，SIGKILL后的OS回收不承诺硬墙钟上限。
两位Codex源码/证据终审与root范围核验通过；round1中止、round2旧评审对象保留，
W0与S46不变。证据`local/s48/a3d/review.md`及`cc/round3/`，无Orin或服务操作。

W0.1只读双份核验已独立 **APPROVED_SOFTWARE_SCOPE**：base `1b21457`，round2
七门全零，335项完整来源一致、恰三文件变化。W0模块48/公开16/旧latch24/doc4，
计数含子入口；权限分支实际euid1000。真实对象、绑定目录替换、三相及零写入均核验，
独立源码/证据及root范围通过，见H包`local/s48/w01/review.md`。开发起点为与W1
已冻结终点相同的派生记录，非本包开发前采样；原轮和审计名称格式误判原样保留。
FIFO回收不承诺OS硬墙钟上限；没有生产调用方或Orin验证。

W2a纯writer模块已独立 **APPROVED_SOFTWARE_SCOPE**：base `81c5c54`，round6
七门全零，344项来源`93ec062c…42aa8`前后及当前一致，恰10文件变化。
模块41/公开19/doc10，workspace 1877通过、1既有ignored；实际euid1000，含子入口，
不重复相加为场景数。唯一Claim、锁目标配对、真实join凭证、修复续段与监督候选
已经验证；增量评审闭合完成读取、原Pending/公告截止、空轮不刷新、同目录实际再锁
及权限恢复。root独立核门/逐名日志/范围，见H包`local/s48/a3e/review.md`。
旧回归的夹具失败/空过滤/未复现限制保留；S46改动排除本提交，无生产接线或Orin验证。

W2b独立关闭/join机制已 **APPROVED_SOFTWARE_SCOPE**：base `b5a0bfa`，round3
十一门全零，346项来源`434632ed…ea4d1`前后及当前一致，恰六文件变化。
health72（含doc1）、机制15/15、公开15、doc16，workspace1914通过、1既有ignored。
独立源码复审闭合截止接纳、因果断言、失败回收、真实拒停及panic经报告取唯一凭证；
root核完整原始日志与逐名结果，五处跨行片段按原文/SHA还原，原日志不改。
round1源码评审未通过与round2 Clippy失败保留；十五个父场景均检查真实子进程。
已提交`6defac4`，证据`local/s48/w2b/review.md`。W1开发期旧锁偶发失败另为观察OPEN；无生产或Orin验收。

S46-B旧实验基线已 **APPROVED_SOFTWARE_SCOPE**：base `6defac4`，round2七门与三条CLI
通过；RR5、gateway1、工具75具名全通过，完整346项来源`fdf653d9…00373`一致。
独立源码/证据终审和root范围核验通过，证据`local/s48/s46/review.md`。七件原工作树
源码独立收口；新修正仅测试stop/join清理，临时同进程unwind观察通过且撤回。
round1格式化中止与临时runner事后重建来源保留；旧长测/Orin wire4 FAIL不提升为
本轮同源PASS，未重跑长测/67场景/硬件，也未完成双队列生产总装或关闭D15。

P-M响应schema准备包已 **APPROVED_SOFTWARE_SCOPE**：base `5abd148`，round3九门通过，
350项来源`4617c60d…6f524`一致，恰8改+4新增。msg lib34、compat34、doc2通过/2既有ignored，
workspace1932通过/1既有ignored；旧五builder20组byte-equality与typed80组循环不重复计数。
独立源码/证据及root范围核验通过，证据`local/s48/pm/review.md`；真实timing与dep-info绑定
的gen_root已保留。新增副作用/退休字段与typed builder，旧API/字节不变，v0与新类型隔离。
round1格式化中止、round2生成常量误用失败保留；无生产新调用者或Orin验证，D15仍OPEN。

后续方案新增验证要求（**Plan Approved，尚无运行PASS**）：容量O1用可控无并发夹具
钉3840/7936总len门；长负载仅对Normal cache Capacity核该阈值，wire/queue等
提前失败按真实原因记录，不能充当缓存EXPECTED_RED。peer额度与实际业务忙分开，
SetEnabled新domain须测旧新schema的实际未知数值行为。详PLAN P.6.14.8–10。

优先级原语函数分包r4已独立Plan Approved（P.6.14.12）；P-M已按上文验收，其他分包尚待实施。
新增门须核外部crate公开BusLease的真实获取、错域返原凭证、正确归还及Drop不释放；
固定域槽的两轮复用、take/withdraw竞争与Execution重复读取；目标原子约束不等于
wait-free。P-M修改build.rs须附实际workspace --timings报告；旧v0依赖/默认尾字段/
未知数值和旧builder兼容仍需真实测试。专项门不能代替每包fmt-all/workspace Clippy。

后续生命周期r6及监督r4仅获独立**方案批准**，不算新增运行PASS。实施后必须分别
验证请求许可单次转移/释放、早回复与E2未知占位、已判超时不变迟到成功、真实满
队列及逐跳保留；监督须核阶段/截止来源、连续未settled区间CAS两种胜负与重置、
Active异常返还义务、真实Lower ACK/逐线程stop、helper join Err、残余句柄无隐式
阻塞Drop、预绑定备用Store只在真实join后写入且保留新Pending。历史sticky与当前
health分开；持续失败/重投耗尽不能冒充真实退休。具体合同见PLAN P.6.14.7。

C5 结论与逐项命令/日志索引为 [C5.json](../.verify/post_e_comm_fault_hardening/C5.json)，
对应归档为 [C5_evidence.tar.gz](../.verify/post_e_comm_fault_hardening/C5_evidence.tar.gz)。
查看原始 run / 执行器时，先按 PLAN P.6.5 将证据包解到仓库根；日常验收命令仍在 PLAN。
旧轮次日志、重复产物和历史归档按用户授权清除；当前 H 阶段的两项软件首红与
两次真实 H2 FAIL 作为修复依据，随唯一 H 包保留。

### 阶段历史（按当时状态保留）

- 实现前清单为 [baseline.json](../.verify/post_e_comm_fault_hardening/baseline.json)：
  20 个 integration target、155 项用例，其中 2 项原有 ignored；枚举不算执行通过。
- 实现前实跑：lib / bins 1345 PASS；RPC 五组 integration 45 PASS，显式指向本次
  debug 构建的 S0 / S1 / gateway。此处仅记录旧代码基线。
- 新增重点：未提交请求超时与异常 Busy 残留、带标签 abort 的 CAS 清理、EStop
  与事务入口两种先后顺序、RR 两道 deadline、启动首 IO 发布门与信任文件屏障。
- 收口需重跑保留用例、debug / release 的 `rt_guard_soak` ignored、隔离 smoke；
  本机条件不足或需要 Orin 现场的项如实记录 `NEEDS_HUMAN`，不得写 PASS。
- Plan 核码澄清见 CHANGELOG 2026-09-08。D15 保持 OPEN，不据本地测试宣称无人值守。
- C2 分包实跑：workspace 全 feature 的 lib / bins 为 1389 PASS；Feetech
  全 feature 145 项库测试 + 12 项独立 integration PASS（库内另含 11 项独立
  有界传输验收）。默认 / 全 feature clippy 完整输出零警告，schema compat 16 PASS，
  real-hw example 编译通过。尚未完成 C3/C4 的运行时收敛与总装，不能替代整候选验收。
- C3 独立评审已批准，S0 库 337 PASS（含 root 34 项独立真实 tick / phase 验收），
  全 48 位 HardRT 兼容性复核通过。liveness 与 bound 超时记录先复现再修正；
  分包 fmt / 两组 clippy / build 零警告，workspace lib / bins 1428 PASS；
  完整 IPC / startup / 进程验收归 C4–C5，证据见 `.verify/post_e_comm_fault_hardening/C3.json`。
- C4 S-41 新增 AC-11(aa)：已 admission live RR 急停取消前缀、dry / expired
  消耗配额、source / 后续新 admission 不误杀、SSF only 与 both 响应对照；首批独立用例通过。
- C4 S-42 新增 AC-11(ab)：Submitted / Unsubmitted timeout、abort→inhibit
  交接读序、初始 guard 刷新后 mailbox 到达；同 tick ClearError 不得恢复，独立反例已过。
- C4 中间验证：marker 9/9、IPC 独立 26/26、完整 S0 lib 370/370、startup 8/8、
  既有 SerialRT integration 5/5 PASS；workspace lib/bins 1461 PASS。
  S-43 的 READY 前后真实 IPC 断连已闭环；追加 RPC 兼容首红为旧启动日志遗漏，
  已恢复并同步双臂锁目录 / unit 退出码断言。最终 RPC 45、unit 16 全过；
  fresh fmt / 双 workspace Clippy / 1461 库-bin测试 / build 全过，独立 **C4 Approved**，
  证据见 `.verify/post_e_comm_fault_hardening/C4.json`。C5 完整软件与真机验证仍未完成。
- C3 新增 S-40 交错验收：pending 同槽取消保留 seq；标记后暂停、标记 CAS 败给
  serial 消费、重复发布者与取消完成后迟到 timeout。已纳入 C3 的实际通过记录。

## 0. 使用约定

### 状态标签

| 标签 | 含义 |
|---|---|
| ✅ | 指标已达成，baseline / regression threshold 已写死 |
| 🟡 | 已有测试但指标未实测 / 未落 CI |
| ⏳ | 测试尚未落地（Step 未到） |
| ⚠️ | 实测未达标 / 有待开的债 |

### 更新触发

- 每完成一个 Step：过一遍本文件，把该 Step 解锁的指标状态从 ⏳ → 🟡 或 ✅
- 基线数值实测：在《表》里填 `measured` + `measured_at` + `run_env`
- 回归阈值变动：走 `docs/CHANGELOG.md` 登记 + 更新本文件《10.1 硬指标》表

### run_env 约定

实测值必须同时记录机器 / 内核 / CPU 频率治理器 / iceoryx2 版本，否则不可比较。
本文件用 `env_id` 字符串代称，详解见底部《附录 · 实测环境登记》。

---

## 10.1 硬指标（与 `00_design.md` §10.1 1:1 对齐）

### 10.1.1 S0 loop

| 指标 | 目标 | measured | measured_at | env_id | 状态 | Step |
|---|---|---|---|---|---|---|
| target rate | 1 kHz | — | — | — | ⏳ | Step 6 |
| p99 latency | < 1.5 ms | — | — | — | ⏳ | Step 10 (Orin) |
| max latency (1 h soak) | < 5 ms | — | — | — | ⏳ | Step 10 |
| watchdog timeout path | < 2 cycles | — | — | — | ⏳ | Step 7 |
| RT thread allocation count (after init) | = 0 | — | — | — | ⏳ | Step 6 |
| RT thread `Arc::clone/drop` count | = 0 | — | — | — | ⏳ | Step 6 |

`docs/00_design.md` §4.9 / §10.1 定义；检测钩子 = `robot-tests::rt_arc_guard` + `assert-no-alloc`（§10.2）。

### 10.1.2 S1 loop

| 指标 | 目标 | measured | measured_at | env_id | 状态 | Step |
|---|---|---|---|---|---|---|
| target rate | 100 Hz | — | — | — | ⏳ | Step 6 |
| p99 latency | < 15 ms | — | — | — | ⏳ | Step 10 |
| policy timeout 不阻塞 coordinator | kill -9 policy host → coordinator 仍 100 Hz | — | — | — | ⏳ | Step 9 |

### 10.1.3 Transport

**Measured baseline**（v0.1 必产，v0.2 起作 regression 基准；按机型分别 histogram）：

| 场景 | 目标 | measured p99 | measured_at | env_id | 状态 |
|---|---|---|---|---|---|
| 512 B message p99 (same host) | baseline | — | — | — | 🟡 Step 5 已具备测试入口，数值待 bench |
| 3 MiB blob p99 | baseline | — | — | — | ⏳ Step 6 加 blob 类 topic 后 |
| 1 h soak command topic drop | `== 0` (hard) | — | — | — | ⏳ Step 9 soak harness 落地 |
| **iceoryx2 publish→receive p99**（RT 内核 + `SCHED_FIFO 70` + CPU pin + mlockall，50 Hz × 10 min）| ~~< 100 µs (hard)~~ | — | — | — | ❌ **hz≥50 判据无效（F7 拍板 2026-09-01，PX-59）**：该场景实测尾部为速率错拍伪影非调度延迟，不 block、不作 gate；新判据待 F7 判据小件 |
| **iceoryx2 publish→receive p999**（同上）| ~~< 500 µs (hard)~~ | — | — | — | ❌ 同上（F7 / PX-59） |

**F7 拍板（2026-09-01，PX-59）**：上两行阈值在发布频率 ≥ 50 Hz 下测得
的是 harness 速率错拍伪影（变频判别实证，见 demo summary 附录 A.1），
**不再作为调度 gate**。`iceoryx2_rt_baseline` bin 仍按内置阈值默认输出
FAIL / exit 1——在 F7 判据小件完成前**该输出仅是 legacy 诊断**，不得
用于 ≥50 Hz 的调度判定；原始数据保留，新判据等错拍机制定位后另拍。

**iceoryx2 RT baseline 指标出处**：`docs/CHANGELOG.md` 2026-04-24 "§4.7 policy_host 调度等价 inline" 条目第 2 项决定。测试需在 Linux RT 内核（5.10 RT / 6.1 RT）+ `chrt -f 70` + `taskset` CPU pin + publisher / subscriber 两端均 `mlockall` 下完成，非 RT 或 macOS 测的数据不作 baseline。

**硬约束（不受绝对值影响）**：

| 约束 | 状态 |
|---|---|
| command topic drop count == 0 | ⏳ Step 9 soak |
| sensor topic drop allowed but counted | ⏳ Step 6 metrics hook |
| `unknown_layout_count == 0`（warmup 后） | ⏳ Step 6 |

**v0.2+ regression threshold**（基线确定后启用）：

| 回归项 | 阈值 | 状态 |
|---|---|---|
| command topic p99 | 相比 baseline 回退 ≤ 30% | ⏳ |
| blob topic p99 | 相比 baseline 回退 ≤ 30% | ⏳ |

**Step 5 已落的 transport 层可观测项**（作为未来 bench 的测量点）：
- `robot-transport::ParseMetrics`：每种 `ParseError` 变体独立 `AtomicU64`
- `Iceoryx2Publisher::loan` 早期拒绝 → `TransportError::SampleTooLarge`（size > tier cap）
- `Iceoryx2SampleWriter::iceoryx2_publish` → `loan_slice_uninit` → `send`（三段可打点）

### 10.1.4 Safety

| 指标 | 目标 | measured | measured_at | env_id | 状态 | Step |
|---|---|---|---|---|---|---|
| EStop RPC → S0 latched | < 20 ms | — | — | — | ⏳ | Step 8 |
| S1 crash → S0 safe output | < 250 ms | — | — | — | ⏳ | Step 7 |
| RPC gateway crash → S0/S1 | 0 tick drop | — | — | — | ⏳ | Step 9 |
| physical estop button → latched | < 5 ms | — | — | — | ⏳ | Step 10 |
| EStop latch 持久化往返（重启 S0 后仍 latched） | 必过 | ✅ 23 unit tests | 2026-04-25 | local | ✅ | Step 8 |
| EStop latch 双文件 fail-over（primary 损坏/缺失 → backup 自愈） | 必过 | ✅ 同上 | 2026-04-25 | local | ✅ | Step 8 |
| kill -9 s0 → 硬件 fail-safe（torque disable/zero/brake） | < 500 ms | — | — | — | ⏳ blocking gap | Step 10 / task-30-post |

### 10.1.4.1 Adapter RT baseline（按 RtClass 分级）

> **task-30 v9（pre-1d）**：原 §10.1 只覆盖 HardRT / SoftRT 假设。task-30 引入
> `RtClass::SerialRT`（§4.6 Adapter 分级），feetech 串口舵机由 HardRT 改标
> SerialRT，挂 `s0_serial_io_thread`。本节列 SerialRT 专属 SLA + baseline 项。

| 指标 | 目标 | measured | measured_at | env_id | 状态 | Step |
|---|---|---|---|---|---|---|
| **HardRT** `read_state` p99 | < 50 µs | — | — | — | ⏳ | Step 6 mock |
| **HardRT** `write_command` p99 | < 50 µs | — | — | — | ⏳ | Step 6 mock |
| **HardRT** `safety_stop_rt` (RT-only fast path) p99 | < 100 µs | — | — | — | ⏳ | M5 mock + Step 10 |
| **SerialRT** `feetech_poke ping`（9-motor handshake，逐 motor） | baseline（毫秒量级）| ✓ 9/9 ping ok | 2026-04-26 | Orin AGX bus2 5A7C122340 | 🟢 task #15 Stage 1 实测 | Step 10 |
| **SerialRT** `safety_stop_fast`（6-motor sync_write Torque_Enable=0）| **< 10 ms** (hard) | < 1 s 端到端断扭（broadcast 已生效）| 2026-04-26 | 同上 | 🟢 端到端真断扭 | M4 落地 + Orin 实测 |
| **SerialRT** `confirm_torque_off` RTT（6-motor sync_read 验证 Torque_Enable=0）| **< 50 ms** (hard) | post-shutdown 全 0（ms-level 实测留 follow-up）| 2026-04-26 | 同上 | 🟢 hardware 真断扭确认 | M6 落地 + Orin 实测 |
| **SerialRT** `latch fsync` 触发 → 双文件落盘 | < 200 ms（hard，daemon 仍 active）| ✓ < 200 ms | 2026-04-26 | 同上 | 🟢 §5/§7 两次复测 | task #15 阻断 1 |
| **SerialRT** `read_state`（6-motor sync_read Present_Position+Velocity）p99 | < 8 ms | — | — | — | ⏳ p99 数采集留 follow-up | Step 10 |
| **SerialRT** `write_command`（6-motor sync_write Goal_Position）p99 | < 4 ms | — | — | — | ⏳ p99 数采集留 follow-up | Step 10 |
| **SerialRT** ballet（6-motor 50 Hz cmd 12 s 全程）| daemon Enabled.Active 持续 | ✓ 8 s+ Enabled，motor 2/4 偏移 +3-4° (±5° amp) | 2026-04-26 | 同上 | 🟢 §4 物理摆动确认 | task #15 Stage 1 |
| **SoftRT** snapshot 更新到 RT thread 可见的 staleness | < 50 ms | — | — | — | ⏳ | v0.4 RealSense |

**SerialRT 测量约束**：
- 必须在 Orin（aarch64 + USB-RS485 实机）跑，dev 机数据**不作 baseline**；
- `safety_stop_fast` / `confirm_torque_off` 跑 100 次取 p50 / p99 / max；
- 触发条件 = `s0_daemon` 已 spawn `s0_serial_io_thread` + Feetech adapter connected
  + bus 上 motor 全 ping 通过；
- baseline 采集时机 = M6 落地后即刻；M7 真机 smoke 跑通过夜间 1 h soak 复查。

**2026-04-26 task #15 Stage 1 实测 baseline**（bus2 right_arm 6 motor + dimos 留扭）：
- §2 9 motor (1-9) 全部 `ping ok: motor_id=<id> ... (Model_Number=STS3215 777)`
- §3 smoke 5s daemon connect 仅，**不**调 enable，motor `Torque_Enable` 维持 baseline (1)，state=Ready
- §4 ballet 12s（mock_s1 anchor 1s + cmd_seq=788）：state Ready→Enabled.Active@t=6s 持续 8s+，post-run motor 2/4 偏 +3-4°（±5° amp 内）
- §5 EStop trigger：daemon active T+200ms 时 latch primary+bak 都 = "1"（fsync 双写已落盘），post-shutdown 6 motor `Torque_Enable=0` 全
- §6 latch=true 重启：`initial estop_latched=true`，state 全程 Safe，hb 进来后仍**不**进 Ready/Enabled
- §7 删 latch 重启：`estop_latched=false` → 第二次 ballet → 第二次 EStop → motor 全 0
  （**⚠️ pre-strict 历史记录**：reviewer round 4 commit `4222df5` 之后默认
  strict fail-closed（A1-5 撤 `--allow-empty-latch` opt-in 后唯一行为）让
  "rm latch && restart" 拒启动，此条 baseline 行为不可在当前 v0.2 复现；
  当前合法 latch=0 重置走 `scripts/systemd/README.md` provision 流程，
  **不**等价于 ClearEStop — 详 docs/CHANGELOG.md 2026-04-26 round 4/5
  条目 + A1-5 commit）

**未填项**（follow-up）：`read_state` / `write_command` p99 ms-level 量化采样（要在 daemon hot path 加 latency
计数 + 100 次抽样）；safety_stop_fast / confirm_torque_off 单独 ms-level 计时（当前只验"端到端 < 1s 断扭"，没拆 sync_write
本身耗时）。这两条留在 task #15 Stage 2 / Step 10 完成。

### 10.1.5 Schema

| 指标 | 目标 | 状态 | Step |
|---|---|---|---|
| old reader reads new writer（所有 v0 组合） | 全通 | 🟡 已落 `robot-msg/tests/compat.rs`（Step 2），v0 只含 JointState；新增 schema 时需补 | Step 2 已做 |
| new reader reads old writer（所有 v0 组合） | 全通 | 🟡 同上 | Step 2 已做 |
| deprecated 字段 id 不可复用 | CI lint 拦截 | ✅ `robot-msg/src/bin/schema_lint.rs`（Step 2） | Step 2 |
| `layout_id` 碰撞检测 | CI lint（固定测试向量） | ✅ `schema_lint` 跑 blake3（Step 2） | Step 2 |
| `SCHEMA_REGISTRY` 严格递增 | CI 单测 | ✅ `schema_id::tests::registry_ids_are_unique_and_monotonic` | Step 2 |
| 每个 `schema_id` 必须有 `SizeTier` | CI 单测 | ✅ `size_tier::tests::every_registered_schema_has_a_tier`（Step 5） | Step 5 |

---

## 10.2 测试基础设施

### 10.2.1 Harness 清单

| Harness | 位置 | 用途 | 状态 | 触发 Step |
|---|---|---|---|---|
| `cyclictest` 1 h + 直方图 | 外部工具（cyclictest deb 包） | RT 抖动 baseline | ⏳ | Step 6 Linux / Step 10 Orin |
| `iceoryx2 rt_baseline` | `crates/robot-tests/src/bin/iceoryx2_rt_baseline.rs`（Step 6 新建）| 50 Hz × 10 min publish→receive 时刻直方图，配 SCHED_FIFO 70 + CPU pin + mlockall | ⏳ | **Step 6 必产** |
| `assert-no-alloc` 钩子 | RT thread 入口 | CI debug+release 跑 0 alloc | ⏳ | Step 6 |
| `robot-tests::soak` | `crates/robot-tests/src/bin/soak.rs`（未创建） | mock 500 Hz state + 200 Hz command / 1 h / 24 h | ⏳ | Step 6 后 / Step 9 |
| `robot-tests::crash_restart` | `crates/robot-tests/src/bin/crash_restart.rs`（未创建） | 随机 `kill -9` S1 / policy / gateway | ⏳ | Step 9 |
| `robot-tests::schema_compat` | 当前由 `crates/robot-msg/tests/compat.rs` 覆盖 | old/new 交叉解析 | 🟡 v0 层已覆盖；跨 crate 版本交叉推到 v0.3 | Step 2 部分 / v0.3 补 |
| `robot-tests::rt_arc_guard` | 未创建 | RT thread 内 `Arc::clone/drop` 计数 | ⏳ | Step 6 |
| `hello_pubsub` | `crates/robot-tests/src/bin/hello_pubsub.rs` | Step 5 最小闭环演示 | ✅ | Step 5 |
| `feetech_bench` | `crates/robot-adapters/feetech/benches/feetech_bench.rs`（task 27c）| criterion 5 组：encoding / protocol / parse / bus / bus_sync(13motor) | ✅ | Step 7 task 27c |
| `feetech::integration_smoke` | `crates/robot-adapters/feetech/tests/integration_smoke.rs`（task 27c）| 13-motor × 1000 循环 sync_read+sync_write + 跨 0 边界 + handshake | ✅ | Step 7 task 27c |
| `scripts/feetech_flamegraph.sh` | 同上 | 封装 cargo flamegraph 跑 bench 长循环 + perf record + SVG | ✅（脚本就绪；首次运行需 `cargo install flamegraph` + `perf_event_paranoid`） | Step 7 task 27c |

### 10.2.2 现存单元 / 集成测试清单（Step 5 完成时刻快照）

| Crate | 测试 | 计数 | 覆盖 |
|---|---|---|---|
| `robot-core` | `time::tests` | 33 | `RobotClock` / `Deadline` / `MonoRawNs` 代数 / Mock / `SystemClock` 单调 |
| `robot-msg` | `schema_id` + `capability` + `size_tier` | 15 | registry 单调 + 穷举 tier + capability flag 位 |
| `robot-msg` | `tests/compat.rs` | 13 | v0 ↔ 当前 JointState 交叉 + deprecated id 拦截 |
| `robot-transport` | unit | 12 | parser / sample / writer / metrics |
| `robot-transport` | `tests/iceoryx2_roundtrip.rs` | 4 | 真后端 roundtrip / size 超档 / attribute mismatch / Debug |
| `robot-transport` | `tests/sample.rs` | 20 | mock round-trip + SchemaMismatch + BlobMisaligned + 资源计数守恒 |
| `robot-transport` | `tests/iceoryx2` 混入 | 10 | qos + service_name + 类型层 |
| `robot-transport` | `tests/trybuild.rs` | 1 test，编译期 3 条负向 | `MessageView` outlive lease / publish 后写 / 二次 loan |

### 10.2.3 CI 顺序（严格串行，全程 nightly）

| # | 步骤 | 状态 |
|---|---|---|
| 1 | `cargo fmt --all --check` | ✅ `.github/workflows/ci.yml` |
| 2 | `cargo clippy --all --all-targets --all-features -- -D warnings`（含 pedantic） | ✅ |
| 3 | `cargo test --workspace --all-features` | ✅ |
| 4 | `cargo test -p robot-transport --no-default-features --features mock`（iceoryx2 off 保底） | ✅ |
| 5 | `cargo build --release --workspace` | ✅ |
| 6 | `cargo deny check`（advisories / bans / licenses / sources） | ✅ 独立 job（`deny.toml`） |
| 7 | `allocation-guard`（`assert-no-alloc` + RT thread 入口钩子） | ⏳ Step 6 |
| 8 | `schema_compat`（跨版本 crate round-trip） | ⏳ v0.3 |
| 9 | 夜间：soak 1 h / 24 h + `cyclictest` + crash/restart | ⏳ Step 9+ |

**CI 配置文件**：`.github/workflows/ci.yml`。matrix 覆盖 `ubuntu-latest` + `macos-latest`（Step 0–5 两平台都跑）。Step 6 起 Linux-only 工作另开 workflow（`SCHED_FIFO` / `mlockall` 等不可在 macOS 跑）。

---

## 10.3 Record / Replay 格式

### 10.3.1 记录必须保存的字段（§10.3 原样引用）

| 字段 | 语义 | 状态 |
|---|---|---|
| `topic` | iceoryx2 ServiceName 原字符串 | ⏳ Step 9+ recorder |
| `schema_id` | `FrameHeader.schema_id` | ⏳ |
| `schema_version` | `FrameHeader.schema_version` | ⏳ |
| monotonic `stamp_ns` | `FrameHeader.stamp_ns`（`CLOCK_MONOTONIC_RAW`） | ⏳ |
| receive `stamp_ns` | recorder 本地取样时刻 | ⏳ |
| `seq` | `FrameHeader.seq` | ⏳ |
| header bytes | 原始 `FrameHeader` FlatBuffers | ⏳ |
| body bytes | 原始 body FlatBuffers | ⏳ |
| blob bytes 或 blob 文件 offset | 两种存储形式都支持 | ⏳ |
| dropped sample marker | recorder 发现 seq 跳号时插入 | ⏳ |
| clock info（monotonic → wall offset 采样） | 录制开始 + 结束 + 中间抽样 | ⏳ |
| robot config hash | `ConfigIdentity.config_hash` | ⏳ Step 8+ |
| calibration hash | `ConfigIdentity.calibration_hash` | ⏳ |
| session_id | 每次运行 uuid | ⏳ |
| active `layout_id` → `JointLayout` 映射表 | 录制期间所有出现过的组合 | ⏳ |

### 10.3.2 回放模式

| 模式 | 语义 | 状态 |
|---|---|---|
| `as-fast-as-possible` | 忽略时间戳最大速度重放 | ⏳ |
| `real-time` | 按原 `stamp_ns` 间隔重放 | ⏳ |
| `scaled-time` (x2, x0.5, …) | 按倍速缩放重放 | ⏳ |
| `step-by-step` | 交互式逐帧 | ⏳ |
| `topic remap` | 回放到不同 topic | ⏳ |
| `robot_id remap` | 回放到不同机器人实例 | ⏳ |

### 10.3.3 长期互操作

- 短期：自定义二进制格式（body / blob concat + index 文件）。
- 长期：对接 MCAP / LeRobotDataset 导出器（§13 开放问题 5）。

Recorder crate (`crates/robot-recorder/`) 规划时机：v0.3 – v0.4，具体排期见 `docs/01_implementation_steps.md`（本文件不重复）。

---

## 10.4 v0.2 退出标准（本文件视角）

Step 10 完成 = v0.2 达成。以下 **所有** 指标必须从 ⏳ / 🟡 升到 ✅：

- [ ] 10.1.1 S0 loop 全部（1 kHz / p99 / max / watchdog / alloc / arc）
- [ ] 10.1.2 S1 loop 全部
- [ ] 10.1.3 Transport baseline + 1 h soak drop=0 + unknown_layout=0
- [ ] 10.1.4 Safety 全部（含物理按钮 < 5 ms + kill -9 s0 硬件 fail-safe < 500 ms）
- [ ] 10.1.5 Schema 全通
- [ ] 10.2.1 全部 harness 状态 ✅
- [ ] 10.2.3 CI 1 – 9 条全通
- [ ] 10.3 recorder 至少 `real-time` + `as-fast-as-possible` 两模式可用（其余推 v0.3）

⚠️ 当前（Step 5 完成时）距离 v0.2 退出标准还差 Step 6–10 的全部工作量 + 本表绝大多数行。

---

## 附录 · 实测环境登记

随实测推进在此追加条目。建议字段：

```
env_id        macos-m4pro-nightly-20260422
host          Apple MacBook Pro M4 Pro, 64 GB
os            macOS 15.4 (Darwin 25.4.0)
cpu governor  N/A (macOS)
iceoryx2      0.8.1
rust toolchain nightly-2026-04-22 (或当时具体日期)
测量场景       Step 5 同进程 pub/sub
测得项        <列表>
备注           macOS 仅作 Step 0–5 开发期支持；§10.1 绝对值不作 baseline
```

初始化：

```
env_id        macos-m4pro-nightly-20260422
host          Apple MacBook Pro M4 Pro（详 CPU 型号补）
os            Darwin 25.4.0
cpu governor  —
iceoryx2      0.8.1
rust toolchain nightly（Cargo.toml edition=2024；未 pin 具体日期）
测得项        (仅定性) hello_pubsub 同进程 roundtrip OK；无 drop；
              size tier cap 早期拒绝正确；attribute 不匹配 open 被拒
备注           不作为 §10.1 baseline —— 设计文档明确要求按 Orin / Linux
              x86_64 两档实测
```

### feetech bench baseline（Step 7 task 27c 实测，Linux x86_64 dev 机）

```
env_id        linux-x86_64-nightly-2026-04-24-feetech-bench
host          (dev 机；CPU 型号 + governor 待 collect_env_id.sh 补)
os            Linux 6.17（PREEMPT，非 PREEMPT_RT）
rust toolchain nightly（rust-toolchain.toml channel = "nightly"）
profile       release（lto=fat / codegen-units=1）
criterion     0.5.1
测量场景       criterion `--warm-up-time 1 --measurement-time 2`（短跑，仅作回归 baseline）
```

| 组 / case | 中位数 |
|---|---|
| encoding/encode_sign_magnitude_pos | 494 ps |
| encoding/encode_sign_magnitude_neg | 492 ps |
| encoding/decode_sign_magnitude     | 368 ps |
| encoding/split_bytes_le_2 / _4     | 184 / 183 ps |
| encoding/combine_bytes_le_2 / _4   | 367 / 368 ps |
| protocol_build/build_ping          | 560 ps |
| protocol_build/build_read          | 562 ps |
| protocol_build/build_write_2byte   | 966 ps |
| protocol_build/build_sync_write_13motor_2byte | 33.2 ns |
| protocol_build/build_sync_read_13motor        | 14.5 ns |
| parse/parse_status_empty           | 1.50 ns |
| parse/parse_status_2byte           | 1.58 ns |
| parse/checksum_6byte               | 1.28 ns |
| bus/write_goal_position_1motor     | 73.7 ns |
| bus/read_present_position_1motor   | 66.2 ns |
| **bus_sync/sync_read_present_position_13motor**  | **866 ns** |
| **bus_sync/sync_write_goal_position_13motor**    | **240 ns** |

**判断**：protocol / encoding 层零瓶颈。13-motor sync_read 端到端 866 ns ≈ 1 µs，
远小于 50 Hz tick (20 ms) 预算；task 28 真机 Orin USB-RS485 实测的延迟瓶颈
**几乎全部在 wire RTT**（1 Mbps × packet ~50 B / motor → 几 ms 量级），不在 Rust
协议层。这是上 Orin 真机前的"协议层无回归"基线。

### SerialRT FIFO opt-in（XLeRobot 部署）——验证与回滚口径（2026-09-01）

§4.6 偏离登记见 `CHANGELOG.md` 2026-09-01（Spec P.4 PX-56/57）。
**默认口径：不传 `--serial-fifo` 时 SerialRT 线程仍为普通优先级，与
00_design.md §4.6 逐字一致**；FIFO 仅 XLeRobot 双臂 unit opt-in。

**验证**（Orin，栈起后；`pid=$(systemctl show -p MainPID --value
xlerobot-s0-<side>-arm.service)`，两层取证）：

1. 调度类 / 落核：`ps -L -p "$pid" -o pid,tid,comm,cls,rtprio,psr`
   → `s0_serial_io_th` 行 `cls=FF rtprio=70 psr=4|5`；其余线程 `TS`。
   （默认 `ps -Lp` 不显示这三列，必须带 `-o`。）
2. 每线程亲和掩码：遍历 `/proc/$pid/task/*/status` 读
   `Cpus_allowed_list` → serial 线程 = `4`（左）/ `5`（右），
   其余线程全部 `0-3`（unit `CPUAffinity=0-3` 继承）。
3. `supervised_demo --phase read-state` PASS + 起栈窗口
   `journalctl -b` 无 `TransportTimeout`。
4. guard 计数 `/run/robot-system/preflight/*.count` 保持 `0\n`。

**回滚**（不动代码，unit 面即可）：

1. 最小回滚（只撤 FIFO）：双臂 unit ExecStart 撤 `--serial-fifo 70`
   （`--cpu`/`--strict-rt` 可留；strict 检查只对已启用项执法）→
   `daemon-reload` + 重启栈 → serial 线程回普通优先级（TS）。
2. 完整回退（还原 `48cc4a9` 前 unit 全文）：ExecStart 撤
   `--serial-fifo 70`、`--cpu 4|5`、`--strict-rt` **三个参数**；删
   `LimitRTPRIO=99`、`LimitMEMLOCK=infinity`、`CPUAffinity=0-3` 三行；
   恢复 `RestrictRealtime=yes`；`daemon-reload` + 重启栈。
3. 内核侧独立回滚：`/boot/extlinux/extlinux.conf.bak-20260901` 还原
   isolcpus（与本偏离正交，非必需）。

### 待登记

- `linux-x86_64-nightly-YYYYMMDD`：Step 6 开工用
- `orin-agx-nightly-YYYYMMDD`：Step 10 前必须

---

## 变更记录

| 日期 | 变更 | 作者 |
|---|---|---|
| 2026-09-25 | 右H2原整体FAIL与后到通电答复保留；ca14f0e Orin原生构建/mock/定向单测仅软件通过，未正式安装或硬件重测，D15 OPEN；按用户要求交接并暂停 | Codex |
| 2026-09-15 | RR R2正式26门、源码独审及真实故障验证通过；R3/R4/Orin待验证 | Codex |
| 2026-09-14 | RR R1本地21门/CAL45/模型6通过；源码与证据独审，R2/R3/R4及Orin未验证 | Codex |
| 2026-09-14 | RR R0 两原包来源/唯一构建图及99项transport、304项RPC回归通过；strict/模型/Orin未验证 | Codex |
| 2026-09-10 | W4b固定五例SIGKILL/原目录恢复独立验收；16门、5+2+72通过，原失败/ignored保留，提交后暂停 | root + 独立B源码/证据审 + A身份二路 |
| 2026-09-10 | W4a正式15门/新增2例/原All72独立终审通过；原统计失败及限定更正保留，后续W4/RR/P-E未完成 | Codex |
| 2026-09-10 | W3生产总装31门/All72/72独立源码与证据终审通过；原失败保留，W4/P-E及Orin待验证 | Codex |
| 2026-09-09 | A3b快照数据竞争修复独立批准；22本地门及原始证据核验通过，Miri范围、ignored专项与Orin未测边界分别记录 | Codex |
| 2026-09-08 | S-45 两端软件验收独立批准；左 H2 首次 FAIL 保真，新物理边界后重测 | Codex |
| 2026-09-08 | 当前 post-E 验收入口、实现前 Cargo 清单与实跑基线；新增 S-39 / D3 / D4 / D13 验收范围，未标新增项通过 | Codex |
| 2026-09-08 | C2 有界传输与 adapter 分包验证；独立测试实际执行，整候选门仍待 C3–C5 | Codex |
| 2026-09-08 | S-40 同槽取消标记与确定性交错验收登记，方案已 ACK，未宣称新增测试通过 | Codex |
| 2026-09-08 | 当前 e6d2b306 / 308 的本地与 Orin 软件验收独立批准；统一 C5.json / C5_evidence.tar.gz 入口，按用户授权清除旧过程数据；H1–H3 与 D15 状态不变 | Codex |
| 2026-09-08 | S-44 的 d6963ed0 / 308 两端工具门、Orin 两核 mock S0 RT 与左 H1/H3 独立批准；H2 和右臂未测，D15 OPEN；证据统一 H.json / H_evidence.tar.gz | Codex |
| 2026-04-22 | 骨架初稿：§10.1/§10.2/§10.3 三级结构 + 状态标签 + env_id 机制；所有数值行占位。Step 5 完成时刻快照 | Claude Code (提议) / loosqk (拍板) |
| 2026-04-22 | §10.2.3 CI 1–6 步接入 `.github/workflows/ci.yml`；`deny.toml` 落地（allow 0BSD；workspace path wildcard + private crate 豁免；iceoryx2 / trybuild winnow dup 走 skip-tree）；`scripts/collect_env_id.sh` 兼容 macOS + Linux。TODO 1/3/4 划掉，只剩 2（assert-no-alloc） | Claude Code |
| 2026-04-24 | §10.2.1 加 3 个 task 27c harness（`feetech_bench` criterion / `feetech::integration_smoke` / `scripts/feetech_flamegraph.sh`）+ 附录加 feetech bench baseline 实测数值。结论：协议层零瓶颈，task 28 真机延迟瓶颈在 USB-RS485 wire RTT。 | Claude Code |
| 2026-09-01 | 附录加 SerialRT FIFO opt-in 验证与回滚口径（§4.6 偏离 PX-56/57 的验证面；默认普通优先级不变） | Claude Code (codex 复审要求) / loosqk (拍板) |
| 2026-09-01 | F7 拍板同步：p99/p999 两行 hard gate 在 hz≥50 宣告无效（PX-59）；bin 默认 FAIL 降级为 legacy 诊断（判据小件前） | Claude Code (Reverse Sync) / loosqk (拍板) |

---

## TODO（与本文件直接相关，尚未落）

1. ~~**CI 接入**：`.github/workflows/ci.yml` 骨架按 §10.2.3 九步串行写；Step 6 开工前完成。~~ → ✅ 已落（`.github/workflows/ci.yml`，ubuntu + macos matrix，第 1–6 步串行 + `cargo deny` 独立 job）
2. **`assert-no-alloc` 集成**：`crates/robot-tests::rt_arc_guard` / `robot-s0::rt_thread` 启动期全局安装；Step 6 必须。
3. ~~**cargo-deny 配置**：`deny.toml`（版本 / 许可证 / 重复依赖）Step 6 前加，拦截未经登记的新依赖。~~ → ✅ 已落（`deny.toml`，`cargo deny check` 本地 + CI 均过）
4. ~~**env_id 自动采集脚本**：`scripts/collect_env_id.sh`（uname / sw_vers / sysctl / rustc -V / cargo tree 合成 id），实测时一键生成登记条。~~ → ✅ 已落（`scripts/collect_env_id.sh`，macOS + Linux 兼容）

- 2026-09-08：新增全部 Cargo 依赖升级分包的最终锁文件与验证入口；不替代 C5 / 现场验收。
- 2026-09-09：登记S-48 A2已提交与A3a共享权限归位的本地门、root评审和证据边界；未接生产或验证Orin。

- 2026-09-09：登记P-T九门、独立终审与同名双配置红→绿证据；未接生产或验证Orin。

- 2026-09-09：登记P-S1/P-L1十一门、独立终审与真实分配守卫正反证；未接生产或验证Orin。

- 2026-09-10：登记P-A1九门、独立终审与真实退休竞争/固定槽禁分配证据；S0文档未运行，生产与Orin待验证。

- 2026-09-22：登记左H2前置拒绝的软件修复、两次原生来源与最终只读复检；保留原FAIL，不外推硬件通过。

- 2026-09-22：登记测试hook同RobotClock有界补足与停止后final_trace门，两端57及Orin原生/AC12通过，原controller FAIL保留；最终独审/root验收已通过，旧H2 FAIL不变。

- 2026-09-22：登记5952498新原生来源、64+10/唯一AC12及122原件独审；仅软件，证据已归档，左H2历史来源不变。

- 2026-09-22：登记右H13本地监督/oracle的22开发+12独立方法验证；保留round6 FAIL和早轮回执边界，右现场未执行。

- 2026-09-22：登记右H13 pidfd能力兼容的本地27/独立5/Orin27与默认入口；首22方法3FAIL/3ERROR保留，仅软件、已归档。

- 2026-09-23：登记右H1/H3各100+1000及真实回收/清场；保留两次前置FAIL，右H2未测，已验收归档。

- 2026-09-29：登记最终Disable共用RPC入口、2.1秒真实回环正负控及Orin软件验收；保留历史失败和硬件边界。

- 2026-09-29：登记腕部小幅探针与诊断类型分类；本机147+5、隔离Linux ARM根Clippy/167+42测试通过。Orin被SSH信任缺失阻断，历史归因和调度边界保留。

- 2026-09-29：按持续目标登记 Rerun 全量观测桥 V1–V8 完成条件；方案已审、实施中，所有新验收结果待补。

- 2026-09-29：更新 Rerun V1–V8 真实软件、Viewer 和故障证据；记录镜像丢失/公平性、GPU 与中断捕获限制；最终 Linux 软件门通过，按本节边界验收。

- 2026-09-30：登记恢复严格 SSH 后的 Orin 最新浮动 nightly 定向软件门、最终桥同六项复验、官方 RRD 核验与正式快照比对；已验收归档、独审/root通过，不复用 Lima 结果冒称本轮全仓通过。
