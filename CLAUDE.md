# robot-system（`robot-core`）

> Rust 通用机器人运行时。S0/S1/S2 三层 + iceoryx2 + FlatBuffers + 外部 RPC。
> 架构设计已冻结 —— 一切以 [`docs/00_design.md`](docs/00_design.md) 为准；不在代码 PR 中改架构语义。

## 新 session 必读（按顺序）

1. 本文件 `CLAUDE.md`
2. [`docs/02_start_here.md`](docs/02_start_here.md) — Claude Code 上手指南
3. [`docs/00_design.md`](docs/00_design.md) — 冻结设计（权威）
4. [`docs/01_implementation_steps.md`](docs/01_implementation_steps.md) — v0.2 实施步骤

冲突裁决：`00_design.md` > 本文件 > 全局 `~/.claude/CLAUDE.md` > `02_start_here.md`。

## 分支与双远程收尾（2026-10-18）

- 长期只保留 `master`、`dev`；功能和修复分支仅在开发期间使用。
- 每次已授权开发完成并通过适用检查后，将成果合入 `dev` 和 `master`，同步推送 `origin`、`github` 两个远程，再核对两端分支 SHA。
- 确认提交已被保留分支包含、远程同步成功后，回收本地和两个远程的已完成开发分支；删除前重新核对远程状态，不强推覆盖他人提交。
- 工作树里的未完成草稿必须保留；回收其分支名时可在原提交上转为 detached HEAD，不因清理分支而合入未验证草稿或删除工作目录。
- Git 收尾不代表实机验收通过；硬件待验证项、D15 `OPEN/DEFERRED` 和 `deployable=false` 按实际结果保留。
- 用户已要求删除 `H_evidence.tar.gz` 及其 Git 历史版本；不再生成或提交该大包。保留的历史摘要不能代替已删除的包内原件。

## 开发教训：先收敛最小可用闭环（2026-10-15）

参考 RP1 实现复盘后确认：当前过度设计主要出现在调试编排、重复验收和证据包装。
一次单电机通信排查被大量准备、子进程和归档拖长，甚至耗尽现场确认时效；后续必须减少这类成本。
本节约束工作方式，不修改冻结架构、安全判据或历史验收结论。

2026-10-20 用户已明确批准一项定向判据变更：仅左轮`right7`，使能/目标速度仍严格为0；速度反馈仅为0或±50时，增加固定1秒五组位置/速度读取，位置跨度≤1计数才接受，缺失、超时或超界继续失败。该授权覆盖实现和验证，不覆盖其他电机、历史结论或新的现场就绪事实；[授权原文](.verify/left_wheel_stop_stability_2026-10-20_r1/approval.json)保留。

2026-10-22 用户另行批准仅前轮`right8`零速保持使能阶段的前两组额外接受原字`0x8032`（−50）继续观察，并保留原值；前8的第三组、断扭后SDK和整机停止读回仍严格为0；左7已获准的独立稳定性判据不变。20秒动作、500毫秒观察及21秒启用上限不变，不推广到其他负值、电机或阶段。[本次授权](.verify/front_zero_intermediate_2026-10-22_r1/approval.json)仅覆盖软件实现和验证，不代替前轮失败后的实际断电回执或新实机GO。

2026-10-22 用户先批准右9复用左7停止复核，随后明确要求三轮统一处理。当前维护停止快照对`right7/right8/right9`使用同一规则：首末使能和目标速度严格为0，速度原字仅0/±50；固定1秒五组位置/速度、共14读，位置跨度≤1计数且包含本次快照位置，采集总预算1.5秒。缺失、超时、超界继续失败；原始速度保留，不用均值抵消正负波动。各轮独立复核，不向机械臂/头部、运动中反馈、SDK初始/最终或保持使能零速检查推广；前8既有中间组例外保持。[三轮授权](.verify/base_wheel_stop_stability_2026-10-22_r1/approval.json)仅覆盖软件实现与验证，不授权新动作、不回填历史失败。

2026-10-22 本轮前8零速保持使能末组返回`0x8032`后，用户报告实际已停止，明确要求将该标准改为±50，并先完成左轮一次。现仅三轮零速保持使能的末组接受原字`0/50/0x8032`；前两组保留正向0..1000减速观察并统一允许`0x8032`，原值不清零。TE1/GV0、20秒动作、500毫秒观察、SDK初始/断扭后严格0和后续三轮14读不变。该候选随后仅前轮完成一次实机并局部通过，读数300→0→0，未触发±50分支；此前前轮原FAIL、用户未断电及左轮独立新GO如实保留。[授权与处置](.verify/base_wheel_stop_stability_2026-10-22_r1/user-disposition.json)，[候选验证](.verify/base_wheel_stop_stability_2026-10-22_r1/zero-stop-update/summary.json)。

2026-10-22 用户进一步批准三轮SDK初始及断扭后也处理±50，先完成软件验证。仅原字50/0x8032触发同一SDK连接的一次新14读，复用现有1秒五组、总1.5秒、TE/GV0、PP跨度≤1的稳定性规则；PV0保持原路径，初始包含当前PP锚点，失败无写。断扭后保留原1秒减速观察，首次±50最多额外1.5秒复核，不续期GO或21秒启用预算。原帧保留，失败不重试；实机动作前先通知，本轮未授权动作。[授权与软件验证](.verify/sdk_wheel_stop_stability_2026-10-22_r1/summary.json)。

- **实现前先读参考原版（2026-10-20）**：有原版或已可用实现时，先阅读实际入口及其调用链，核对版本、依赖、初始化、参数/单位、指令编码、连接复用和停止顺序，再实现最小等价路径。不能只看 README、API 名称或凭经验补流程；上游当前版本与现场安装版本分别核实。
- **遇到问题先对照原版**：先找本地与参考实现的具体差异，用原帧和日志确认执行到了哪一步，再选择最小修改或有界试验。尚未进入运动阶段，不算完整运动指令无效；不先加大动作、改 PID、放宽判据或增建框架。用简短记录说明差异即可，保留已有安全边界。
- **固定当前目标**：一台 XLeRobot、一份明确配置、既有 `scripted` 后端和统一操作入口，跑通“指令 → 实际动作 → 状态反馈 → 停止”。已有受监督双臂 demo 作为基线，只补未闭环部分；当前不扩展 S2、模型推理、新机器人或通用会话框架。
- **每轮只解决一个实际阻塞**：先提出能由最少必要读数或一次有界试验回答的问题。复用现有工具、配置及启动/收尾入口，不按轮次复制新驱动，不为单个故障先搭框架；持续控制复用 S0 已有总线连接。
- **把准备放在现场确认之前**：先完成编译、检查和执行入口准备，再确认本轮现场条件。有效授权和持续看护覆盖的范围内不反复问同一问题；现场确认后只做必要的当前设备、占用及安全检查。现有时效过期或现场状态变化时按原规则重新确认，不以改时间戳续期。
- **按变更风险验证**：源码、产物和适用环境一致时引用已有验证，只补受影响检查；有新失败或未解决疑点再扩大验证。现行提交质量要求仍执行，不把整理文档变成一次全仓或实机验收。
- **分清诊断与验收**：目标设备通信、动作精度、整机停止分别记录。只读诊断使用维护独占和必要前置；动作测试保留原启用、停止及失败处置要求。局部通过不覆盖整轮失败，软件通过不代表实机闭环。
- **证据集中且够用**：复用现有 `run.json`/摘要入口并保留必要原始日志、原帧和失败事实；不再生成内容重复的报告、清单和归档。历史原件不因本节而删除，项目入口只记当前结论与下一步。
- **避免重复封装**：已在 Git 中的源码使用固定提交和清单定位；不再把源码包重复放进归档、启动脚本和日志。精简旧归档必须逐项核对，保留独有日志、原帧及历史差异源码，并说明旧路径的去向。
- **保留必要安全边界**：S0/S1 隔离、唯一写者、持久急停、超时、失败粘性和停止读回继续保留。D15 保持 `OPEN/DEFERRED`、`deployable=false`；缩小本轮范围不等于放宽门槛，也不构成新的硬件操作授权。

下一步收敛顺序：先定位左1通信及轮速零速/反馈语义的具体问题；再收口腕部、头部遗留问题，复用
`robotctl → RPC → S1 scripted → S0 → Feetech → 状态读回 → Disable` 固定入口；
底盘固件、单位、零速和停止合同实证通过后，才补接既有三轮运动学与 Velocity 执行链。
验收目标是同一入口可重复执行、实际状态可见、正常停止及上游失联按既有语义生效；机械臂、头部、底盘分别报告结果。

## 状态

> 已完结阶段的流水账（commit 链 / 审计条目 / 测试计数）整段归档至
> `docs/handoffs/v0.5_status_archive.md`，逐字保留未删改。本节只留当前态——
> AGENTS.md 每轮都整份进模型上下文，历史留在这里等于每次请求重复付费。

```
设计版本:  v0.1.2（冻结）

当前软件:  UTC2026-10-22三轮SDK初始/断扭后±50已复用现有14读稳定性复核，同连接普通READ、原帧保留。
           PV0保留原路径，TE/GV严格0，1秒五组PP/PV、跨度≤1、总1.5秒；初始失败无写。
           断扭后原1秒减速观察不变，±50只触发一次新窗口；不续期GO/21秒、不补写或重试。
           本机138项137过/SDK1跳过；Orin138全过/0跳过，实际SDK模拟串口同方法21场景通过。
           独审10项为138子集；597源及两Python/SDK12首尾一致，实际fmt→根Clippy通过。
           自有质量%171和临时文件已清理、原target复用；sdk_wheel_stop_stability_2026-10-22_r1/summary.json。
           新候选尚未实机，无UART/新GO；后续动作前先通知。D15 OPEN/DEFERRED、deployable=false保持。

已有实机:  UTC2026-10-22前8在上一版一次+500/20秒通过，400次非零SYNC、79组反馈，累计位置9920。
           零速TE1/GV0时PV300→0→0、479.417510ms；断扭后SDK TE/GV/PV严格0。
           诊断/STOP/位置/106身份PASS，现场确认明显持续转动、结束完全停止；manual=false。
           三轮前后六个14读窗口PV及普通速度快照PV均0，位置跨度均0；本次未触发±50容差，不证明根因修复。
           366child结束，两串口四锁释放，自有硬件%169及质量%170已清理；750原件2349179B逐SHA保留。
           网络失败期间没有启动，旧回执过期后按新回执只执行一次；收集及清场校验的首失败亦保留。
           结合此前右9、左7，三轮跨轮次/版本的架空受监督单轮20秒→零速→断扭/停止反馈完成。
           顺序为右轮既有通过、旧前轮失败后用户要求先测左轮通过、最后仅补新版前轮；未重复左/右轮。
           原run整体FAIL仅因固件合同DEFERRED，不覆盖历史FAIL；固件单位/Phase、地面组合和生产Velocity未验收。
           同源597及两Python、实际fmt→根Clippy通过；zero-stop-update/summary.json。
           本轮看护与清场已结束，无后续动作；D15 OPEN/DEFERRED、deployable=false保持。

此前软件:  UTC2026-10-22仅将三轮TE1零速末组统一为原字0/50/8032，前两组保留正向减速及8032。
           原值保留，TE1/GV0、20秒/500ms/21秒不变；SDK初始/断扭后严格0、三轮14读不变。
           本机133项132过/SDK1跳过，Orin133全过含真实SDK模拟串口；实际fmt→根Clippy及独审通过。
           zero-stop-update/summary.json；新候选前轮局部通过见上条，不回填下述前轮原FAIL。

此前实机:  UTC2026-10-22前8、左7按顺序各完成一次+500/20秒，各400次非零SYNC及79组反馈。
           前8零速TE1/GV0时PV250→-50→-50，旧严格末组失败；最终TE0有效ACK、连接关闭。
           原NEEDS_MANUAL_POWER_CUT、STOP/位置/身份UNKNOWN保持；用户拒绝断电并报告实际已停止。
           用户随后明确要求先测下一个，左7使用独立GO及未续期的原现场事实，未重试前轮。
           左7零速PV300→0→0、断扭后0；诊断/STOP/位置/身份PASS，现场确认持续转动后完全停止。
           三轮14读在前8前置、左7前后共9个窗口通过；本轮各窗PV全0，位置跨度0或1。
           左轮原整体FAIL仅为固件合同DEFERRED；588child结束、两串口四锁释放、自有两pane清理。
           1207原件4324871B逐SHA回收；base_wheel_stop_stability_2026-10-22_r1/summary.json。
           历史三轮TE0均出现过0/±50；TE1零速末组本次仅前8复现，阶段不能混同。
           当时仅软件调整零速末组0/±50；随后新版前轮见当前闭环，历史原件与结论不变。

当前只读:  UTC2026-10-22右9完成一次14读，用户未观察并要求重采后，再完成一次同范围14读。
           首轮PV为0/0/-50/-50/0，第二轮0/+50/+50/0/0；两轮PP均仅949–950，首末TE/GV均0。
           第二轮现场全程看清、无转动或抖动；支持近零反馈抖动假设，具体固件原因仍UNKNOWN。
           两轮DIAGNOSTIC_CAPTURED、各105身份PASS/74child回收；STOP未评价、无参数写或启用。
           原件均核SHA，所属%160/%161已清理、看护收尾；right_wheel_feedback_2026-10-22_r1/summary.json。
           只读软件Orin126项全过；后续三轮判据已获软件授权并验证，见当前软件；未追加实机动作。

当前实机:  UTC2026-10-22前8再次完成+500/20秒，400次非零SYNC、79组反馈，累计位置9907。
           保持使能零速PV350→0→0、阶段479.378477ms，断扭后SDK TE/GV/PV均0；未触发8032例外。
           整机停止末段right9普通PV有效原帧0032=+50，原版解码相同；后续UART封锁，左7无GO/未执行。
           STOP/位置UNKNOWN、106身份PASS，原NEEDS_MANUAL_POWER_CUT保持；用户已确认断电。
           用户先说都未动，随后明确更正前轮正常、左轮未动；本次结束瞬间停止未单独确认。
           同$121/%157/PID740265自然dead1/exit1，308child回收，632原件2072867B核SHA，仅所属pane清理。
           front_zero_intermediate_2026-10-22_r1/summary.json；right9具体原因UNKNOWN，不放宽停止门或自动重试。

此前软件:  UTC2026-10-22按用户明确授权，仅前8零速前两组额外接受原字8032并保留原值。
           第三组及前8初始/断扭后/整机停止仍严格零；20秒、500ms、21秒上限与左7原判据不变。
           本机121项120过/SDK1跳过，Orin同121项全过；实际fmt→根Clippy通过，SDK模拟串口覆盖8032。
           597源、两Python及SDK12身份核对通过，软件进程及临时资源清理；只生成PLAN，无GO或新UART。
           front_zero_intermediate_2026-10-22_r1/summary.json；本次实机结果见上条，不覆盖历史失败。

上轮实机:  UTC2026-10-22按同路径前8一次+500/20秒完成，400次非零SYNC、79组反馈，累计9917。
           零速保持TE1/GV0时首PV400、第二组原字8032=-50，触发原速度越界；第三组未执行。
           独立SYNC0及末写TE0 ACK通过，SDK关闭；失败后UART封锁，原STOP/位置/身份UNKNOWN。
           NEEDS_MANUAL_POWER_CUT保持，已要求双电源断电，回执待定；左7无GO、未启动。
           194child结束、两串口四锁释放；400原件1807603B逐SHA回收，仅所属%154清理。
           只读观察器KeyError已按原失败组raw字段核清，未重启硬件；控制源码/停止判据未改。
           同源597及两Python、实际fmt→根Clippy通过，所属质量%155/tmp清理，未重跑测试。
           front_wheel_feedback_2026-10-22_r1/summary.json；先完成失败收尾，不自动重试或追加左轮。
           D15 OPEN/DEFERRED、deployable=false保持。

上次右轮:  UTC2026-10-22右9一次+500/20秒及零速保持使能检查通过，现场确认明显持续转动、结束完全停止。
           400次非零SYNC、79组反馈，累计位置9947；TE1/GV0下PV300→0→0，零速阶段479.519889ms。
           启用至TE0 20.500607833s；诊断/STOP/位置/106身份PASS，SDK关闭、manual=false。
           原run仍仅因三轮固件合同DEFERRED整体FAIL，不回填历史前8+50或先前未看清的轮次。
           310child结束、两串口四锁释放；637原件2071757B逐SHA回收，仅所属%152清理，看护结束。
           同阶段前8只读14项TE/GV0、PP3779跨度0、PV0；用户未观察，仍为诊断而非物理静止验收。
           front_wheel_feedback_2026-10-22_r1/summary.json；本轮未改工具、停止门或参数。
           同源597及两Python复核、实际fmt→根Clippy通过；所属质量%153清理，未重跑测试。
           下一步只沿既有入口补前轮/左轮零速保持使能，动作前通知并确认现场，不复用本轮GO。
           固件单位及统一Velocity链尚未闭环；D15 OPEN/DEFERRED、deployable=false保持。

上轮实机:  UTC2026-10-22右9零速保持使能检查一次完成，TE1/GV0下PV300→0→0。
           零速阶段479.565107ms，启用至TE0 20.500526819s；诊断/STOP/位置/106身份PASS。
           原run仍仅因三轮固件合同DEFERRED整体FAIL；用户未看清，不据此宣布现场动作观察通过。
           用户要求再来一次20秒，原06:57现场时戳不续期、另发新GO；R2初始前轮right8 PV原帧0032=+50。
           当时Mode1/TE0/PP3778；probes空、运动写0，后续UART粘性封锁，STOP/身份/位置UNKNOWN保留。
           用户随后确认两个电源已切断、机械臂和底盘稳定支撑，OFF_USER_REPORTED；原NEEDS标志不回填。
           两轮982原件逐SHA回收，478child消失、两串口四锁释放，仅自有%146/%147清理；软件收尾另清%148。
           本轮仅日志/MD，原生实际fmt→根Clippy通过且597源/两Python同源；不重跑108，不改停止门或Phase。
           base_zero_stop_2026-10-21_r1/summary.json；下一步先核前轮+50的静止证据，不随机重跑右轮。
           三轮固件/单位及统一链尚未闭环，D15 OPEN/DEFERRED、deployable=false保持。

软件基线:  2026-10-21仅在既有pulse添加可选零速保持使能检查，默认路径和合同不变。
           同连接20秒动作后SYNC0，固定50/250/450ms三组TE1/GV0，末组PV严格0，再原清零/独立TE0。
           零速阶段≤500ms、跨下一采样槽即失败；启用总21秒不变，正常收尾固定预留100ms。
           本机108方法107PASS/1SDKskip，Orin108/0skip含实际SDK六子case；独审9是108子集。
           现装nightly1a2c4797e实际fmt→根Clippy通过，597源/两Python/SDK12首尾一致。
           14必要原件逐SHA回收，12child结束，仅所属pane/tmp已清理；原target保留。
           base_zero_stop_2026-10-21_r1/summary.json；本阶段无UART/新GO，实际供电和落地状态未知。
           Phase12静态拒绝、三轮固件合同、D15 OPEN/DEFERRED及deployable=false保持。

当前闭环:  2026-10-21本地（UTC10-22）按同连接SDK路径，先前8后左7各一次+500原始值20秒完成。
           每轮400次非零SYNC、79组PV/PP；累计位置前轮9921/左轮9929，SDK初始及最终PV严格0。
           断扭后PV前200→0、左300→0，各约276ms归零；两轮诊断/17电机停止/位置分类/106身份PASS。
           用户分别确认明显持续转动、结束完全停止；加上先前右9，三只轮子独立受监督单轮闭环PASS。
           每个原run仍仅因三轮固件合同DEFERRED而整体FAIL，不回填；整车组合运动及统一RPC链尚未验收。
           每轮638原件逐SHA回收（共享现场确认），620child结束；仅清%143/%144，SDK关闭、两串口四锁释放。
           同一次04:19:13现场事实授权两轮，独立GO各消费一次、动作前分别通知，未续期或重试，无后续UART。
           本机100方法99PASS/1SDKskip，Orin100/0skip含真实SDK三目标黄金；独审6是100子集。
           nightly1a2c4797e实际fmt→根Clippy通过，597源/两Python/SDK12首尾一致，无新target或服务安装。
           base_wheel_sdk_targets_2026-10-21_r1/summary.json；用户问放地，已建议先关两电源且双臂保持支撑。
           实际OFF/落地未确认，旧架空事实不复用；本轮看护结束，D15 OPEN/DEFERRED、deployable=false保持。

最新闭环:  2026-10-21本地（UTC10-22）动作前已通知并收到当次就绪，仅右9同连接SDK一次20秒完成。
           +500原始值实际发送400次，79组PV/PP、累计编码器9950；启用尝试至断扭20.019634902秒。
           断扭后PV原值250→0，约275ms内停稳；右9诊断、17电机停止及106项身份PASS。
           左7末读PV50按获准稳定性判据接受，其他电机仍严格零；不声称51项值全零。
           用户明确确认右轮明显持续转动、结束后完全停止，本单轮维护闭环PASS；未自动重试。
           原run整体FAIL仅因三轮固件合同DEFERRED，原STOP/历史FAIL不改写；其余两轮和统一RPC链未验收。
           638原件逐SHA回收，310child结束；同$104/%140/PID725892自然exit1，两串口四锁释放，仅清自有pane。
           right_wheel_20s_2026-10-21_r1/summary.json；本轮看护已解除，无后续UART，下一动作仍先通知。
           D15 OPEN/DEFERRED、deployable=false保持；软件20秒窗口不代表硬件断电保护。

最新软件:  2026-10-21本地（UTC10-22）已按官方常驻连接/循环发送对照，复用已装SDK同一连接。
           仅右9固定+500原始值20秒，末次零速及独立TE0，最多1秒只读停稳且最终TE/GV/PV严格零。
           保留左7获准判据及其他电机原停止门；SDK活动/失败时禁止CLI，未写模式/Phase/PID/EEPROM。
           本机与Orin同95项/0skip含真实SDK黄金通过；实际fmt→根Clippy通过，597Rust、两Python、SDK12首尾一致。
           独审及74引用/当前USB/占用/隔离导入通过；仅所属软件pane和临时件已清理，无新UART或GO。
           right_wheel_20s_2026-10-21_r1/summary.json；旧逐命令草稿保留stash，不合入未验证实现。
           此软件候选后续单次实机见最新闭环；下次仍先通知轮子/速度/时长，不复用现场确认或消费GO。
           D15 OPEN/DEFERRED、deployable=false保持；上一轮停止FAIL及用户未见轮动不改写。

最新实机:  2026-10-21本地（UTC10-22）新现场确认后，左轮14读符合获准判据，身份PASS。
           左PP1211/1212跨度1，PV50/50/0/50/0，TE/GV全零；1073.233167ms，单项PASS。
           随后右9一次SYNC0→TE1→SYNC500→SYNC0→TE0，PV50/500、PP3329→3333→3482。
           TE0后60.357593ms起读PV250，原STOP/模式身份UNKNOWN、NEEDS_MANUAL_POWER_CUT不改写。
           用户明确报告轮子没有动；编码器变化不代表可见动作通过，并要求下次动作前先通知。
           用户要求不断电、改20秒；仅保留原要求和未断电事实，不伪造断电或复用已消费GO。
           608原件逐SHA核对，292直属child回收；仅清理%137/%138，两串口四锁无占用，无后续UART。
           left_wheel_stop_stability_2026-10-20_r1/summary.json；后续仅准备同连接SDK20秒候选，实机待验证。
           D15 OPEN/DEFERRED、deployable=false保持；任何下一次动作前先通知目标轮、速度和持续时间。

当前实现:  2026-10-20 用户批准仅左轮right7增加停止稳定性判据，已复用维护入口实现。
           TE/GV仍严格0，PV仅0/±50；固定1秒五组PP/PV、跨度≤1，前后补TE/GV，共14读。
           前后快照各采新窗口并保留原PV；其他电机严格零，失败仍封锁UART并要求人工断电。
           本机及Orin同88项通过；nightly07e526db9下实际fmt→根Clippy通过，597输入及两脚本同源。
           独审及62引用/当前USB/占用核对通过，所属软件pane清理；本阶段无UART或新GO。
           left_wheel_stop_stability_2026-10-20_r1/summary.json；新判据实机及右9动作闭环待验证。
           旧FAIL不改写；D15 OPEN/DEFERRED、deployable=false保持。

当前只读:  2026-10-20 左轮right7固定12读实机完成，首末TE0；PP1211/1212跨度1，PV四次0、末次-50。
           现场全程看清、未见左轮转动或抖动；支持反馈量化/抖动假设，不能据此把±50判零。
           1056.519169ms采集在原1.5秒预算内；仅SYNC_READ，无寄存器写、启用或动作。
           72直属child回收，93项身份PASS；STOP为NOT_EVALUATED，不覆盖原普通READ失败。
           同$97/%133/PID716357自然dead1/exit0，154原件逐SHA回收；两串口四锁释放、所属pane清理。
           left_wheel_feedback_2026-10-20_r1/summary.json；新GO已消费，本次采集收尾，无后续UART。
           原软件77项及原位置/右9入口、STOP51保持；下一步先审轮子停止判据，不随机重试动作。
           D15 OPEN/DEFERRED、deployable=false保持，右9新顺序仍未获硬件评价。

本轮实机:  2026-10-20 新右9顺序尚未执行，初始right7普通速度READ有效原帧为0032（+50）。
           此前独立读取Mode1/TE0/PP1212；全轮88次普通READ、动作写0，后续UART已封锁。
           原STOP51全UNKNOWN、模式身份UNKNOWN及NEEDS_MANUAL_POWER_CUT保持，不回填通过。
           同$94/%130/PID715088自然dead1/exit1，148直属child回收；306原件逐SHA核对。
           用户10:41UTC确认双电源切断、机械臂和架空底盘稳定支撑；OFF_USER_REPORTED。
           两串口四锁已释放，仅所属dead pane已清理；此后无UART或自动重试。
           right_wheel_reference_2026-10-20_r2/summary.json；新顺序未获硬件评价，实际轮动UNKNOWN。
           原Python没有±50死区或实际停止读回；下一步仅准备左轮位置/速度时序诊断，不放宽停止门。
           Orin同源597项/最新nightly07e526db9下实际fmt→根Clippy通过；D15及deployable=false不变。

本轮软件:  2026-10-20 已核对XLeRobot与Orin实际旧Python的调用链，按原版先启用再发非零SYNC。
           仅修改既有poke例子和右9维护入口：SYNC0→等待50ms→TE1确认→SYNC500→原有界采样。
           收尾改为SYNC0→TE0，TE0为末写；补右9普通TE同次原帧，停止门和固件合同不改。
           两端同69项维护测试、Orin示例18项/优化构建及fmt→根Clippy、源码独审通过。
           原生ELF22693d15已绑定，PLAN bbeed8f5及62引用/当前USB/两串口四锁核对通过。
           所属软件进程和pane已清理，复用原target；以上为本轮实机前的软件准备结果。
           right_wheel_reference_2026-10-20_r2/summary.json；软件通过不能覆盖上述实机失败。
           不扩展框架或三轮控制，D15 OPEN/DEFERRED、deployable=false保持。

近次实机:  2026-10-20 右9非零候选仅执行一次，预置GV500后TE原帧读回1，提前失败并断扭。
           实际三写原帧为GV500→TE0→SYNC GV0，没有TE1写；计划Enable及500ms采样均NOT_RUN。
           TE0后TE/PV原帧均零，但SYNC清目标后最终普通READ的TE又为1；该末读无strace原帧。
           两次TE1与GV写仅有时间关联，工具源码/三写帧未发现隐含启用，固件根因仍UNKNOWN。
           其余16电机48停止值零，右9随后PV/GV封锁；原STOP UNKNOWN、NEEDS_MANUAL_POWER_CUT保持。
           同$89/%125/PID710022终态dead1/exit1、PID消失、两串口四锁释放及93项身份通过；
           608份原件795442B已逐SHA回收，296直属child结束，仅清理所属dead pane。
           用户已确认双电源切断、双臂和架空底盘稳定支撑，并报告本次右轮/前轮都没动。
           OFF_USER_REPORTED，不回填原FAIL或历史轮动；此后新UART0、未重试。
           right_wheel_pulse_2026-10-20_r1/summary.json；下一步只核GV写与TE变化关系，
           不再加大速度，不把“预置后TE1”放行，不放宽停止门；D15 OPEN/DEFERRED、deployable=false。

近次零速:  2026-10-20 原SYNC零速R4的PV三次均0、STOP51及93项身份通过，用户报告两轮没动。
           692份原件已回收复核；原整体FAIL仅因BASE_FIRMWARE_CONTRACT_UNVERIFIED，manual标志false。
           base_zero_sync_diagnostic_2026-10-20_r4/summary.json；不回填R3 UNKNOWN或历史±50。
           随后非零候选软件本机/Orin同62项、默认PLAN/缺GO拒绝及独审通过；该结果不等于实机通过。
           源码b3296137/fed9f7fe；nightly07e526db9下实际fmt→根Clippy通过，597源同ae00cf4f。

近次只读:  2026-10-19 前轮right8三项只读实机通过：TE=0，SYNC/普通PV原字均0000。
           当次现场确认后复用既有b2入口，05:55:16–19UTC完成，九发九收校验正确。
           三工具及driver退出0，99直属child回收；六项非UART后验及串口/四锁释放通过。
           原帧和必要日志保留在base_front_pv_compare_2026-10-13_r1/hardware-2026-10-19。
           本轮无参数写/Enable/动作；两次PV非同时采样，旧±50未复现、原因仍UNKNOWN。
           不外推整机停止或底盘动作通过；下一步仍定位轮速启用前后异常条件，不重复静态三读。
           D15 OPEN/DEFERRED、deployable=false保持；本轮采集已收尾，无后续UART。

上次整理:  2026-10-19 按用户授权清理旧会话及重复源码封装，精简H2软件和可视化验收包。
           H2包87.3→7.0MiB，可视化包86.1→18.9MiB；独有日志、原帧和历史差异源码保留。
           旧摘要不回填；三个清理清单记录原SHA及保留位置，源码引用固定1045301。
           Orin最新nightly b6eeb93c9下实际fmt→根Clippy通过，597输入同源且未变。
           普通提交，不改写历史；无新UART/服务/安装，D15及deployable=false不变。

上次Git:   2026-10-18 Git收尾：记录master/dev及双远程规则，按用户要求移除H_evidence大包。
           Orin最新nightly b6eeb93c9下实际fmt→根Clippy零警告；597输入前后同ae00cf4f。
           本次提交收录下述10/13–10/16诊断记录；其中“未提交”为当时状态。
           未完成工作树草稿保留；无新UART、正式安装或服务操作，硬件结论不变。

近次软件:  2026-10-16 前轮right8既有三读候选补齐Orin软件验证：同三项内存测试通过，
           默认入口0、未绑定执行2；同组本机/原生测试不累计，候选源码未改。
           固定b2 stat入口、当前USB和原维护绑定已核；PLAN0、缺现场/GO拒绝2。
           .verify/base_front_pv_compare_2026-10-13_r1/summary.json；新UART0，硬件输出尚未创建。
           仅准备TE原帧→PV原帧→普通PV三读，任一非零/未知仍封锁，不改Phase或停止门。
           已询问本次供电/架空/支撑/持续看护，回执与GO待定；不复用昨日左1现场确认。
           无Rust修改、Cargo重验、正式安装或服务操作；记录未提交，D15及deployable=false不变。

近次实机:  2026-10-15 左1握手及后续三项只读通过：各自当次现场确认后，tmux内复用f201。
           13:10:34–35UTC握手得到型号777；13:20:53–54UTC模式/使能/速度均0，速度原字0000。
           三项分别PING+型号READ+目标READ，九发九收均ID1/error0/校验正确；无参数写/Enable/动作。
           三工具及壳exit0；目标串口/四锁无人占用，所属PID退出；本次指定只读范围完成。
           .verify/left1_communication_2026-10-15/summary.json；旧超时未复现，原因仍UNKNOWN。
           未追加其他电机或动作；不外推长期稳定或整机停止通过，返回既定最小闭环时只选下一实际阻塞。
           未改Rust、正式安装或服务；D15 OPEN/DEFERRED、deployable=false保持，记录未提交。

上轮记录:  2026-10-13 R2重复入口实际执行，首左1握手失败，right9诊断NOT_RUN。
           首09:39:27现场事实后fresh17/23、USB两侧无变化、58refs及唯一PLAN通过。
           root准备耗过09:49:27期限，旧本地GO未启动保留；用户再次确认仍就绪、立即执行。
           新事实09:50:54至10:00:54，独立GO仅执行一次；main3b/helper70/b2冻结件不变。
           driver669897/@112/%112自然dead1；seq43左1 read Operating_Mode exit1，64.489704ms。
           over_budget=false，stdout0B，stderr72B为bus.connect握手电机无响应id1；具体原因UNKNOWN。
           全run只有一次UART工具调用，SYNC_WRITE/WRITE/Enable0；后UART粘性封锁，STOP/身份UNKNOWN。
           61child/122原流全部真回收与逐SHA核实，run3f804c06/213920B保持NEEDS人工断电。
           START至同dead观察均在新GO内；首observer本地绑定失败/pidnull保留，修正后只观察同进程。
           collector仅所属dead pane终态/回收2child，无新UART或独立17后验；156原件715be0c1封存。
           用户09:54:45已确认双电源切断、机械臂/架空底盘稳定支撑；OFF_USER_REPORTED，看护结束。
           .verify/base_zero_sync_diagnostic_2026-10-13_r2_hardware/summary.json；不回填原NEEDS/STOP/身份。
           下一步先核左侧电机供电/总线连接，不自动重试右轮或放宽停止门；旧±50原因仍OPEN。
           R1右9SYNC三次PV0子项PASS但前轮8终读+50的整run失败保留，新轮没有右轮动作观察结论。
           front3候选仅本机3方法通过/未执行；SSH恢复、旧quality目录不存在，未补跑提交质量。
           本轮记录未提交；正式安装与服务未改，旧5ee8a9c/1eb89ee未push，最后远端仍67d8b2a。
           腕部/头部长期参数与三轮主动运动/固件合同OPEN；右H2完整PASS、D15 OPEN/DEFERRED。
           deployable=false；架构、S0/S1接口与服务未改。

实现阶段:  v0.5 候选 E ✅ 受监督功能 demo 完结（2026-09-01 真机：单臂×2 +
           双臂 + 双夹爪全 PASS + RT 三层加固回归全绿；收口宣称唯一口径 =
           "机器人控制执行面受监督功能 demo 跑通"）。
           D15 硬件 fail-safe 保持 OPEN——不宣称生产替换 / 无人值守；正式
           退役等 post-E 生产安全硬化候选闭环（D-BI3 r5：旧栈已 disable
           自启，unit 保留可回滚）。
           当前 = post-E 通信故障硬化 Spec Approved（2026-09-05 round 21，
           8 项决策全部 ACK）；函数级 Plan Approved，按用户2026-09-14最新分工：
           root定方案与评审，原生子agent具体开发和测试，各阶段验收通过即提交。
           实施状态与证据 = team-swe/specs/post_e_comm_fault_hardening.HANDOFF.md；
           已落地：新权限、高/普通双队列、唯一writer、RPC责任账本与受监督关闭链。
           R4c监督退出与真实重启、R4d-A原连接竞争已验收提交；A基线f9a85be。
           R4d-B双主机同源软件验收已通过：本机14fresh/53复用，Orin有效67门。
           原四失败保留，独审与root验收均通过；详HANDOFF及H包，不称单轮全过。
           B已提交cdeddd2并推送origin；用户2026-09-20授权继续P-E软件长负载。
           P-E两端All72、4096负控和8192长载已验收提交2f1de02，未push。
           P.7基线7c9ffaa已验收。9/22左H2一次前置拒绝，driver exit1/reap，
           authorize/launch均0，未触电机；原拒绝及两个未知PID清场FAIL保留。
           预检修复1b6db3e/4191bf0已独审；50项Python、真实sudo集成通过。
           Orin新build HEAD4191bf0：生产4/测量3、7help/15收尾与只读复检通过。
           首native后零启动计数误拒已修，原FAIL保留；不称单轮全过。
           该软件修复已收口b4ff720；后经继续及“本次全部就绪”确认执行一次左H2。
           本次RT门通过，topic-idle 100+1000 PASS；topic-inflight 100+639后FAIL。
           seq13681注入请求3895634ns、实测3894208ns，短1426ns触发原严格门。
           authorize/launch各1，工具与driver exit1/真实reap；独立断扭和清场成功。
           第3/4组未执行，原FAIL证据已收口eb59bcc，不自动重试。
           注入延时/尾窗修复fb600fd已软件验收，c5daa67运行本次新左H2通过。
           四组各100预热+1000正式、原P99门通过；工具/driver exit0真实reap，断扭/清场通过。
           独审/root已验收，本轮已收口10cff7a；三次旧测量FAIL及一次前置拒绝完整保留。
           第三组SLA-B max52.502592ms保留，判据为P99；收尾电源UNKNOWN，本轮值守已解除。
           右工具0047920与W1测试2e5b515已本地集成并验证；合并态64Python及最终SHA10项、fmt/Clippy通过。
           合并5952498已完成Orin新原生绑定并通过独审：6recipes、生产4/测量3、7help、64+10、单mock4×2。
           18次真实完成、122原件已核；原生软件证据已归档并收口4441f59，实际build仍5952498。
           右H13本地准备已收口9afdc92；首Orin22方法3FAIL/3ERROR因Python缺pidfd API，原FAIL保留。
           native/implementation_r3按能力选择自有child回收，本地27+独立5及Orin27/默认入口通过。
           该历史软件阶段真实wait、前后状态相同且无收养child；已归档，历史round6与早轮回执缺项保留。
           右H13软件已收口3790fd9；9/23本次右H1/H3各100+1000测量通过，实际build仍5952498。
           两工具真实reap/清场成功；首readonly冲突与setsid前置拒绝FAIL保留，证据已独审/root验收并归档。
           右rx_trust.lock由工具首次合法创建，marker仍缺失、latch不变；值守已解除、电源UNKNOWN。
           W1 fork/exec窗口机制已证实，旧偶发失败具体根因仍OPEN_UNCONFIRMED；原FAIL保留。
           历史交接（截至2026-09-25）：左H1/H2/H3、右H1/H3通过；右H2四组测量PASS但最终Disable约2秒Cancelled，
           整体run/report FAIL；首USB瞬态及漏传alignment零启动FAIL保留。工具/driver真实reap，
           独立清场PASS，右marker/latch均Dirty；用户2026-09-23先确认断电、后报告通电且电源/USB重插，
           本次交接时物理现状未知，支撑/接触区未重新确认，重插不构成新H2授权。软件修复ca14f0e已提交推送，
           Orin源码ff-only对齐，571源聚合ae5a04ff、生产4/测量3原生构建、mock AC-12与
           final9/Endpoint1/Lower2定向测试仅软件通过，证据已入H包；2.1秒延迟回执
           端到端及右H2硬件重测未做，正式安装未做，
           六服务停止，deployable=false、D15 OPEN。
           证据保留当前包，旧过程数据与重复文档按用户授权清除。
           Orin 调试 / RT 检查需要权限时直接 sudo，不放宽既定验收预算。
           当前续作（2026-09-29）：最终Disable共用入口及2.1秒真实tonic回环已软件验收。
           Orin最终fmt/根Clippy、lib4+工具182、回环6/9场景、66Python及绑定peer串行workspace通过。
           默认并行S1两例超时仍OPEN，旧FAIL不覆盖；已更新HANDOFF，阶段提交、不push。
           此次未核现场物理状态，右H2仍整体FAIL，D15 OPEN、deployable=false。
           同日后续：ca14冻结件双臂腕部各一次往返及断扭收尾PASS，头部/底盘未测主动运动。
           已加穷尽诊断分类，本机147+5、隔离Linux ARM根Clippy及167+42通过；该阶段Orin因SSH信任待核未测。
           原日志不回填Revoked，读早退/过期tick问题独立OPEN；详情见HANDOFF及hardware_smoke报告。
           9/29 Rerun软件接入已验收：独立robot-viz、十类pub/sub、有界隔离镜像及真实Viewer文件/实时画面。
           macOS与Linux专项17+6通过；Linux根fmt/Clippy、完整串行workspace、13退出、609 RT库测试及优化构建通过。
           源码/动态独审已完成，原失败及有损/多发布者公平性限制保留；证据见 .verify/robot_viz_2026-09-29/summary.json。
           该阶段未push、未部署Orin，SSH信任当时待核；上述Linux全仓结果来自隔离Lima。
           当前续作（2026-09-30）：严格SSH已恢复，Orin独立目录验证a6957cc的620项源码快照。
           已更新浮动nightly至2026-09-30/rustc1.101.0-nightly(5c543b0b8)，fmt/根Clippy、
           17单测、9镜像测试（另1子夹具ignored）、6显式SHM、独立优化构建/help通过。
           独立构建与首集成桥SHA不同，最终0097228b桥已补跑同六项通过；共32项不同测试，不计为38项。
           源码/产物前后身份及真实回收通过；正式ca14 clean、四件SHA不变、六服务停止且无控制进程。
           最终正常RRD官方同版verify通过：1000帧/10类/11实例，gap/drop/reject均0。
           Orin定向软件已验收归档，独审/root通过；本轮SIGINT夹具0帧仅证明生命周期。
           新证据入口 .verify/robot_viz_orin_2026-09-30/summary.json；本轮未重跑完整workspace。
           正式ca14安装未改、未做新硬件动作，右H2/D15与deployable=false不变。
           本阶段仅本地提交、不push；下一步真实发布者观测及硬件门仍需各自验证。
           同日H2续作：产物校验V2跟随最新nightly，前后真实编译器身份绑定六配方；新执行拒V1。
           59120e34原生六配方/七冻结件通过，浮动nightly实际更新为5c543b0b8；589源聚合48360e9f前后相同。
           fmt/根Clippy、71 Python、默认PLAN_ONLY、helper14真实机制及七help通过；非完整workspace。
           冻结测量件mock AC12四组各2软件样本、同源tonic6测试/9场景通过，硬件H2未测。
           release/post/archive独审与root验收通过，253原件及本机证据已归档至 .verify/h2_nightly_v2_2026-09-30/summary.json。
           04:29:11UTC正式后验ca14 clean、四SHA未变、六服务inactive且无控制进程；本阶段无安装/push。
           9/30用户确认无独立自动断电保护，决定暂缓D15实施/验收；状态OPEN/DEFERRED，不记PASS。
           D15工作单保留CodeMap与建议矩阵，非Spec Approved或硬件实现；供电分域未知，本轮不再追问/选型/采购/接线。
           受监督demo沿既有D15-R继续；下一步右H2完整复测，原FAIL与deployable=false保持。
           同日独立软件候选：周期Motion写仅Revoked后重检原安全门，由PeriodicRead独立申请读租约；
           外层deadline保原相位、跳过错失周期至严格未来点，零周期拒绝、溢出保原pending移交。
           两组Orin red原断言FAIL与首次联网setup SIGINT130保留；latest nightly5c543b0b8下定向软件门已通过。
           本机fmt、原生fmt/根Clippy通过；调度6、诊断7、rt_guard+commhook S0lib710、serial_rt_e2e5全过/0ignored。
           调度6与诊断7均为710子集，不累计；两端589输入逐项一致、验证后复核一致，聚合c0388807；三变更SHA批次首尾一致。
           证据 .verify/serial_periodic_recovery_2026-09-30/summary.json，已归档，独审与root验收通过；未部署/未push。
           该周期恢复候选阶段未跑完整workspace或release构建，不外推硬件通过。
           同日续作：b01603b已快进整合，Orin新V2六配方七冻结件通过，更新nightly仍5c543b0b8。
           589输入两端/构建前后逐项一致、聚合c0388807；本轮fmt/根Clippy、71Python、七help、mock4×2、tonic6/9通过。
           S0lib710与serial_rt_e2e5只引用周期恢复同源原件，不累计；release/post独审与root软件验收通过。
           证据 .verify/h2_serial_recovery_2026-09-30/summary.json；正式四件与服务/proc批次中后一致，未部署/push/硬件动作。
           本轮非完整workspace；阶段文档提交后remote快进对齐，再新alignment/当次现场事实/独立GO。
           右H2历史整体FAIL、D15 OPEN/DEFERRED及deployable=false保持；详HANDOFF。
           历史34条Unknown未证实归因、右H2整体FAIL、D15 OPEN/DEFERRED及deployable=false不变。
           同日右H2实际一次：runtime4a8e171/buildb01603b，四组各100+1000原P99门全过，第四组1分类不匹配保留。
           最终非dry Disable Ack/Disarm与工具exit0/reap通过；driver严格trace门拒3条IpcClaim跨轮标签，整体FAIL。
           driver/SSH exit1真实回收，独立11项清场、17电机34项零扭矩使能/零速度读回、后11+16门通过。
           正式四件未改、六服务停止、无控制/读回进程，两USB/两侧锁无持有者；右Dirty/LATCHED、左保护文件不变。
           19:01:19UTC收尾后值守已解除，现场电源/支撑UNKNOWN；头部/底盘主动运动未测。
           证据 .verify/right_h2_2026-09-30/summary.json；整体FAIL及历史Unknown保留，下一步先闭环trace上下文软件问题。
           同日S1测试夹具软件修复：仅cfg(test)独立Node/端口RAII与发送前连接刷新，生产25621B前缀不变。
           Mac定向默认并行15/libClippy、隔离Lima根fmt/Clippy及S1完整默认并行287通过，0failed/ignored。
           589输入仅该测试文件变动、聚合72ac7a50，root最终集成同源及提交前门通过；非Orin/非完整workspace。
           原s49两例超时和Mac/Linux/独审/root各原FAIL保留，历史具体归因OPEN_UNCONFIRMED。
           .verify/s1_parallel_events_2026-09-30/summary.json已验收；下一步非RT工具trace归属/尾采集软件修复。
           无正式安装/部署/push，D15 OPEN/DEFERRED、deployable=false不变；新硬件整轮须新准备/现场事实/GO。
           同日非RT SLA工具修复：整次真实Arm/Ready owner与一次真实reap后尾drain，raw/capture/spool保留。
           新run稳定排序/完整CAS派生，A/B、原正式集合、RT/authority与Python严格门不改；未知/错代次/故障等仍FAIL。
           新共享工具容量门44000owner/8192epoch事件/256B行/8192retry bins，溢出留原件硬拒，旧长载未重验。
           隔离Lima最新nightly5c：fmt/根及featureClippy、默认并行194含12oracle、合成4401/4000与5反例、短mock4×2通过。
           主树589输入83048e98仅sla变化，独审/root与提交前fmt→根Clippy、uv合同复验通过；原FAIL保留。
           .verify/h2_trace_owner_2026-09-30/summary.json；非完整workspace、未Orin新V2冻结/真实RT环暂停/硬件H2。
           用户另确认现场条件后推进独立维护层硬件验证，结果与本软件阶段分开；旧H2/D15/Unknown/deployable=false不变。
           当前维护分类续作：精度/17×3停止/逐项身份后验分开，收尾失败粘性与+24/±4/800ms保持。
           本机/Orin同35及默认零动作通过；r4未动作轮漂移漏项已修并保留原FAIL；三轮live固定DEFERRED。
           1c5758新Orin V2六配方/七件、589聚合83048、latest nightly5c及71Python/7help/mock4×2/tonic6+9/12owner通过。
           12是历史194子集，本轮非全仓/硬件；维护35另源另门不混入71。提交前fmt/根Clippy再次通过。
           独审/root通过；证据 .verify/hardware_maintenance_classification_2026-09-30/summary.json，两份Orin原包不回填。
           人工断电待答、H2/左腕原FAIL/D15 OPEN_DEFERRED/deployable=false不变；底盘纯运动学独立实施、未接S0/S1。
           当前底盘阶段A：XLeRobot前8/左7/右9具名SI正逆变换/同比限速，几何与上限显式，原ArmIK不变。
           Orin latest nightly5c、fmt/根Clippy、crate30（21底盘子集+9手臂）通过；590聚合0e580bd9，首lint101/30NOT_RUN保留。
           独审/root及提交前质量绑定通过；证据 .verify/xlerobot_base_kinematics_2026-09-30/summary.json，非S0/S1/raw/Phase/硬件。
           当前B0：Phase18/CW26/CCW27原始字节诊断读取，四具名写API及poke开串口前拒三名写入。
           主树五源已格式化整合，590聚合3d8491ee；前两Clippy101保留（E0004/四测试lint），新错误归原参数类(8,1,0)。
           Orin最新nightly5c、fmt/两根Clippy、库160+例子5、release及9次真实exit2拒写通过；18新方法为165子集，独审/root通过。
           证据 .verify/feetech_diagnostic_metadata_2026-09-30/summary.json；本轮非全仓测试或H2六配方。
           B0不解码Phase/换算单位/接S0/S1或执行硬件；人工断电PENDING、H2/左腕FAIL、D15/deployable=false保持。
           同日维护硬件：现场XLeRobot底盘前8/左7/右9既有速度模式1只读确认；脚本两端同19项/仅计划与独审通过。
           首uv父壳前置拒绝UART0；新run仅左腕947→967→953，回程+6超±4/800ms，整体FAIL，未重试。
           断扭ACK、17电机终读零扭矩/零速度、300child回收及独立无UART后验通过；原入口/位置/后验FAIL保留。
           右腕/头部/底盘动作未执行，未接S0/S1；原NEEDS_MANUAL_POWER_CUT不回填，物理断电确认待答/供电UNKNOWN。
           .verify/hardware_maintenance_2026-09-30_r2/summary.json；本轮无后续动作、无安装/部署/push，H2/D15/deployable=false不变。
           当前续作（2026-10-08）：Velocity入站与SerialTick归零基础六源已整合，非完整底盘B1。
           Orin独立导出源基于8fb17b+六处dirty，native_git_head=null；590聚合43b9d675首尾一致。
           最新nightly1d81eb4ad、fmt/默认及组合根Clippy、串行S0lib641/734与serial_rt_e2e5通过。
           库去重734+独立e2e5=739，24新方法为库子集不累计；原六轮FAIL/NOT_RUN/网络255保留。
           r7远端13条命令原回执exit0/reap通过；旧本机owner/SSH缺失，wait/reap UNKNOWN，不由远端PASS回填。
           原RR超时具体归因仍UNCONFIRMED，旧ELF单项PASS与keep-going诊断均不计739。
           .verify/xlerobot_velocity_foundation_2026-10-08/summary.json已归档，独审/root及提交前fmt→同源根Clippy通过。
           旧ELF仅身份未封存；追加包历史stat名册未持久化，原件字节与源码首尾可独核。
           P1A身份/P1B轮速配置仅孤立Mac软件候选、未整合；真实wire期限和底盘固件合同未闭环。
           用户已重新确认电源/USB复位、支撑和持续看护；本软件阶段未下发新硬件GO，不回填旧NEEDS/FAIL。
           本阶段未正式安装/硬件动作/完整workspace测试/release/push；H2/左腕FAIL、D15 OPEN/DEFERRED、deployable=false保持。
           同日独立维护：固定位置目标选择的本机/Orin同源40项PASS（新5为子集）；READ独立170项/211child PASS。
           使用历史f201位置维护ELF，重新核本体，非最新nightly新Rust控制产物；B0工具不作位置替代件。
           本次仅右5→左7→左8，右5往返PASS，head1回程2642/2634、+8超原±4/800ms而FAIL；head2/左腕/base主动动作NOT_RUN。
           最终STOP 50/51整数零，right9 PV=-50/TE0/GV0而FAIL，报告NEEDS_MANUAL_POWER_CUT；base主动写0/DEFERRED。
           359driver及52wrapper child真wait/reap、216非UART后验/正式ca14四SHA/六服务停/四锁释放/保护不变PASS不能覆盖FAIL。
           人工断两个电源已通知、确认PENDING；停止本轮，禁止后续UART/动作/自动重试，不推断-50来源、单位或轮转动。
           厂家STS内存表（门户version=181）：Phase12 bit3使零目标为最高速度；现场FW主/次未读，未作Phase/Lock/PID/模式/EEPROM写。
           .verify/hardware_selected_targets_2026-10-08/summary.json封本轮原件，前置/漏fixture等旧FAIL全保留。
           随后用户确认两个电机电源已切断、机械臂保持支撑；root于06:35:40UTC记录收到确认，实际断电时刻/遥测未知。
           新确认结束本轮看护收尾，不回填原NEEDS/FAIL或构成新GO；其后没有UART/动作。
           同日固件合同七源：新增地址0/1只读版本名，四写API及poke开串口前拒写；普通Phase写禁令保持。
           纯typed轮速profile要求具名ID/777/模式1/明确FW/完整Phase与raw上限，拒bit3零即最高速，不赋予Enable权限。
           bit2按厂家名义0.732/0.0146RPM换算、bit15符号幅度；超限拒绝而不单轮clamp，现场FW/单位合同仍未硬件验证。
           Orin591输入ddad9513、最新nightly1d81、fmt/默认及real-hw根Clippy、171库+6poke通过，新增12为177子集。
           新独立release ELF bde890bb已冻结，help/两固件名写入预期exit2；16不同child真reap，源/工具/ELF首尾一致。
           未做strace/全workspace/H2六配方/新RT硬件门或正式安装；证据 .verify/feetech_firmware_contract_2026-10-08/summary.json。
           同日P1A/P1B/P1C十一源已整合：完整Node/Port来源、显式轮速参数、纯有界新鲜度/重建状态机。
           生命周期投影不代替authority；当前/退休各一组来源，无生产IPC/writer/Enable接入，Velocity仍无TX。
           Orin596/b919cd32、实际浮动nightly1d81，fmt/默认及rt_guard+real-hw根Clippy通过。
           core147+transport129+guard37共313不同Rust方法及doc1通过；mock61/default36不累计，原库ignored2/docignored2保持。
           18原生/5本机child自然回收、38源窗口/编译器同；主树整合后596与实际fmt绑定原生提交前门。
           两轮Clippy101与r4名单夹具FAIL保留；证据 .verify/b1_ingress_components_2026-10-08/summary.json。
           非完整workspace/release/RT/H2或硬件门，无安装/服务激活/push；断电确认后无UART/新GO。
           下一步来源绝对期限串口写仅函数方案，尚未实现；FW未读/Phase12未改，左腕/头1/H2 FAIL、D15 OPEN/DEFERRED、deployable=false保持。
           同日来源绝对期限写六源已整合：借RobotClock的固定source/call期限，首写、partial及返回时严格到期拒绝，
           不续期/整帧重放；新普通写入口强制显式probe，未知transport默认拒绝，旧Position/Safety/Confirm保持。
           Mac与Orin分别217不同方法（库200+集成12/5）；29新增及default/real-hw同名200不重复累计。
           Orin r2实际fmt→两根Clippy/default200通过、负控Display误拒原batch FAIL保留；r3仅剩余门通过。
           两批19原生命令/42源码窗同596 bfe2318f及nightly1d81，所有自有child自然回收，不称单批全过。
           root主树实际fmt0、整合后全部596同原生，复用上述同源质量门；旧Mac r1-r6/定位/native准备FAIL保留。
           .verify/feetech_absolute_write_2026-10-08/summary.json封必要原件与唯一源码tar，独审/root软件验收通过。
           未接S0/S1或真实轮速TX，下一步专用wheel adapter另验；无全workspace/release/H2/新硬件/部署/push。
           头1回程+8、右9停止读值-50原FAIL保持；用户已人工切两个电源并支撑，无新GO，FW未读/Phase12未改。
           左腕/右H2 FAIL、D15 OPEN/DEFERRED及deployable=false保持。
           同日固定缓冲原始读回两源已整合：MAX32固定staging，完整组校验后一次提交，原word/尾槽保持。
           具名表仍仅1/2B，4B只核同生产私有核心；沿旧read调用预算，不证明绝对READ期限/USB身份。
           Mac与Orin各230不同方法（lib213+集成12/5），13新增为213子集，default/real-hw213重叠不累计。
           Mac最终12child自然回收，逐child只保存聚合/相等字段，批次首尾有完整597清单；未nocapture或导出nestedPID。
           Orin本轮15child/32完整源清单/7本机owner均真实回收或核验；两8B负控SIGABRT(core dumped)原行保留。
           main实际fmt0与597/5c8c4c04绑定本轮Orin fmt→两根Clippy及nightly1d81；Mac首轮编译FAIL/r2PASS保留。
           .verify/feetech_wheel_raw_read_2026-10-08/summary.json为验收入口；无全workspace/release/RT/H2/新UART/安装/push。
           未接S0/S1或真实轮速TX，连接/固件/Enable与专用wheel adapter另验；断电确认后无新GO，旧FAIL/D15状态不变。
           同日按用户“过度设计”反馈暂停P3-A打开会话/P3-B事务代次框架，未完成草稿保留工作树，未合主树。
           第三件绝对READ合同暂缓未派实施；仅既有feetech_poke新增read-raw，Mac/Orin各13工具方法通过。
           Orin fmt/两根Clippy/release通过，首离线缓存resolver101保留；主树同597/03a11，证据 .verify/feetech_poke_raw_2026-10-08/summary.json。
           优先头1回程+8/右9终读-50的实际原帧、固件和参数事实，不改精度/停止门、不回填旧FAIL或缩减整机目标。
           历史规则：2026-09-23用户因额度要求暂停；2026-09-25仅授权将当前进度
           写入handoff并推送Git，之后到别处开发。本次交接经独审/root验收后commit/push，
           当时暂停开发及真机测试；真实H2仍需新物理重置/支撑/在场事实与独立GO。
           RX 重同步 / 串口 reopen 已移出本候选。
           后续仍需 ①通信故障硬化硬件门 → ②D15 退役决策 → ③F7 baseline 判据小件；
           Rerun软件按用户后续目标先行完成，Orin部署/硬件观测另验，通信硬化不自动关闭D15。
           详 docs/handoffs/v0.5_demo_run_2026-09-01_summary.md
           + docs/CHANGELOG.md 2026-09-01 两条。

已完结:    候选 E  替换执行面(受监督demo)  → v0.5_demo_run_2026-09-01_summary.md
           候选 A  operator CLI (robotctl)  → v0.5_operator_cli_summary.md
           候选 B  GetState/StreamState     → v0.5_get_stream_state_summary.md
           候选 C  SetEnabled RPC           → v0.5_set_enabled_summary.md
           候选 D  EStop 触发 RPC            → v0.5_estop_summary.md
           v0.5 基建  iceoryx2 0.9.3 + 全依赖升级（2026-07-12：解 toolchain pin +
                      撤 mem::forget workaround ×2）→ 见归档
           v0.4 收口  候选 1 / 2(r1+r2) / 3 / 4 → v0.4_*_summary.md
           v0.3 Phase A/B + v0.2 收尾        → v0.3_*_summary.md + 见归档

§6.7 真宣称: #1 ✅ S0 存活 = 软件层安全充分条件
            #2 ✅ systemd 看门狗（v0.3 A2 freeze hook + freeze_systemd_smoke 端到端真锁）
            #3 ✅ EStop 不可清为失忆态（v0.3 A1 ClearEStop control action 闭环 + A1-5 撤
                  --allow-empty-latch；唯一清 latch 路径 = ClearEStop control action）
            #4 ✅ S0 重启不失忆（v0.2 fsync 双写 + crash_restart_smoke 5/5 PASS）
            #5 ✅ 幂等写 RPC（IdempotencyCache + idempotency_key fingerprint + idem_replay e2e 真过）

上游项目:   dimos / dimos-deploy（只读移植参考；本地路径以实际 checkout 为准）
```

## 核心不变量

来自设计文档 §6.7，任何代码修改都不得破坏：

1. **S0 存活 = 软件层安全充分条件**；S0 不在时靠硬件 fail-safe（厂家电机 command timeout）。
2. **systemd 是最外层看门狗**；`sd_notify` 超时 → 重启。
3. **EStop 不可被软件清除为失忆态**；持久化到 `/var/lib/robot/<id>/estop_latch`，只能由 `ClearEStop` RPC (maintainer+) 解除。
4. **S0 重启不失忆**：EStop / fault code / 最后一次 safe 输出都 fsync 双写。
5. **幂等写 RPC**：同 `CommandMeta.idempotency_key` 不产生重复副作用。

## S0 线程模型（v0.2 骨架；v0.3 A1-3 加 rr_server_thread）

```
s0_ipc_thread        普通优先级    iceoryx2 订阅 command/heartbeat  → 写 latest-command slot
                                   (v0.3 A1-3) drain RR control channel → action executor
s0_rt_thread         SCHED_FIFO    读 command slot → HardRT adapter → 写 rt_snapshot
s0_pub_thread        普通优先级    读 rt_snapshot  → iceoryx2 publish state/error/heartbeat
s0_io_thread*        普通优先级    poll SoftRT adapter             → 写 softrt_snapshot
s0_rr_server_thread  普通优先级    (v0.3 A1-3) iceoryx2 RR server I/O → forward 到 ipc_thread
```

硬约束：**RT thread 不碰 iceoryx2、不 `Arc::clone/drop`、不 heap allocation**。违反 = 破坏实时性。

> **多 bus 场景语义**：v0.2 骨架 M=1，上述 `s0_rt_thread` 是单例。v0.4 起真多 bus 落地时（XLeRobot 2–3 × USB-RS485、ATOM01 4 × CAN），`s0_rt_thread` 按 **"每 HardRT bus 一实例"** 展开（方案 B：tick 闭环 drain-RX → read target → safety → serial write → per-bus snapshot slice）。详细语义 + 候选回写 §1.2 触发条件见 [`docs/CHANGELOG.md`](docs/CHANGELOG.md) 2026-04-24 "S0 `s0_rt_thread` 多 bus 语义澄清"。

## v0.2 实现顺序（已收尾，留作历史参考）

设计文档 §11.2。v0.2 Step 1–10 在 `0fa1d77` 全部 land，**最小闭环 + Orin
真机三轮闭环全通**。当时后续走 v0.3 Phase A → B 路线（详
[`docs/handoffs/v0.3_entry.md`](docs/handoffs/v0.3_entry.md)），不再按
Step 序号推进。

```
1.  robot-core::time                 ✅
2.  robot-msg schema registry        ✅
3.  FrameHeader / Body / Blob parser ✅
4.  SampleLease<T> / SampleWriter<T> ✅
5.  mock iceoryx2 pub/sub            ✅
6.  S0 四线程 mock                    ✅
7.  watchdog + CONTROL_LOST_* 切换    ✅（task 24-29）
8.  EStop latch 文件双写 + 重启恢复    ✅（fsync 双写真锁住 §6.7 #4）
9.  crash/restart 测试套               ✅（crash_restart_smoke 5/5 PASS）
10. Feetech 单关节                     ✅（Orin 三轮闭环全通：±5° / ±17° / 双端 ±17° 同步）
```

## 规划的 workspace 布局

```
robot-system/
├─ Cargo.toml                # workspace 根（无 [package]）
├─ rust-toolchain.toml       # nightly（docs/00_design.md §9）
├─ .cargo/config.toml        # sccache + target 目录
├─ configs/                  # 机器人配置 TOML
├─ 3rd-party/                # vendor flatc、可能的 iceoryx2 patch
├─ docs/
│   ├─ 00_design.md          # v0.1.2 冻结设计（权威）
│   └─ 10_verification.md    # 验证清单（v0.2 已落地，v0.3 持续维护）
├─ scripts/
│   ├─ deploy.sh
│   └─ deploy-windows.ps1
└─ crates/
    ├─ robot-core/           # trait、错误、time；零 runtime 依赖
    ├─ robot-msg/            # FlatBuffers schema + 生成代码
    ├─ robot-transport/      # iceoryx2 封装 + SampleLease<T> / SampleWriter<T>
    ├─ robot-kinematics/     # IK / FK / 雅可比 / 底盘运动学 trait + 纯类型（零 runtime 依赖）
    ├─ robot-kinematics-<robot>/  # 每种机器人一个实现：xlerobot / piper / ...
    │                        #   v0.4 起按需启用；用户可 fork 自己的 robot-kinematics-<mine>
    │                        #   而不动 s1 / s0 / adapter
    ├─ robot-s0/             # 四线程二进制
    ├─ robot-s1/             # coordinator 二进制（持 Option<Box<dyn ArmIkSolver>>，按 feature 选实现）
    ├─ robot-policy-host/    # ExternalPolicyHost 二进制（Python/ONNX/HTTP）
    ├─ robot-adapters/
    │   ├─ mock/
    │   ├─ feetech/
    │   ├─ piper/
    │   ├─ realsense/
    │   └─ webcam/
    ├─ robot-rpc/            # tonic gRPC 网关
    ├─ robot-s2-bridge/      # pyo3 module
    ├─ robot-viz/            # Rerun / Foxglove 桥
    ├─ robot-recorder/       # record/replay
    ├─ robot-cli/            # robotctl
    └─ robot-tests/          # soak + crash/restart + schema_compat
```

**IK 分层说明**：IK 住在 S1，不进 S0 / adapter。`robot-kinematics` 定义 `ArmIkSolver` / `BaseKinematics` trait；每个机器人一个 `robot-kinematics-<robot>` 实现，按 feature flag（`xlerobot` / `piper` / ...）挂到 `robot-s1`。替换机器人 = 换一个 feature，s1 / s0 / adapter 都不用改。实现时机见 `docs/01_implementation_steps.md`（v0.4 起），trait 草案 v0.3 前定。

## Rust 规范

- **Rust 默认使用最新 nightly，严格不用 stable**。`rust-toolchain.toml` 保持浮动 `channel = "nightly"`，使用前先更新最新工具链；对应 `docs/00_design.md` §9。
- **Edition 2024**。**不写 `rust-version`**；记录实际 `rustc -Vv` / `cargo -V` 用于溯源，不因记录或复现需求默认 pin 日期。
- **核心 crate 默认不用 nightly-only feature**：`robot-core` / `robot-msg` / `robot-transport` / `robot-s0` / `robot-s1` 原则上不写 `#![feature(...)]`；确需启用必须在 crate 顶部注释列出并过 review。目的是**降低 nightly 升级导致的回归风险**，不是留 stable 后门。
- 工具类 crate（`robot-cli` / `robot-viz` / `robot-rpc` / `robot-s2-bridge` / `robot-tests` / `robot-recorder`）可自由使用 nightly feature。
- 新 nightly lint 优先就地修复；仅真实编译回归阻断时临时回退已验证日期，并登记原因、影响范围与解除条件，修复后恢复浮动 nightly。CI 不跑 stable 回退演练。
- Allocator 按 §0 表：S0/S1 用系统默认 + RT 禁 alloc；外部工具（cli/viz/rpc/s2-bridge）用 mimalloc。
- `panic = "abort"` **仅限** `[profile.release.package.robot-s0]` / `[profile.release.package.robot-s1]`；其他 crate 保留 unwind。
- 控制流多分支用 `match`，避免 if-else 链（全局 §5）。
- **`cargo fmt` + `cargo clippy --all --all-targets -- -D warnings` 必须在每次 commit 前零警告通过**。
- CI 严格串行（全程 nightly）：`fmt → clippy → test → build → cargo deny → allocation-guard → schema_compat → soak (夜间)`。

### 业务代码禁用的 API

- `SystemTime::now()` / `std::time::Instant::now()` / `tokio::time::Instant::now()` → 走 `robot-core::time::RobotClock`。
- RT thread 内的 `Vec::push`、`Box::new`、`format!`、日志格式化、`Arc::clone/drop`、`FlatBufferBuilder` 构造、文件 / 网络 IO。
- `google.protobuf.Any` 作为 RPC payload → 用 `CommandMeta` + 强类型 Command。

## 构建假设

- **v0.1–v0.5 开发**：macOS 可跑前 5 步（纯类型 + 单元测试 + mock iceoryx2）。
- **第 6 步起需要 Linux**（`SCHED_FIFO` + `mlockall` + `sd_notify`）；Orin 或 Linux 开发机。
- **硬件接入（第 10 步）**：Orin + Feetech STS3215 + RealSense。

## 与上游 dimos 的关系

- 上游 dimos / dimos-deploy 是 Python 控制栈的只读移植参考；本地路径以实际 checkout 为准。
- `robot-system` 的目标是**完全替代 dimos 运行时**，保留 dimos 项目里已验证的 adapter 逻辑（Feetech 寄存器序列、运动学、校准）作为 `robot-adapters/feetech` 的 Rust 移植参考。
- v0.5 候选 E 已按原生替换控制执行面的路线跑通受监督 demo；旧栈自启已 disable、unit 保留回滚。
  不再执行早期“改造 dimos 为 RPC 客户端”的计划；正式退役待 D15 等生产安全硬化闭合。

## 提交规范（与全局 CLAUDE.md §4 一致）

- 分支：`master` 保护；`feat/*` `fix/*`。
- Conventional Commits + emoji 前置，不加 Co-Authored-By。
- Commit 前硬性：`cargo fmt --all && cargo clippy --all --all-targets -- -D warnings`。

## 现在不讨论的话题（架构已冻结）

`iceoryx2 vs eCAL` / `FlatBuffers vs Cap'n Proto` / `gRPC vs ZMQ` / `S0/S1/S2 重分层` / `Python 进 S1` / `SoftRT 挂 Pub` / 改 `layout_id` 宽度 / 改 `panic=abort` 范围。

要改走 `docs/CHANGELOG.md` + `docs/10_verification.md`，不改 `docs/00_design.md`。
