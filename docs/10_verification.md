# 10 · 验证与测试（活文档）

> `docs/00_design.md` §10 的落地版本。历史表格保留各阶段的实测与待验证标签；
> 当前 post-E 候选按下方验收入口推进，不以历史 PASS 替代本轮结果。
>
> - 权威语义来源：`docs/00_design.md` §10 / §6 / §4.9。
> - 偏离 / 调整语义走 `docs/CHANGELOG.md`，不在本文件改。
> - 本文件每次更新 → 在底部《变更记录》追加一行。

---

## 2026-09-08 依赖升级分包

全部 27 项外部直接依赖的最终版本与验证记录见
[依赖升级记录](handoffs/dependency_upgrade_2026-09-08.md)。该分包基于 C4，最终锁定后
使用 `--locked` 执行静态检查、测试与构建；C5 与硬件验收仍按各自条件独立收口。
依赖分包已独立 **Approved** 并提交 `0001858`：完整 tests **1643 PASS、2 项原有
ignored**；lib / bins **1461 PASS 包含在 1643 内，不相加**。原有 ignored 未计为通过，
完整执行证据见 [report.json](../.verify/dependency_upgrade_2026-09-08/report.json)。

## 当前候选：post-E 通信故障硬化（2026-09-08）

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
