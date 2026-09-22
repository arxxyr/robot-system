# 交接 · post-E 通信故障硬化

## 当前步骤

2026-09-22用户继续后，注入延时软件修复源码已提交`fb600fd`。仅测试hook用同一RobotClock
仅真实读数达到原deadline才成功；不足继续正时长sleep，最多8次后失败锁存。
溢出、倒退同样失败，由调用者真实释放lease。工具在gateway/S0停止后独立核`final_trace`，拒绝尾窗sticky或未知状态，
不改`final_stop`安全动作原结论。原1–5ms请求、四组100预热/1000正式、SLA和trace wire不变。
571项新编译输入aggregate=`52b1832fd3e7d6ae741098fff05a3cfd45f16056f33dfb13b2d53bafc1c58d17`；
source/build HEAD均为`fb600fd`，不继承上一阶段旧产物身份。本地57项定向测试和必要clippy通过，
Orin六条原生recipe完成生产4/测量3和7help，同57项及唯一AC12 mock四组各2通过，共20次completion。
final_trace确认双child停止且valid，非dry-run收尾、实际UID清场、来源及正式安装状态核验通过。
原controller误猜缺少`000_`前缀的final_stop路径而FAIL，保留不覆盖；独立只读final_validation
随后exit0补验，不称单轮全过。最终证据独审/root验收已通过。
普通与FIFO70@CPU4无硬件时钟探针各4000项，均未出现短延时或补等；已确认旧std sleep用
MONOTONIC而量测用RAW，但没有复现原短1426ns，不能宣称该样本具体系统根因已确认。
本阶段不安装、不操作硬件、不自动H2或push。用户最新要求持续推进至额度剩25%，
替代35%及每阶段自动暂停；每阶段独审/root验收和commit保留，真实H2仍须新的物理重置/支撑/在场事实。
此前三次已采样H2 FAIL与一次前置拒绝原样保留，D15 OPEN、deployable=false、右臂未测。

证据阶段入口为`local/s48/h2_delay/`；已完成独审/root验收，归档身份见H.json；本阶段post_archive_review与commit回执为包外后生件，下包承接。

### 上阶段真实H2失败（收口eb59bcc，以下保留当时结论）

**2026-09-22 本次真实左H2为FAIL，软件收尾完成；失败证据收口commit后暂停供用户检查。**
用户明确继续并确认“本次全部就绪”；runtime HEAD b4ff720、实际build HEAD4191bf0，
571编译输入及三Python身份继承已核，未安装产物或修改测量预算。09:46:29至10:09:47 UTC，
单次authorize/launch各1。PX-56真RT门通过；topic-idle 100预热+1000正式PASS，
SLA-A P99=25440ns、SLA-B P99=10649408ns；topic-inflight 100预热+639正式后invalid1。
attempt739/seq13681原InjectedDelay请求3895634ns、实测3894208ns，短1426ns；
请求范围合法，严格b>=a门拒绝。第3/4组未执行，全程分类重试0、原invalid dropped0。
这是载荷有效性失败；时基/等待实现不一致仍待验证，不称已证实根因或改成PASS。

独立ExternalEStop、Disarm、GetState3及非dry-run DisableAck均成功；工具和driver自然
exit1/真实reap，外层signals[]。最终related/unreadable均空、串口fuser空、六服务inactive/PID0，
双marker Dirty。正式四件、配置、标定、unit及编译源前后未改。无新物理断电确认。
证据入口H包`local/s48/h2_run/`；完整原件在其precheck目录execution_collection，
原report/run FAIL保留。后续需先诊断并修复注入时长保证，再按原预算软件验收与新现场边界执行；
本阶段不实施下一修复或自动重试，不push，D15 OPEN、deployable=false，右臂未测。

**此前同日：左 H2 前置拒绝的软件修复已验收并提交b4ff720；以下为历史阶段。**
用户完成本次电源/USB重置并确认持续在场后，实际driver exit1且真实reap，
authorize=0、launch=0，未进入电机/串口访问。TRUST目录0755不符既定0750；
原清场另有两个不可读PID，后来消失，身份未知，不能用事后sudo清场回填原PASS。
原件位于H包`local/s48/h2_new/precheck_20260922T082008Z_a9c44c4c/`。

修复复用真实身份/RT/TRUST前置；进程检查先绑定PID/start_ticks/boot/UID，
仅对权限不足候选用固定sudo只读探针，未知/身份变化继续拒绝。控制进程仍用loosqk。
TRUST仅0755→0750，inode/属主/五个现有文件身份与内容未变。
默认单测不依赖sudo；显式Orin无害child集成单独取证。571编译输入与测量预算未变。
源码1b6db3e/4191bf0已独审提交；最终50项Python、Orin真实sudo集成通过。
实际build HEAD4191bf0两端clean同源，原生生产4/测量3、7help、15份自然收尾及
共享只读复检通过；571编译源/预算不变、未安装。没有豁免旧三Python继承门。
本包入口h2_new/result.json、artifacts_delivery.json、root_acceptance.md与review/final_review；
归档后审查、最终同步及commit回执为包外后生件，下包承接。收口commit后暂停检查。
本次现场确认属于已退出attempt；后续真实H2需明确继续及适用的新现场事实。

首修源码`1b6db3e`已在两端clean对齐；首轮原生六recipe、七help与十五份收尾通过。
共享只读预检随后发现系统早期进程的合法`start_ticks=0`被误拒，原FAIL保留。
已核PID1/2/42原stat字段22为0，二次修复仅接受非负启动计数，继续核完整身份；
负值、缺字段、格式错误和身份变化仍拒绝。4191bf0重新绑定产物及最终复检通过；
首轮与末轮来源分别保留，不称单轮全过。

以下保留P.7已验收基线与此前步骤，不作为2026-09-22硬件通过或当前物理断电证明。

**P.7软件交付准备已完成，收口提交后暂停供用户检查；不自动进入真实H2。**
本轮按用户要求使用原生Astra/medium开发与独审，root方案/验收/提交。每个大阶段须用户
明确继续才进入下一阶段；周剩35%仍为额外停止上限，不自动push。

P.7操作单已提交`9867064`，驱动/报告oracle/共享产物校验源码`8491b42`；systemd
依赖顺序误拒修复`c759082`，省略复杂空数组的同连接typed核验修复`b7cbaca`。
37项合并Python测试、3项helper夹具通过；独审另含报告/真实无害child与systemd
拒绝反例。提交前fresh fmt→Clippy通过，571编译输入926a0f1d…未改。

双端在实际`b7cbaca` clean上对齐，Orin八件旧源码独立备份后精确恢复/快进，现役四件、
配置、标定及七unit前后未变。Orin原生nightly-2026-09-10 release先冻结生产四件，
再冻结测量三件，实际Cargo feature/profile/ELF183及七次精确`--help`全通过；15份
真实完成回执/PGID清场全部通过，六业务服务仍停止。首轮因SSH PATH缺cargo bin在
rustc spawn前失败，原16件保留；只修启动PATH后的成功轮80件已逐SHA回传，不称单轮全过。
成功run为`native_20260920T114445Z_41e55668`，build HEAD永久记录`b7cbaca`；随后
仅文档/证据收口提交可按祖先与完整编译源/三Python未变继承，不能回填构建来源。

P.7 H入口`local/s48/p7/`：`result.json`、`root_acceptance.md`、`review/final_review.md`、
`artifacts_delivery.json`及实际同步/构建原件；随本包承接P-E归档后与commit回执。
本阶段归包审计、最终Git对齐和commit回执为包外后生件，位于
`target/post_e_h2_rt/s48/p7/post_archive_review/`及`commit_receipt.json`，下包承接。
未安装产物、未授权marker、未触电机或串口、未取得新现场事实；deployable=false、D15 OPEN。

此前P-E已提交`2f1de02`，两端571输入各All72通过；4096原FAIL/exit1在实际Normal
3840/3840严格派生EXPECTED_RED，最终Lower与收尾成功。8192本机360.459s/8298唯一成功/
运行期GC1391，Orin360.087s/8270/GC1379；Lower的18/21 GC另列。RPC309、工具178通过，
原预算/wire4不变。P-E当时实际Git为本机B/OrinA加同源差，与本轮真实同HEAD/clean分开记录。
B已提交`cdeddd2`并按用户“先push”推送origin；后续P-E/P.7各提交未push。

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
原连接竞争与双环境验证已在C/A/B依次闭环。P-E、P.7及前置修复已验收；三次真实左H2 FAIL保持；本次fb600fd仅修测试hook等待保证及尾窗检查，按最新25%指令持续推进，新的现场测量另核事实。每包必要检查与独立/root评审通过即commit。
RR r3析构与公共CAL两项方案已ACK，直接替换现有工厂，保topic/schema/FF；
所有peer与自动清理者同批同源，模型/迁移实现按分包开发和评审。
旧跨原子可见性与映射删除问题已在R1–R3实现，R4双环境软件验证已通过；不外推额外内存序证明或完整候选闭环。
W1旧锁偶发失败仍列观察，本轮All通过不等于其根因已闭合。
P-E保原20/s、50ms节拍、360–600秒、成功唯一新写>8192、真实wire4与所有结果final_stop。

## 现场边界

左H1/H3 PASS、三次已采样H2 FAIL；9/22另一次driver前置拒绝不算测量。
当天已确认左臂断电重上电与USB重插、双臂支撑；收尾后电源未新确认。S-46本地8192长测通过，
Orin真实882唯一写时wire4满FAIL；右臂未测，六服务最后停止，D15 OPEN、deployable=false。
R4/P-E软件运行没有更新物理状态；9/22事实仅绑定已结束attempt，不再要求本次继续值守。
新使能前须取得新的电源/USB重置与持续在场事实，
不复用旧H2驱动或已消费授权。Orin为`loosqk@10.0.1.40`，调试/RT必要时直接sudo；
标定沿Orin已核Python值，串口按原D2 by-id身份。候选后续顺序保持D15→Rerun桥→F7。
