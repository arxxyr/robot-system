# 交接 · post-E 通信故障硬化

## 当前步骤

**用户已明确“继续，用astra medium”；当前推进P.7软件交付准备，原生Astra/medium开发与独审。**
本大阶段完成新驱动/共享产物校验及测试、独立评审提交、双端Git对齐与独立目录
原生release两套产物验收后暂停供用户检查，不自动进入真实H2或下一大阶段。
每个大阶段都须用户明确继续才进入下一阶段；周剩35%仍为额外停止上限，不自动push。
此前手动暂停已由本次继续解除，原断点保留在`target/post_e_h2_rt/s48/p7/pause_receipt.json`。

P-E已验收提交`2f1de02`，P.7操作单已验收提交`9867064`，两者未push。
571编译输入冻结926a0f1d…，8件Rust源。各host All72全过；4096原FAIL/exit1在实际
Normal容量3840/3840严格派生EXPECTED_RED，最终Lower与完整收尾均成功。8192本机
360.459s/8298唯一成功/运行期GC1391，Orin360.087s/8270/GC1379；Lower的18/21 GC另列。
每端RPC309、工具178通过，原预算/wire4不变。本机部分质量门按实际未变输入复用并完成
fresh提交前fmt→Clippy；Orin完整quality/build/behavior同轮通过，无重试。922件回传原件
逐SHA一致，Orin267进程终场无相关或不可读、六服务仍停。实际Git HEAD为本机B、OrinA
加同源差；这是本批软件来源约定，不是新现场所需同HEAD/clean。
P-E H入口`local/s48/pe/`：result.json、root_acceptance.md、review/final_review.md及
两端run原件；ELF本体/缓存/重复传输包不入H。B包外后生归档与commit回执随本包承接。
P.7操作单已提交9867064；新H2驱动、报告oracle与共享产物校验五件Python源码已获独审/root验收，
本次源码提交。34项合并Python测试、3项helper夹具通过；独审另核8个报告反例与3个真实无害child
异常路径。root提交前fresh fmt→Clippy及默认PLAN_ONLY通过，571编译输入未改。源码评审见
`p7/review/source_review.{md,json}`，提交回执随后记录；两端Git同步、原生构建和现场H2未执行。P.7证据暂存
`target/post_e_h2_rt/s48/p7/`，不在当前P-E H包内；下包同时承接P-E归档后与提交回执。
新鲜只读盘点确认Orin四件实际安装路径为本仓库`target/release/`，禁止默认release
构建覆盖它们；使用已有独立`target/rr-r4db-arm/root`缓存，先冻结生产四件再测量三件。
六服务inactive，handoff active/exited且MainPID0；这不是电机物理断电证明。实际SSH
loosqk的rtprio为99/99、memlock unlimited，沿该UID核验，不默认sudo整个IPC进程。
R4d-B双主机软件验收通过，独立终审及root验收无阻断；已提交`cdeddd2`，
并按用户“先push”推送`origin/feat/post-e-comm-fault-hardening`。
B归档后回执见git及 `target/post_e_h2_rt/s48/rr_r4d/b/commit_receipt.json`；
该回执记录提交时未push，早于后来实际推送，且是未含于B包的后生件。
新阶段原生子agent开发与独立评审，root定方案/整合/提交；新提交不自动push。
35%停止线取代此前B后暂停及20%停止线，停止前保存未完进度，不将未验收内容提交为完成。

- B阶段唯一编译基线：A `f9a85be4f7064a54695eb33a7f5bf022d31b64eb`，两端原生固定
  nightly-2026-09-10；570输入首末/当前一致，B未改tracked生产或测试源码。
- 本机有效67门：14 fresh/53逐目标复用；Orin有效67门：R1前20 + R2 workspace1
  + R3五门 + R4 deny1 + R5新40。R5实际40/40，四次原FAIL及真实来源保留，不称单轮全过。
- 修正执行前置与证据接缝：workspace显绑同批peer；S0仅NO_COLOR=1；官方漏洞库
  两次clone失败后传输已核官方HEAD的完整缓存，仍执行原deny；B checker精确解析
  并核Cargo下载进度包身份。原断言、业务预算、源码和依赖政策保持。
- Orin原生26报告、19新增profile实例、239调用/237唯一组合通过；六exec/五组
  RefUnit/UnrefUnit及真实退出/重启均已核。两端压力证据3072轮，B新2048/A复用1024。
- 本机本次提交前fresh fmt→根Clippy零警告通过。末轮05:00:31 UTC结束，05:01:13
  同run清理扫描265进程，40gate PID加2已知PID全退，相关进程/IPC/unit及不可读项为空。
  无关default.target/PulseAudio任务保留，不宣称所有系统任务为空。

H包入口 `local/s48/rr_r4d/b/`：`root_acceptance.md`、`final_evidence_review.md`、
`result.json`、两host coverage summary及两段独立机器核验。7041开发清单小件已逐核，
后生独审/root与生成回执另收；ELF本体和派生传输包不嵌入。A后生15件按7已归档/8新增
衔接。A/C历史提交分别f9a85be/1b0feff，其余历史按H索引和git定位。

本次仅受控软件验证，未触电机/串口。D15 OPEN、deployable=false；默认并行CI锁例
旧限制仍在。左H1/H3 PASS、H2两次FAIL、左最后断电；右未测、S46 wire4历史FAIL。
P-E纯软件长负载已通过；不得外推新硬件通过、生产退役或整个通信候选已完结。

## 已提交W4b证据

正式round1连续16门全部满足原期望（14个exit0、两个参数拒绝exit2）。
376源/129事前输入首末一致，恰8改3新，无依赖/构建图或冻结设计变更。
默认S0 605/0/0、完整hook858/0/1、工具89/0/0；重叠分组不相加，原60秒soak仍ignored。
新增SIGKILL5/5、原W4a2/2、原All72/72；4实际产物、979软件文件已逐件核验。

五例均原Child真signal9/reap，旧RR端点随后才结束；首Clear没有回复，不声称已退休。
重启复用原持久目录与锁，首轮14文件完整集合/SHA不变。空marker拒启动；
合法marker的1/0与0/0保守恢复1/1；无marker的0/0原加载false。四次正常重启的
初始latch与只读回复均直接验证，未使能硬件。同内核SIGKILL不等于掉电验证。
原compile/scope/software/Clippy失败共11轮36raw保留；首五软件FAIL未回填为PASS。

## 证据入口

- root裁决：`local/s48/w4b/review.md`。
- 正式run：`w4b/c/round1/run.json`，SHA `a2ea5c48…6ad7d`；
  result为`w4b/c/result.json`，SHA `bc1588d4…64911`。
- 最终376源清单：`w4b/c/formal_source.sha256`，SHA `7b65294c…cb674`。
- B非作者终审：`w4b/reviews/final/robot-w4b-final-source-evidence-independent-review.md`。
  B原件389项、A身份二路5073项、root原件119项均无失败。
- 上述w4b路径在唯一[H证据包](../../.verify/post_e_comm_fault_hardening/H_evidence.tar.gz)中
  对应`local/s48/`；精确索引、SHA及归档边界见[H.json](../../.verify/post_e_comm_fault_hardening/H.json)。
  本次归档后审查与commit回执留本地w4b/post_archive_review及commit_receipt.json；
  不声称位于此前生成的tar内。W4a归档后回执和RR只读方案已随本包携带。

## 后续路线

用户最新要求开发改用原生子agent。CC在2026-09-14额度限制处停止，root已停止其背景agent并退出会话（PTY70557 exit0）；R2剩余及后续由原生开发子agent实现/自测、另一个独立子agent评审，root方案/整合/提交。
开发子agent仍可按需并行拆分，明确写入范围，Cargo继续由一个开发者串行拥有。

W3已将新权限、高/普通双队列、唯一writer、RPC责任账本接入生产调用方。
W4a文件同步故障、W4b固定五例及RR R0–R3已验收；R4完整T8/SUP/ENV、
原连接竞争与双环境验证已在C/A/B依次闭环。P-E已完成双端软件验收，后续P.7软件准备；每包必要检查与独立/root评审通过即commit。
RR r3析构与公共CAL两项方案已ACK，直接替换现有工厂，保topic/schema/FF；
所有peer与自动清理者同批同源，模型/迁移实现按分包开发和评审。
旧跨原子可见性与映射删除问题已在R1–R3实现，R4双环境软件验证已通过；不外推额外内存序证明或完整候选闭环。
W1旧锁偶发失败仍列观察，本轮All通过不等于其根因已闭合。
P-E保原20/s、50ms节拍、360–600秒、成功唯一新写>8192、真实wire4与所有结果final_stop。

## 现场边界

左H1/H3 PASS，H2两次FAIL；最后确认左臂断电、双臂支撑。S-46本地8192长测通过，
Orin真实882唯一写时wire4满FAIL；右臂未测，六服务最后停止，D15 OPEN、deployable=false。
上述硬件状态沿用最后现场记录；R4/P-E软件运行没有更新电机物理状态。
新使能前须取得新的电源/USB重置与持续在场事实，
不复用旧H2驱动或已消费授权。Orin为`loosqk@10.0.1.40`，调试/RT必要时直接sudo；
标定沿Orin已核Python值，串口按原D2 by-id身份。候选后续顺序保持D15→Rerun桥→F7。
