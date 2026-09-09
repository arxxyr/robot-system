# 交接 · post-E 通信故障硬化

## 重启后已恢复（当前入口）

用户已明确“继续”，已恢复软件推进，不重跑已消费的H2驱动。重启后核实工作区
仍保留全部改动。用户随后指定root定方案和评审，cc-claude具体开发与测试，
用户最新要求优先节省root token，并允许多个Claude或Codex子agent并行。
用户进一步明确：cc-claude完成实现、测试和自查后，必须由root或独立Codex子agent
作最终评审；Claude交叉检查只作为前置检查，不能代替最终评审和提交门。
当前Cargo和W0.1源码归writer Claude独占；root只做最终决策、阻断项处理与验收提交，
详细方案和独立复核可以委派。旧开发agent保持停止，新并行角色见下。
A1验证基线为`ce61623436138342b6a2d234b10bada71d6877ca`，分支
`feat/post-e-comm-fault-hardening`。A1已独立评审提交`9fe20ea`，未push；S-46源文件
仍未提交，不得reset/clean或冒充已收口。S-47合入S-48；A2已评审提交`dee67fa`，
A3a共享权限归位core已评审提交`fdac02c`。A3b快照并发修复已通过本地22门及独立源码/证据评审，
已提交`d67752ded3fba7fd5de1ea31b83243b6f7cd8382`，未push。
当前A3c/W0纯文件凭证获独立及root Plan Approved，按PLAN P.6.14.5交writer Claude
实现与测试。W0 round5已由Codex完成源码与证据终审，结论Changes Requested/
EVIDENCE_INCOMPLETE：create后fstat失败丢凭证、续段0→1反例缺口、最终轮来源与
fmt-all记录缺失。已按a3c/root_round5_review.md回修并在round10重新冻结，七门通过，
两位Codex分别完成源码与证据终审，root采纳APPROVED_SOFTWARE_SCOPE；旧轮和
评审对象保留，不回填缺失证据。
W0已提交`8624f27d3d2340926e1909dcb8a95aaff9c6349d`，未push。
W1锁/恢复已获APPROVED_SOFTWARE_SCOPE：round3十四门通过，335项来源前后一致，
独立源码终审与证据核验均通过，root采纳，已提交`1b2145737143eb631cb45b32d96a40d8d2d8ec43`。round2进程夹具缺口已修，
旧轮及21对象评审快照保留，最终证据a3d/review.md与cc/round3。无Orin/服务操作。
监督及请求生命周期合同已批准；生产总装尚未批准。
W0.1只读pair已获APPROVED_SOFTWARE_SCOPE：round2七门全零，335项完整来源一致，
恰三文件变化；独立源码/证据终审及root范围核验通过，证据w01/review.md和cc/round2。
起点为事后重建的派生记录，与W1冻结结束清单逐字相同；不冒称开发前实际采样。
round1中止与审计v1名称格式误判均保留。下一包W2a纯模块r4获root方案批准，详P.6.14.9；
待W0.1提交后按a3e/cc_claude_task.md实施。容量r3及root精确拒绝分类/O1澄清已获合同批准，
详PLAN P.6.14.10；后续生产总装及网关函数实施仍待评审。
W2b关闭/join机制r3已独立方案批准并获root采纳，详P.6.14.11；待W2a提交后实施。

最新用户决定：基础控制与普通工作分两个有界队列，每轮先高后普通；每项必须
非阻塞推进。关节目标合并最新值，急停/Disable可否决旧Enable/Clear的恢复权限。
已写Spec S-48、PLAN P.6.14及入口文档；旧S-47统一FIFO容量方案暂停并入。

S-48 A1纯文件Clear事务已实现、本地门通过并获独立APPROVED_SOFTWARE_SCOPE。来源
`b61a1534aab527528f075c16c36d72b7079869317c75819e9264eb1504b6c20e`/313输入、
base `ce61623`；模块28入口、公开API7、原latch24、compile-fail3均PASS，
全workspace fmt/Clippy零警告。原始证据`target/post_e_h2_rt/s48/a1/second/`，
first Clippy首红已修复保留，不覆盖。验证期间源码摘要未变。

112字节marker绑定canonical目标及完整事务，Prepared/Repaired不可Clone、按值
消费，inspect可读不冒充durable，repair前复核，false前严格同步marker；
特殊marker文件拒绝，临时文件撞名最多64，true主→备、false备→主严格同步。
16个实际进程退出点验证恢复；逐步骤hook是成功操作之后中断，unlink后的目录
同步错误是目录open失败，不称真实fsync errno或物理掉电实测。
A1尚未接生产控制链；A2独立权限原语已通过，后续A3双队列+writer总装、A4网关端到端验收。

A2函数级方案已获独立Plan Approved及增量ACK，见PLAN P.6.14.2，现按用户最新分工：root定方案与评审，本机cc-claude执行
独立control_authority模块、公开API/真实allocation guard测试及Cargo。
采用低24位状态/高40位权限epoch的AtomicU64，3位Phase覆盖Clear与Enable生命周期，
domain绑定非Clone owner；旧事务占位直到真实退休，StartView只能启动一次。
MAX核当前修复目标/Exhausted，有限typed safety转换，观测绑定domain/epoch/Phase/
safety，commit_enable还核当前TorqueOn；两种同代次Confirm交错必测。
此阶段只注册新模块，不迁移生产权威、不接双队列。旧开发agent已停止；
委托文件为target/post_e_h2_rt/s48/a2/cc_claude_task.md，证据写a2/cc，root不并发跑Cargo。

A2最终来源`439af44ef2f4897143d227e2ffb09f85d4dd9bad7fc64d2de333c8d6f248a8d3`，
base `9fe20ea`、317输入；第六轮13道门全部PASS：fmt、全workspace及guard特性
Clippy零警告；模块48、compile-fail13、公开默认16、debug/release guard各21、
rt_guard-only18；A1回归28/7/24/3。root逐行评审并独立复算日志SHA/计数/源码身份，
确认非A2源码（含A1、S-46）未变，结论APPROVED_SOFTWARE_SCOPE。
证据`target/post_e_h2_rt/s48/a2/review.md`、`root_evidence_receipt.json`与`cc/sixth/`。
首两轮Clippy红、第三轮证据脚本中断、第四轮并发夹具红、第五轮通过均保留；第六轮
才是最终源码。未运行硬件，不外推为双队列已接通或S48整体完成。

A3a依据PLAN P.6.14.3只调整依赖方向：control_authority归位robot-core，s0直接
重导出同一模块，domain计数器只有core一份；s0的真实inhibit常量对照保留在其
cfg(test)中，不扩大pub(crate)。默认关闭的验收构造feature由s0显式转发。
来源`6e9571abd0e4d0d823b8c1b0df4fb27d774aa51dc2cc126d2d59cdfae92b7084`，
base `dee67fa`、320输入（包括3个已删除旧路径）。第二轮18道门PASS：fmt、
全workspace与core/s0带特性Clippy、core无默认特性check；core模块126、文档13、
s0常量对照1、公开默认18、debug/release guard各23、rt_guard-only20；A1回归
28/7/24/3。七个生产feature图均无验收特性，阳性对照确认转发实际可见。
workspace/all-targets热缓存build --timings完成，8.72s仅为本次构建记录。
首轮feature图的阳性对照失败保留，改用反向图后第二轮通过；非源码失败。
root独立复算源码/日志SHA和计数，核与A2实现等价及非A3a输入不变，批准本包；
证据为H包`local/s48/a3a/review.md`、`root_evidence_receipt.json`、`cc/second/`。
本包不改PreemptProbe、adapter、旧权威或snapshot，尚未生产接线和Orin验证。

A3只读勘察见包内`local/s48/a3/root_integration_survey.md`：仍需锁定强类型wire租约
归属、全部旧权威写入口替换、两队列准入与退休、单writer生命周期及端到端保留容量。
A3b已获APPROVED_SOFTWARE_SCOPE：最终来源c359e632…d9c5bb/328输入，base fdac02c。
预分配AtomicU64载荷、固定codec、&mut写端和非回绕版本替换旧并发volatile；旧模块
Miri三seed全报数据竞争，修复后同驱动三seed均无UB。RT/Serial/IPC耗尽退出，daemon
补standalone监督；回归用publish前同域时间与真实READY比较，真实join后再导出。
第六轮16门+补跑6门全部PASS；workspace1740/1 ignored、doctest17/8 ignored、
transport hook77/3 ignored、S0 guard+hook517/1 ignored、release guard25/0。
四条既定RT guard专项及parent ignored另跑；67病例、7生产feature图及阳性、实际
timing均通过。独立证据审计核1411文件，result SHA 2e0d420b…c3；root核26源码SHA。
最终证据为H包local/s48/a3b/review.md、independent_source_review.md、
independent_evidence_receipt.json、cc/sixth及sixth_supplement。首四轮FAIL与第五轮旧
源码PASS保留；没有把后者冒充最终通过。当前软件批准不包含Orin或双队列总装。

当前并行编排：Claude PTY67537/PID91744仅在target准备S46测试RAII补丁，不应用、不Cargo；writer
Claude PTY70639/PID259899独占源码和Cargo，W0.1冻结待提交，随后W2a；priority方案Claude
PTY99320/PID260261。三会话已核模型
claude-fable-5-1、effort=max。恢复时核实际/proc和会话状态，不能因观察超时重启。
紧凑进度脚本target/post_e_h2_rt/s48/root_team_watch.py；不读thinking或整屏刷新。

A3两份r2提案及交叉报告冻结，仍未批准总装。共同裁决见a3/root_decision_notes.md。
W0四项凭证阻断由cc_writer_r2_resolutions.md §A闭合并获独立批准；正式函数级
PLAN P.6.14.5再次独立Plan Approved，开发任务a3c/cc_claude_task.md，证据a3c/cc。
W0 round10已获snapshot_plan_review源码批准及a3b_evidence_audit证据批准。
监督r2独立终审要求修Active义务接管、IPC/watchdog stop和join失败保留，
监督r4已独立复审并获root合同批准，continuous-unsettled告警时间与真实退出预算
明确分开，Spec S-48 / PLAN P.6.14.7及CHANGELOG已同步，尚待函数级实施包。
priority生命周期r6已独立复审并获root Plan Approved：信用/旧wire/真实队列、
业务截止锁存与非RR/只读RR分流均已闭合，见root_priority_lifecycle_r6_approval.md。
容量r3和root最终澄清合同已批准并回写PLAN P.6.14.10；旧S46证据不改，新的O1验收与网关/生产总装函数计划仍须另审。
root_priority_lifecycle_r3_review.md明确未知/未退休请求不能只按TTL遗忘、RR请求
关闭safe overflow，但这些方向尚未构成总装实施授权。A3b归档不包含当时尚未冻结
的两份r2增补，index已记录；后续包再按最终SHA收入。cache类内预留
与旧4096门若冲突，须明确Reverse Sync并保留旧S46结果，不默默扩容/改计数。
root只裁决与整合，避免重复全量源码/日志阅读。旧implementation/reviewer保持停止。

A3c/W0最终来源`80294b56…145f73`/331输入，base `d67752d`；round10七道门
严格串行PASS，fmt-all、全workspace及hook Clippy零警告，模块42、公开10、旧latch24、
文档反例4，均无失败或ignored。目录fd绑定所有文件操作，Created保留活fd；
create后首个metadata失败返同一凭证，目录sync成功后Durable提升不可失败；
unlink后失败保留Unsynced并只重同步，0→1续段核真实primary/backup十步。
原16个进程退出点保留并新增身份采样点；不称存储设备实际EIO或物理掉电测试。
round1–9及round5评审快照保留，快照清单自指问题仅修清单、12件历史对象未变。
独立证据核331项完整来源前后/当前一致，W0外含S46未漂移；最终证据见H包
`local/s48/a3c/review.md`、`independent_source_review_round10.md`、
`independent_evidence_receipt_round10.json`和`cc/round10/`。fd-less凭证不宣称抗inode
ABA；本包尚未安装W1目标锁，单writer及无并发改名者仍为接线前提。未运行Orin。

现场最新确认仍为左臂断电、双臂支撑；六服务停止，D15 OPEN。未开展新串口操作。
恢复软件工作不需要物理授权；任何新H2需软件审核/提交/两端对齐及新现场事实。

## 当前状态（2026-09-09）

C1–C5 已独立评审提交，C5 `85efefb`；S-44 工具 RT 修复与左 H1/H3 已提交
`a161a7b`。S-45 采样修复已独立 APPROVED_SOFTWARE_SCOPE、提交 `965bcd7`，
入口文档 `ce61623` 已同步 Orin，未 push。左 H2 两次均 FAIL；S-46 / PLAN P.6.12
本地五阶段获独立ACK，但Orin长测遇真实wire4满FAIL。用户明确基础控制高优先级、
关节目标普通通道；S-47背压暂停并入S-48优先级隔离设计。[Spec](post_e_comm_fault_hardening.md) 是有效语义。

- 左 H1：100预热+1000次六电机轮询完整ping，零错误；时延只记录特性。
- 左 H3：100预热+1000次完整六ID断扭广播，一写零读，正式host调用最大
  0.491552ms；不保证UART发完或机械断扭。
- 左 H2 首次（`a161a7b`/`d6963ed0…4b11a`）：RT门通过；35次在途分类失配后，
  epoch10072缺同epoch acquire，严格分析器拒绝，正式0。真实final_stop确认
  state3且Disable成功，进程全退。无dropped或通信Fault，不能冒充总线故障。
- 左 H2 第二次（`ce61623`/`c07a9886…514b1`）：RT门通过；topic/idle完成100预热+
  138正式、零失配。4096条幂等缓存耗尽（3620背景干跑+476真实恢复）；另11条
  背景请求启动迟到至少50ms，原始时间戳证实串行await慢RPC导致后继迟到。
- 第二次 final_stop：GetState已确认state3，随后Disable容量满拒绝，整体仍FAIL。
  所有进程退出、串口释放、六服务inactive/PID0、正式四SHA未变、marker双份1。
  用户随后确认左臂已断电、双臂继续支撑；最初PENDING快照与后来现场回复分开留证。
- 右 H1–H3 未测，D15 **OPEN**，`deployable=false`，不自动启动常驻服务。

当前证据唯一入口 [H.json](../../.verify/post_e_comm_fault_hardening/H.json) 与
[H_evidence.tar.gz](../../.verify/post_e_comm_fault_hardening/H_evidence.tar.gz)。两次H2
FAIL按各自来源保留，第二次原始目录为 `target/post_e_h2_rt/orin/left_h2_current/`。
C5保留唯一C5包；S-45源码c07a/309输入，两端67场景、Orin18门及四RT guard已批准。
原失败与修复后通过不覆盖；S-45真实构建base仍为a161，不能反填后来的运行HEAD。

## 当前分工与下一步

按用户最新分工，root维护Spec/PLAN/文档、亲自评审源码与原始验收证据；
cc-claude实施并执行测试，独占本地Cargo。旧implementation/reviewer不再承担本阶段；
远端操作由root统一调度。S-47合入S-48。共享文件按owner写，同机Cargo串行；每阶段评审和
fmt/Clippy等必要门通过后commit，不push，不改冻结 `docs/00_design.md`。

S-46已有固定due的有界64任务背景、H2专用8192/TTL300缓存装配、分项
final_stop，以及供两端执行的真实mock长负载4096 RED / 8192 GREEN门。生产默认、幂等合同与
wire实际4槽不改；首错停止新背景请求及下一次使能，不取消已经发出的live恢复RPC。
软件正例至少360s且成功唯一新写>8192，最多600s；任何wire满或节拍失败仍FAIL。
两端各75工具/255RPC和原67通过。本地4096预期RED、8192运行360.229s/8281
唯一写全PASS并独立ACK。Orin在882唯一写时真实ExceedsMaxActiveRequests，
不是缓存满；前四个背景回复在拒绝后约299ms才完成，最终收尾全PASS，8192未跑。
原始702文件已逐SHA核验到`target/post_e_h2_rt/orin/s46/`；本地两次与Orin一次长测
均已收尾，无进程残留。
S-47有界背压暂停，子方案合入S-48控制优先级隔离。当前JointCommand先于topic
EStop读取、Disable无独立优先级、Clear/EStop同步持久化阻塞IPC。新方向为
端到端基础控制保留容量、唯一state owner安全降低抢占、单disk writer解耦。
用户进一步明确两个有界工作队列、每轮先高后普通；关节目标普通侧合并最新值，
保留既有原子安全notice，每项非阻塞推进，不扩通用多级队列框架。
A1文件事务已实现并完成本地验证，原子权限及总装协议继续分阶段二审；
不允许只读epoch后继续store(false)，不以异常重启无条件额外锁存替代事务协议。
Spec S-48/PLAN P.6.14为当前用户要求和阶段验收依据。
原20/s、原67及AC-12取样后P99不放宽；新入口验证从真实publish/call前留戳。
用户已确认左臂断电，本轮现场操作结束；现在只做软件验证，不再打开总线。

第二次新物理确认已消费，`left_h2_current`目录不能复用，旧S-45驱动不能再执行。
S-48完整软件独立评审、必要软件门、提交与Orin对齐后，准备并审核新驱动/新输出目录，
再取得新的左臂电源/USB物理重置及持续在场事实。右臂物理步骤尚未确认。

Orin为 `loosqk@10.0.1.40`，仓库同本机路径。此前现场产物在
`target/post_e_hardware_current/s45/recheck/artifacts/release`，留其原始构建身份。
普通loosqk的rtprio99/memlock无限可用，NOFILE1024；必要时sudo，但其限额不同。
工具在Tokio创建前taskset CPU0–3，serial FIFO70，左CPU4/右CPU5，其余OTHER0@0–3。
实际内核CONFIG_PREEMPT=y，不称PREEMPT_RT；真实串口按D2 by-id身份选择。

用户明确复用Orin上Python标定：两臂配置已直接引用
`/home/loosqk/repo/dimensional-applications/calibration/left_arm.json`、`right_arm.json`。
左SHA `d946abf3…8c6f7de`，右SHA `98ad6936…2cea03e`；每臂六关节/ID1–6映射及
Rust加载字段兼容。读取同一文件的软限位，不重新标定或重复写EEPROM。
后续硬序：通信硬化现场验收 → D15退役决策 → Rerun桥 → F7判据小件。

剩余清理事实：Orin旧Python仓库仍约11.5GB（.venv约9.63GB、recordings约1.86GB）；
当前calibration约20KB被两份配置直接引用。现阶段未删除，保留后续按退役顺序清理。
