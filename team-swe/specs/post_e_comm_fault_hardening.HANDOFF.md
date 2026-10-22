# 交接 · post-E 通信故障硬化

## 当前步骤

### 仅准备零速单独停轮检查，尚未执行实机（2026-10-21本地；UTC10-22）

已对照从Orin复制的实际Python项目：`bus2_base.write_stop` 在原连接只发送SYNC零速；此前三轮试验随后立即断扭，不能据此证明保持使能时零速能独立停轮。仅在既有pulse入口增加可选`--zero-stop-check`，旧默认合同和scope不变；不接通生产Velocity或放行Phase12。

保持原固定+500/20秒动作后，正常发送一次SYNC0，以完成时刻为锚固定50/250/450ms读取三组TE/GV/PV，每组TE必须1、GV必须0，末组PV严格0。零速阶段从SYNC开始不超过500ms；每组跨下一槽即失败，避免迟到采样挤在末尾。原启用尝试至TE0的21秒预算不变，固定预留100ms正常收尾；任一失败仍执行原finally清零、独立断扭及既定停止门，失败粘性保持。只改两Python，未写Phase、PID、模式或EEPROM。

最终本机108方法107PASS、同版SDK黄金1项显式skip；Orin108/0skip包含实际SDK的三目标×默认/零速六子case，仍是一个黄金方法，不累计为额外测试。独审9方法是108子集；原500ms和21s预算、严格末组PV0、GO及失败粘性通过定向反例核对。现装nightly1a2c4797e下实际fmt→根Clippy通过，597项Rust输入、两Python及SDK12完整身份首尾一致。14份必要原件88499B逐SHA回收，同$109/%145/PID734447自然exit0，12直属step child消失，仅所属pane及临时runner已清理，原target保留。

早期断言误置造成的本地测试失败、独审发现的采样压缩内存反例及首次SSH观察255保留；只复查同一句柄，未重启批次，不宣称所有准备一次通过。正式仅准备right9一份PLAN，摘要e4c2913b…be7817，尚无GO或硬件输出目录。本阶段无UART、实机动作或现场事实；实际供电/落地状态未知，旧架空授权不复用。见[唯一摘要](../../.verify/base_zero_stop_2026-10-21_r1/summary.json)、[原生结果](../../.verify/base_zero_stop_2026-10-21_r1/software/result.json)及[独审](../../.verify/base_zero_stop_2026-10-21_r1/review.json)。下一步只在当次架空、供电、看护及可立即断电确认后，提前通知并执行一次右9零速保持使能验证；三轮固件合同、统一控制链、D15 OPEN/DEFERRED及deployable=false保持。

### 前轮、左轮单轮闭环通过，清场完成（2026-10-21本地；UTC10-22）

04:19:13UTC首次记录本次就绪答复，明确授权先前轮right8、通过后左轮right7，各一次原始+500/20秒，逐轮零速/断扭及停止检查。动作前分别通知，front467160d2…fa1eb、leftf7bf4c10…7e956两个独立GO各仅消费一次，第二轮没有续写现场时戳；未自动重试。每轮同一SDK连接实际400次非零SYNC、79组PV/PP，累计模位置前9921/左9929；启用尝试至TE0完成前20.019802554秒、左20.019716978秒。SDK初始及最终PV严格0；断扭后原PV前200→0、左300→0，均约276ms完成归零，原值保留。

两轮诊断、17电机停止、位置分类及106项身份均PASS，manual_power_cut_required=false。left7获准稳定性判据仅用于原前后snapshot新14读，没有移入活动阶段或推广给前8。用户分别明确报告前轮、左轮“明显持续转动，结束后完全停止”，与先前右9合起来，三只轮子在各自独立受监督试验中均完成“指令→实际动作→反馈→停止”。两份原run仍整体FAIL，唯一错误为原三项`BASE_FIRMWARE_CONTRACT_UNVERIFIED`/DEFERRED；不改写原件，不宣称整车三轮组合运动、固件/单位合同或统一`robotctl→RPC→S1→S0`链通过。

前$107/%143/PID732748、左$108/%144/PID733244均自然dead1/exit1；每轮310直属child和620原流真实回收、SDK连接关闭，两串口四锁无人占用，仅所属dead pane已清理。每轮638原件（共享一份现场确认）均逐SHA回收；run前d181da87…17c594a92、左6c9ec7e0…37ecb250fc。没有终态后UART、正式安装或服务操作。用户随后问能否放地，root已答复先关闭两电机电源、保持双臂支撑再平稳放地，USB不用拔；实际OFF和落地状态尚未收到，不推定完成，旧架空事实不能用于新动作。看护已结束，剩余仅记录及Git收尾。见[唯一摘要](../../.verify/base_wheel_sdk_targets_2026-10-21_r1/summary.json)、[前轮观察](../../.verify/base_wheel_sdk_targets_2026-10-21_r1/front-physical-observation.json)与[左轮观察](../../.verify/base_wheel_sdk_targets_2026-10-21_r1/left-physical-observation.json)。D15 OPEN/DEFERRED、deployable=false保持。

本阶段仅两Python固定目标选择：`--wheel-pulse front|left`，旧right9兼容；目标贯穿PLAN/GO/TX/RX/ACK/抓帧/位移豁免，不加自由速度、时长或ID。两脚本e4c538ab…adb9/bcd6a7c5…a986；本机100方法99PASS+1SDKskip，Orin100/0skip含现场SDK三目标黄金；独审6为100子集。实际nightly1a2c4797e的fmt→根Clippy通过（原target因工具链更新重编），597输入/两Python/SDK12首尾一致，未另建target；软件pane和临时runner已清理。root最初只读准备误读PLAN_ONLY外层拒绝，已仅修正解析，原失败保留，无UART/GO。仓库外网直连超时后，使用Orin到文档所示Gitea10.0.1.16:20080转发，保留原域名host key验证，双远程fetch恢复，remote URL未改；最终提交后按现行规则同步并回收fix分支。

### 右9受监督单轮动作/反馈/停止闭环通过，三轮和统一链仍未验收（2026-10-21本地；UTC10-22）

03:27:00UTC首次记录本次直接就绪答复，动作前明确通知仅right9、原始+500、20秒；随后核当前USB、服务/进程、两串口四锁及74引用，通过后新GO仅一次消费。同SDK连接实际发送400次+500、79组PV/PP，PV450/500/550，累计位置9950计数含多圈回绕；启用尝试至独立TE0完成20.019634902秒。零速/TE0后首PV250保留，下一组PV0，约275ms内停稳；TE/GV均0。此次支持“250可能出现在短暂停稳阶段”，不能抹掉原500ms停止失败或据此给右9放宽最终零速门。

右9诊断、17电机停止及106项身份均PASS，manual_power_cut_required=false。左7末次PV50按已获准固定1秒五组位置稳定性判据接受，其他电机严格零；不称全部51项值为零。用户直接报告“右轮明显持续转动，结束后完全停止”，本单轮受监督维护的“指令→实际动作→反馈→停止”闭环通过。原run结果仍FAIL，唯一错误为既定三轮`BASE_FIRMWARE_CONTRACT_UNVERIFIED`/三项DEFERRED；该run及旧原件不改写，不外推其余两轮、三轮运动学/固件/单位合同或`robotctl→RPC→S1→S0`统一链通过，也不宣称旧故障唯一根因已证实。

638原件2057805B逐SHA回收；run SHA2f6b0ef4…4b127c792，937条SDK原帧保留。同$104/%140/PID725892自然dead1/exit1、310直属child真实结束且PID消失；SDK连接已关闭，两串口四锁无人占用，仅所属dead pane已清理，无终态后UART或自动重试。已通知本轮动作/清场完成并解除看护，继续保持机械臂/架空底盘支撑；未收到实际断电确认，不推定OFF。见[唯一摘要](../../.verify/right_wheel_20s_2026-10-21_r1/summary.json)、[现场观察](../../.verify/right_wheel_20s_2026-10-21_r1/physical-observation.json)及[原件清单](../../.verify/right_wheel_20s_2026-10-21_r1/hardware-collection.json)。下一步复用同路径准备其余两轮的最小验证，任何新动作先通知并核当次现场状态；本次GO不覆盖后续。D15 OPEN/DEFERRED、deployable=false保持。

### 右9同连接SDK20秒候选软件准备完成，未执行实机（2026-10-21本地；UTC10-22）

按真正官方常驻连接及循环发速路径，只在既有维护入口复用Orin已装`feetech-servo-sdk 1.0.0`/`pyserial 3.5`，基础Python `-B -I -S`隔离导入固定12项源码/元数据。仅right9同一串口连接、固定+500原始速度、名义50ms循环发送20秒；跳过错失周期，不延长绝对截止时间。未调用官方整机configure，未写模式、Phase、PID或EEPROM。原17电机前置、维护独占、GO及失败粘性保持；SDK活动时不允许CLI碰串口，finally独立零速/TE0，随后最多1秒只读停稳，最终TE/GV/PV必须严格0才继续原整机停止/身份检查。原左7获准稳定性判据保留，右9不接受±50豁免；20/21秒为软件预算，不是硬件断电保证。

最终主脚本72e42872…3217a67、测试d2209129…18cc8；本机和Orin相同95项、0skip，含实际已装SDK内存串口黄金测试。独审额外核对真实SDK对畸形ACK误报成功的负控、实际TX处截止时间复查等，3方法为95子集不累加。Orin实际fmt→根Clippy通过，597项Rust输入976cbae7…80a0e2、两Python及SDK12完整身份c61efd2c…eb3044首尾一致，实际nightly07e526db9。唯一native批次$103/%139/PID725474自然exit0、step child已回收；14必要原件逐SHA回收，仅所属pane和临时runner已清理。最初SSH255及启动前引用/长度拒绝保留，不能称所有准备一次通过。

74引用、当前USB/服务/进程/串口四锁、基础隔离SDK导入及固定PLAN 5e5d167f…15edb5已通过非UART准备。无新GO、UART、实机20秒或正式安装；旧逐命令20秒草稿保留stash 7a34b74d…199a33，不合入未验证代码。下一次启用前先通知用户目标轮、原始速度和20秒时长，准备完成后确认当次供电/架空/支撑/持续观察及可立即断电；旧现场确认和消费GO不复用。上一轮NEEDS_MANUAL_POWER_CUT、未获断电确认及用户“轮子没有动”均保持。见[唯一摘要](../../.verify/right_wheel_20s_2026-10-21_r1/summary.json)、[原生结果](../../.verify/right_wheel_20s_2026-10-21_r1/software/result.json)及[独审](../../.verify/right_wheel_20s_2026-10-21_r1/review.json)。D15 OPEN/DEFERRED、deployable=false保持。

### 左轮新判据实机通过，右9停止失败且现场报告轮子未动（2026-10-21本地；UTC10-22）

02:34:49UTC记录新的现场供电、三轮稳定架空、双臂空载支撑和持续看护确认，授权先左14读、通过后一次右9。左14读TE/GV均0，PP1211/1212/1211/1211/1212，PV50/50/0/50/0，1073.233167ms；候选和93项身份PASS，整机STOP仍NOT_EVALUATED。仅使用当次新GO，没有续写旧确认时刻。

随后新右GO执行一次原500ms候选：5条原帧严格SYNC0→TE1→SYNC500→SYNC0→TE0；右9 PP从3329到3333/3482，PV50/500。独立TE0后60.357593ms开始读取PV，原字250，后续UART已粘性封锁；原NEEDS_MANUAL_POWER_CUT、STOP UNKNOWN、模式身份UNKNOWN不回填。收到断电请求后用户明确“不断电，再跑一下轮子，20s”，其后又确认“轮子没有动”；记录未断电及现场未见轮动，不能以编码器变化宣布实机成功。用户要求“下次动之前通知我”，后续动作前必须先告知目标轮、速度和持续时间。

两个原run SHA分别1c58a2a2…2e75f360和4f6ac3f4…62b3aef7；608份原件1020371B逐SHA回收，292直属child和584原流核对通过。同$101/%137/PID724165自然exit0、$102/%138/PID724339自然exit1；仅所属dead pane已清理、PID消失、两串口四锁无人占用，无后续UART。见[唯一摘要](../../.verify/left_wheel_stop_stability_2026-10-20_r1/summary.json)、[原件独审](../../.verify/left_wheel_stop_stability_2026-10-20_r1/hardware-review.json)及[现场观察](../../.verify/left_wheel_stop_stability_2026-10-20_r1/physical-observation.json)。

对真正[官方键盘入口](https://github.com/Vector-Wangel/XLeRobot/blob/b017b5e6354bd9f61f4247a920c72622ca0aade0/software/examples/4_xlerobot_teleop_keyboard.py)核对后，必须更正“与官方相同”的表述：官方一次connect并循环向三轮发速度，configure还配置电机并全总线使能；此前SYNC0→50ms→Enable来自dimos，不能说完全复现官方。此前500ms候选为逐命令新连接、右9一次写入；官方stop_base只写零，没有实际PV停止门。差异尚不能证明未动根因。20秒逐命令候选已完成本机95项、未Orin/未GO/未硬件，现暂停该草稿，先核安装SDK和官方初始化，选“同一连接、持续发送”的最小单轮对照。D15 OPEN/DEFERRED、deployable=false保持。

### 用户批准左轮right7稳定性判据，最小实现及软件验证通过（2026-10-20）

用户直接同意仅right7实施并验证：TE/GV严格0，PV仅0/±50，固定1秒五组PP/PV且位置跨度≤1计数；缺失、超时或超界仍失败。[授权原文](../../.verify/left_wheel_stop_stability_2026-10-20_r1/approval.json)记录原问题与答复来源。只调整既有普通读取即时门、前置快照及最终停止分类，原PV保留；前后快照必须各自产生新窗口证据，不借旧run或status布尔值放行。复用b2、维护四锁和GO，不新增CLI/驱动/生产链。现有左轮只读选择升级为14读，补齐窗口前后GV0；独立采集仅判左轮候选，整机STOP仍单独验证。

本机及Orin同88项测试通过，11个新增方法为其子集，两端不累加。最终主脚本SHA为4b822e7e…1276f624，测试0633803c…c9410137；Orin latest nightly07e526db9下实际fmt→根Clippy通过，597项Rust输入和两Python首尾一致。独审确认默认和右9路径均在窗口失败后封锁UART，其他电机严格零值；首审发现的日志关闭/报告保存失败后candidate仍PASS已修，两故障负控通过。原87项软件结果保留，旧新源码仅保留可逆差异补丁，不复制整份源。见[唯一摘要](../../.verify/left_wheel_stop_stability_2026-10-20_r1/summary.json)、[最终原生结果](../../.verify/left_wheel_stop_stability_2026-10-20_r1/software/final-validation/result.json)及[窄审](../../.verify/left_wheel_stop_stability_2026-10-20_r1/review.json)。

14:17:17UTC已核62引用、当前USB、服务停止、进程及两串口四锁无人占用；所属软件pane均已清理，复用原target，没有新发布构建。最终两份未执行PLAN已重绑定，左轮9b5df681…caa96a6、右9为2e15e4c8…42d29c4；旧计划从未下发GO或创建硬件输出，修订摘要保留。尚无新UART或新实机结论；用户本次回复是判据变更授权，不是新的供电、支撑和看护确认。下一步仅在当次现场确认后先验证左轮14读，通过才按另一个有界GO复用原右9单次诊断，任一失败/未知不追加UART。其余电机、原右9写顺序、失败粘性及历史FAIL保持，D15 OPEN/DEFERRED、deployable=false不变。

### 左轮只读采到-50反馈，位置仅1计数跳变，现场未见转动（2026-10-20）

11:20:33UTC收到当次供电、架空支撑及左轮全程观察确认，在600秒时效内仅执行一次固定12读。首末TE均0；五组PP为1211/1212/1212/1211/1212，PV为0/0/0/0/-50，末次速度原字8032及原帧保留。整批1056.519169ms，在原1.5秒软件预算内。只有12次SYNC_READ，没有写参数、Enable或动作；PP/PV先后采样，不称同帧。用户随后明确报告“全程看清，左轮没有转动或抖动”。这支持静止附近量化/抖动假设，不能证明精确静止、把±50直接判零或回填此前普通READ故障。

原结果为DIAGNOSTIC_CAPTURED，STOP为NOT_EVALUATED_DIAGNOSTIC_ONLY，93项非UART身份检查通过。72直属child回收；11:21:43UTC观察同`$97/%133/PID716357`自然dead1/exit0，所属PID消失、两串口四锁无占用，仅本次dead pane已清理。154份原件278355字节逐SHA回收，原run SHA为c9fd8651…9e7687a。见[唯一摘要](../../.verify/left_wheel_feedback_2026-10-20_r1/summary.json)、[原帧独审](../../.verify/left_wheel_feedback_2026-10-20_r1/hardware-review.json)、[原始run](../../.verify/left_wheel_feedback_2026-10-20_r1/hardware/run.json)及[现场观察](../../.verify/left_wheel_feedback_2026-10-20_r1/physical-observation.json)。GO已消费、看护收尾，无后续UART或自动重试；电源最新仅用户本轮报告ON。

下一步先核对现有轮子停止判据与厂家反馈精度，避免重复随机试验。当前未修改停止门、Phase、模式、PID或EEPROM；右9新顺序尚未执行，三轮动作及固件合同仍未闭环，D15 OPEN/DEFERRED、deployable=false保持。

待用户决策的最小候选仅针对right7：TE/GV仍须严格为0；PV仅允许0或±50时，补一次固定1秒五组PP/PV，要求位置跨度不超过1计数，未知、超期或超界继续失败。原始PV保留，不加全局死区；其他电机及历史结果不变。这是需实现、软件负控和实机另行验证的候选，当前未落地或判安全。需一致处理普通READ即时门、`validate_snapshot`及`classify_stop_snapshot`，不能只改报表。厂家0.0146RPM与0.732RPM的比例约50，但原表精度说明位于目标速度，未承诺当前速度±50必为噪声；本次也未读GV。项目`CLAUDE.md`第28行明确“不修改冻结架构、安全判据或历史验收结论”，因此必须取得用户对这项判据变更的明确同意后继续实现，不从本次只读授权推定同意。

本次结果提交前Orin同源597项、nightly07e526db9下实际fmt→根Clippy通过，两Python冻结文件未变，见[提交质量](../../.verify/left_wheel_feedback_2026-10-20_r1/commit-quality/result.json)。未重跑测试、构建产物或执行后续UART。

### 左轮位置/速度短时只读准备（2026-10-20，以下为实机执行前记录）

为区分持续位移与静止附近反馈跳变，仅在现有维护入口增加互斥`--left-wheel-feedback-only`。固定right7首TE0、0/250/500/750/1000ms五组PP后PV、末TE0，共12次读取；复用b2冻结工具，不写寄存器、不自动重试。整批1.5秒是软件预算而非硬截止，实际起止及原帧保留；TE非零、通信、GO或采集超期后停止后续UART。非零PV只作观测，成功结果为`DIAGNOSTIC_CAPTURED`，STOP明确为`NOT_EVALUATED_DIAGNOSTIC_ONLY`，不能放行右9动作。此处SYNC_READ与R2普通READ不同，不以未复现或采集成功回填原故障。

复用原四锁、当次600秒现场事实、主机/工具/USB身份及非UART收尾；原位置/右9动作入口与STOP51保持。两端同77项脚本测试通过，新增8项属于子集；独审另核错误原帧和超窗后无后续读取。Orin最新nightly07e526db9下实际fmt→根Clippy通过，597项Rust输入仍同976cbae7，两个Python文件另核同源。主脚本fc5cc4f9、测试75a6eb41；正式PLAN为225b8511，区别于软件演练PLAN3d613bcd。62引用、保护文件、当前USB及两串口四锁无占用已核，软件所属`%132/PID715864`自然退出后已清理，复用原target。

见[唯一摘要](../../.verify/left_wheel_feedback_2026-10-20_r1/summary.json)、[原生软件日志](../../.verify/left_wheel_feedback_2026-10-20_r1/software/result.json)及[窄审](../../.verify/left_wheel_feedback_2026-10-20_r1/review.json)。目前无本轮GO、硬件输出或UART；供电状态仍为R2用户报告已断开。本次只读仍需当前恢复供电、架空支撑与持续看护确认，准备完成后再询问；不复用已消费的右9授权。正式安装与服务未改，D15 OPEN/DEFERRED、deployable=false保持。

### 新顺序在初始左轮读取阶段停止，断电收尾已确认（2026-10-20）

10:38:49UTC收到当次恢复供电、三轮架空、支撑、持续观察及立即断电确认，在原600秒时效内仅启动一次。seq130为左轮right7普通READ地址58、宽2；请求`ff ff 07 04 02 3a 02 b6`，响应`ff ff 07 04 00 32 00 c2`，ID、长度、错误码和校验正确，原字0032解码为+50，与stdout一致。此前独立读取Mode1、TE0、PP1212，不能凭单个位置样本判断运动。原Python解码也返回+50，未发现符号或字节序差异。

全轮88次普通READ，没有WRITE、SYNC_WRITE、启用或断扭命令；`probes=[]`，右9新顺序NOT_RUN。seq130后仅18项非UART收尾，148直属child及296原流全部回收。原STOP51全部UNKNOWN，模式身份UNKNOWN，另93项非UART身份检查通过；这些后验不能覆盖原`NEEDS_MANUAL_POWER_CUT`。10:40:10UTC已观察同`$94/%130/PID715088`自然dead1/exit1；10:41:00UTC收到“双电源已切断、机械臂和底盘仍有稳定支撑”，记OFF_USER_REPORTED，实际电气断电时刻未知。两串口四锁无占用，所属dead pane已清理，没有后续UART或重试。

306份原件450294字节已逐SHA核对，原run SHA保持`32ecc765…377288`。见[本轮摘要](../../.verify/right_wheel_reference_2026-10-20_r2/summary.json)、[原帧独审](../../.verify/right_wheel_reference_2026-10-20_r2/hardware-review.json)和[断电回执](../../.verify/right_wheel_reference_2026-10-20_r2/power-cut-confirmation.json)。本次只补结果记录；Orin同源597项、nightly07e526db9下实际fmt→根Clippy通过，未重跑测试或发布构建，见[提交质量](../../.verify/right_wheel_reference_2026-10-20_r2/commit-quality/result.json)。

对照原版后确认：dimos将PV直接换算为轮速和里程计，通信重试不对非零数值消抖；`write_stop()`仅发送零目标并将缓存速度清零，XLeRobot的`stop_base()`也没有PV停止读回。原版没有可直接移植的“±50即静止”判据。厂家细单位/精度之比约50只支持量化假设，不能证明本轮静止。下一步仅准备right7位置/速度原帧短时序列并同步观察左轮，区分持续位移与静止附近反馈跳变；不重复静态三读、不放宽STOP51、不自动重试右9。新采集须另有当次供电和看护确认；本轮断电回执不授权恢复供电。D15 OPEN/DEFERRED、deployable=false保持。

### 对照原版调整右9单次诊断（2026-10-20，以下为实机执行前准备）

先核对了[XLeRobot固定源码](https://github.com/Vector-Wangel/XLeRobot/blob/b017b5e6354bd9f61f4247a920c72622ca0aade0/software/src/robots/xlerobot/xlerobot.py#L618-L644)和Orin实际安装的`xlerobot-dimos 0.1.0`。后者的`bus2_base.py`与`common/feetech_io/bus.py`已同本地dimos提交`9ece6bd0e1ffda95248820c1e54b23541747cee0`逐文件核SHA：实际路径是SYNC零速、等待50ms、启用，再以GroupSyncWrite发送非零速度；地址46、宽度2字节。官方插件未锁定LeRobot版本，不能把上游当前依赖当成现场安装；已核对的LeRobot 0.6.1/SDK 1.0.0路径也没有隐含TE1写入。

上一轮在断扭时预置GV500后提前失败，既没有执行计划Enable，也没有完成运动观察；GV500到TE0请求只有45.613ms。现仅调整既有维护入口和`feetech_poke`例子：新增受参数检查的单ID `sync-velocity`，右9固定执行“SYNC0→至少50ms→确认GV0→TE1及读回→SYNC500及读回→原100/400ms速度与位置观察→SYNC0→TE0”。500ms仍从TE1尝试前起算，TE0是最后一条动作写；任何不确定动作保留一次独立TE0，初始零失败不重发，收尾零失败在TE0后封锁UART。原STOP51、身份后验、失败粘性及600秒现场时效不变，仅补右9普通TE的同次strace。

这次只回答原版顺序下右轮能否动作并完成停止读回，不修改Phase、模式、PID或EEPROM，不接三轮生产控制链。Python原版复用连接，当前维护诊断仍复用原有逐命令工具，这个差异明确保留，未另建连接框架。TE与GV变化的具体原因、旧静止±50仍未证实；单轮通过也不覆盖固件合同、D15或完整底盘验收。当前电源状态仍为上轮OFF_USER_REPORTED，无新UART；完成全部准备后再确认当次现场状态。软件验证与下一次执行只在[本轮入口](../../.verify/right_wheel_reference_2026-10-20_r2/summary.json)记结果。

Orin实际fmt→根Clippy、real-hw例子Clippy、18项例子测试、release及坏参数拒绝均通过，nightly `07e526db9`和597项输入聚合`976cbae7…80a0e2`首尾一致。冻结ELF为1517688字节、`22693d15…1820b0`，已绑定最终Python；两端同69项维护测试通过，不重复累计。默认/完整PLAN、互斥及缺GO拒绝通过，PLAN固定`bbeed8f5…4e8322`；root核对62引用、保护文件、当前USB及两串口四锁无占用。所属软件pane `%128/%129`确认自然退出后已清理，复用原target，无正式安装、服务变更或硬件I/O。现仅待当前供电、架空支撑及持续观察确认；历史确认和GO不续用。

### 右9非零诊断在预置后失败，人工断电及两轮未动已确认（2026-10-20）

09:14:13UTC记录当次供电、三轮架空、支撑、持续观察及立即断电确认，仅启动一次已准备的右9诊断。型号777、FW3.10、Mode1、Phase12、Lock1及初始TE/PV/GV零均读回；seq173普通WRITE向地址46写500并获有效ACK，seq174原帧确认GV500，seq175原帧却读到TE1。脚本报“预置后不是断扭”，计划中的显式Enable及500ms内两对PV/PP采样均未执行，不能称为完成一次非零转动。

本轮实际写帧仅右9 GV500、TE0、SYNC GV0三次，没有TE1写。f201对应历史b01603b源码的连接只做PING/型号READ，指定写入口未发现隐含启用。seq176独立TE0获ACK，seq177/178原帧为TE0/PV0；随后seq179 SYNC清目标，seq180原帧确认GV0。然而最终seq278普通READ输出`Torque_Enable@9 = 1`，其后UART粘性封锁。该末次TE只有原stdout、没有同次strace原帧；两次TE1与写目标速度的时间关联不能直接认定为固件自动启用。其余16电机48停止值零，右9的后续PV/GV未读取，原STOP UNKNOWN及`NEEDS_MANUAL_POWER_CUT`保持。

09:15:25UTC已观察同`$89/%125/PID710022`自然dead1/exit1，296直属child结束，93项身份后验通过。608份原件共795442字节已回收逐SHA核对；所属PID消失、两串口四锁无人占用，仅所属dead pane已清理。09:16:48UTC收到用户“双电源已切断，机械臂和底盘仍有稳定支撑”的回执，之后用户报告“两轮都没动”，仅记本次右轮/前轮的现场观察。当前OFF_USER_REPORTED，实际电气断电时刻无遥测；不覆盖原失败，也不回填此前轮动UNKNOWN。原终态收集文件内当时的断电PENDING保留，以后续独立回执补充。

见[本轮摘要](../../.verify/right_wheel_pulse_2026-10-20_r1/summary.json)、[原始run](../../.verify/right_wheel_pulse_2026-10-20_r1/hardware/run.json)及[现场观察](../../.verify/right_wheel_pulse_2026-10-20_r1/physical-observation.json)。本次收尾后无新UART或重试，未改驱动或参数。下一步只核实FW3.10/Phase12下GV写与TE反馈变化的关系，相关读取保留实际收发；不增加速度、不放宽预置或停止条件、不扩展三轮控制。D15 OPEN/DEFERRED、deployable=false保持。

独立复核及root均核对608份原件，原run SHA仍为69e08d25…17108b2，原帧和普通READ输出的边界见[硬件证据复核](../../.verify/right_wheel_pulse_2026-10-20_r1/hardware-review.json)。本次仅补诊断记录；09:23:03UTC Orin实际fmt→根Clippy通过，597项源ae00cf4f及nightly07e526db9前后不变，两份Python另核未变，见[提交质量](../../.verify/right_wheel_pulse_2026-10-20_r1/commit-quality/result.json)。未重跑测试或新建发布产物，软件检查不覆盖本轮停止失败。

### 右9单次非零诊断的软件及执行准备（2026-10-20，以下为实机执行前记录）

按用户要求使动作可见，只在既有 `run_hardware_maintenance.py` 增加固定 `--right-wheel-pulse` 选择，与位置选择互斥，默认路径不变。复用f201普通写、b2原帧读取/SYNC清零和原raw helper/strace；无新ELF、连接框架、服务或生产Velocity接入。右9型号777/FW3.10/Mode1/Phase12/Lock1需当次匹配，断扭时预置+500raw并读回，启用尝试起100/400ms各取PV与PP，两对必须在500ms内完成，采完即断扭；500ms不是精确运行时长，1秒失败门也不是硬件截止保证。运动中反馈限于正向0–1000raw及单圈差值0–512，需观察到正向速度和位置变化；该判据与停止零值门分开，不把旧静止±50改判正常。

收尾先独立TE0并确认TE/PV零，再SYNC清目标并读回；随后仍是原STOP51及身份后验。停止未知封锁后续UART，信号、超期、读写/抓帧错误保留失败，已有动作尝试仍独立TE0。四写保存同次strace，不重发；原帧先保存再验证返回时GO期限。首轮独审发现的缺写原帧和末次PP跨GO仍PASS两项已修，[独审记录](../../.verify/right_wheel_pulse_2026-10-20_r1/review.json)保留复现及修复证据。

本机及Orin同62项测试通过，不累加重复方法。Orin实际默认PLAN/完整PLAN退出0，互斥及缺GO预期退出2；随后fmt→根Clippy零警告。nightly07e526db9、597项Rust/配置ae00cf4f前后不变；两份Python单独绑定为b32961374bba0b1fc910983202171d50157b2192f7493700b95efc98ebd62ff3 / fed9f7fe25fb8864da34c79733b3a9b18351e94c56e19110bb451067660d9aa5。所属软件pane/PID正常退出并清理，必要日志已回收核SHA。root另核61引用、保护文件、当前USB及两串口四锁无人占用，无UART。固定PLAN为450df179a1e2be70e384810fe72e73661f89e9efea7cddd6c887ac53af249c2b，见[本轮入口](../../.verify/right_wheel_pulse_2026-10-20_r1/summary.json)。

尚未生成本次GO或hardware输出、未执行非零动作；原现场确认已超600秒，准备完成后才核当前供电、架空、支撑和持续观察。原三轮固件合同仍DEFERRED，D15 OPEN/DEFERRED、deployable=false，不以局部诊断代替完整底盘验收。

### 右轮零速R4收尾通过，现场确认两轮未动，转向一次非零诊断（2026-10-20）

08:46:16UTC收到当次供电、架空、双臂支撑、持续观察和立即断电确认，原600秒时效保持。08:47:11UTC仅启动一次原R4；08:47:49UTC观察同`$86/%122/PID708705`自然dead1/exit1。独立原件核对仅右9 SYNC Goal0、TE1、TE0三写，SYNC结束至Enable尝试540.762082ms；三次PV0000在11.650625/211.765737/411.840880ms完成。STOP51终读全零、93项身份后验通过，337直属child和674原流一致；原整体FAIL仍是固件合同DEFERRED，人工断电标志false。所属PID消失、两串口四锁无占用，仅本次dead pane已清理。

692份原件942266字节已回收逐SHA核对，见[本轮摘要](../../.verify/base_zero_sync_diagnostic_2026-10-20_r4/summary.json)及[独立复核](../../.verify/base_zero_sync_diagnostic_2026-10-20_r4/review.json)。用户对本次全程观察回答“都没动”，仅记录右轮/前轮R4未见运动，不回填R3或历史±50。用户随后要求动作明显；刚才命令为零速，因此本轮不能验证非零转动。停止与清场已完成，未为此重做零速试验。

下一步只补现有维护入口的一次右9非零原始值诊断，保留参数、时窗及运动/停止判据分离。旧未调用pulse的raw20/150ms若采用厂家细单位，仅对应约0.263°；其PV≤40又小于厂家约50raw精度，是明确的软件试验参数疑点。量化只是假设，不能据此认定旧静止±50正常。Phase12/FW3.10的完整固件合同仍未闭环，不写Phase/模式/EEPROM，不开放三轮控制链。准备完成前不要求新现场确认，不续期旧GO；D15 OPEN/DEFERRED、deployable=false保持。

R4收尾前实际Orin fmt→根Clippy通过，nightly07e526db9及597源ae00cf4f前后一致；新增非零诊断若改源须另验证，不复用这份检查冒充新源码通过。

### 右轮零速诊断与停止子项通过，原件已回收，实际轮动未确认（2026-10-19，10-20续收尾）

收到06:12:30UTC当次架空、支撑、持续看护及观察右轮/前轮的确认后，复用原候选3b681657、原helper和b2工具执行一次R3，没有新增工具、采样或参数写。断网前读取的原run显示：右9启用窗口三次PV原字均0，断扭及17电机51项零值停止读回通过，身份后验通过，manual_power_cut_required=false、child_active=false。06:13:25UTC已观察同一`$84/%120`、PID698162自然dead1/exit1；原run整体FAIL仅因保留的`BASE_FIRMWARE_CONTRACT_UNVERIFIED`，不改写为整轮PASS。

用户对实际轮动观察答“再试一下”，该回答授权下一次相同试验，但上轮轮动仍为UNKNOWN。本次未复现±50，不能解释其来源或宣称三轮实际运动闭环。其后家庭网关`100.64.0.1`离线，既有SSH路径返回SOCKS error 1；用户报告刚才碰到电源并要求重测网络。重复试验尚未启动，无后续UART。

10月20日网络恢复，691份原件共941208字节已回收到本地并逐SHA核对。独立窄审确认仅右9三写，SYNC发送结束至Enable尝试540.900581ms；三次PV均0000，分别在19.831947/219.869727/411.662627ms完成，均在原500ms采样期限内。该期限不代表总启用时长；原观察函数收尾记时500.621881ms保持。7份普通三轮PV原帧均零，终读stdout独立重建STOP51全零，93项身份后验通过；337直属child均回收，674原流哈希及journal一致。再次核实同一已退出pane、所属PID消失、两串口及四锁无占用，清理仅该dead pane，无新UART。

见[当前摘要](../../.verify/base_zero_sync_diagnostic_2026-10-19_r3/summary.json)及[原帧复核](../../.verify/base_zero_sync_diagnostic_2026-10-19_r3/review.json)。10月19日fmt/根Clippy因连接失败未启动的NOT_RUN保留；10月20日Orin更新nightly至07e526db9后实际执行fmt→根Clippy，均退出0、零警告，597项源码在本机及原生前后同ae00cf4f，编译器前后一致。必要日志在本轮precommit-quality/native-2026-10-20，无Rust修改或新增测试。R4复用同一候选，仅改变输出与产物根目录；原61引用/保护及两USB身份均匹配，尚未生成现场事实或GO、未访问UART。上次现场确认已过期，已请求本次供电、架空、支撑、持续观察和看护状态，收到后只执行一次原试验；不改写旧时间戳。D15 OPEN/DEFERRED、deployable=false保持。

### 前轮三项只读实机通过，旧非零速度原因仍未确定（2026-10-19）

用户明确确认本次已供电、三轮稳定架空、双臂空载有支撑、区域空旷并可持续看护及立即断电；05:54:29 UTC记录收到回执。复用原候选、b2冻结工具和维护门，在tmux中仅执行一次right8使能原帧→速度原帧→普通速度读取，05:55:16–19 UTC完成。TE为0，两种PV的完整响应均为`ff ff 08 04 00 00 00 f3`，原字0000；三次各自的PING和型号读取亦通过，型号777，九发九收均ID8/error0/长度与校验正确。没有WRITE、SYNC_WRITE、Enable、参数写入或动作。

三份原始strace中的工具均退出0，driver 697073对应`$82/%118`自然dead/exit0；99直属child真实回收，198原流SHA及字节数匹配，六项非UART后验通过。进程退出后确认两串口和四锁无占用，清理仅本次已退出pane；209份采集原件共377775字节已按SHA核对回收，未重复打包。见[统一摘要新增实机记录](../../.verify/base_front_pv_compare_2026-10-13_r1/summary.json)及[原始run](../../.verify/base_front_pv_compare_2026-10-13_r1/hardware-2026-10-19/base-front-pv-compare-r1/run.json)。两种PV在不同时刻读取，本轮0000仅说明此次未复现旧±50，不能解释其来源、证明物理轮动或代替17电机停止验收。原失败结论不回填，轮速固件/零速/停止合同仍未闭环；下一步围绕启用前后异常条件收敛，不重复静态三读或扩展控制框架。D15 OPEN/DEFERRED、deployable=false保持。本轮采集已收尾，没有后续UART。

提交前复用Orin同源597输入与既有缓存，实际按顺序执行`cargo fmt --all`和`cargo clippy --all --all-targets -- -D warnings`，均退出0；nightly b6eeb93c9前后相同，源码聚合ae00cf4f未变。五份必要质量日志随本轮原件保存，没有重建发布产物或新增实机读取。

### 按授权精简四组归档，保留必要原件（2026-10-19）

删除旧会话备份和 SYNC 零速软件采集的重复源码包、内嵌源码启动脚本及重复回传封装。597项源码均逐字节匹配固定提交 `1045301ab53e91f2e133c2217351f297841068d9`；原生包68项与展开副本一致。`child.log` 只移除重复 `archive_base64` 载荷，其余字段完整保存在 `child.compact.json`；独有日志和仍被实机候选引用的冻结 `feetech_poke` 未变。原路径、SHA及去向见[清理清单](../../.verify/xlerobot_sync_zero_2026-10-13_software/native/cleanup_2026-10-19.json)。

H2软件包由87.3缩至7.0 MiB，仅移除三个可重建软件 mock ELF、严格重复内容及Git已有源码；编译器/构建配方/产物身份和历史差异源码保留，两个同大小但不同内容的报告均保留，详[查找说明](../../.verify/h2_trace_owner_2026-09-30/CLEANUP.md)。可视化包由86.1缩至18.9 MiB，七轮源码均可恢复，只移除与外置样例或其他成员逐字节相同的录像、截图及重复封装，全部独有日志和不同场景录像保留，详[清理清单](../../.verify/robot_viz_2026-09-29/cleanup_2026-10-19.json)。原摘要、原验收结论及其中当时的包SHA不回填；新清单解释旧包路径，不能将新包SHA用于宣称原包未变。

本轮提交前在Orin同源597输入上，最新nightly检查及实际 `cargo fmt --all` → `cargo clippy --all --all-targets -- -D warnings` 均通过，输入前后保持ae00cf4f；没有改Rust源码或重做硬件验收。以普通提交精简当前版本，历史提交保持可追溯，Git历史体积不因当前删除立即缩小。未完成工作树草稿及本轮范围外的实机证据不动；D15 OPEN/DEFERRED、deployable=false及硬件未闭环项不变。

### 分支收尾规则与历史大包移除（2026-10-18）

用户要求开发完成后仅保留 `master`、`dev`，并同步 `origin`、`github`。本次将已完成的源码和诊断记录收口，未完成工作树草稿保留。用户另明确要求删除 `.verify/post_e_comm_fault_hardening/H_evidence.tar.gz`，包括阻挡 GitHub 同步的历史版本；该路径已加入忽略规则。`H.json` 和下文验收结论作为历史记录保留，原文中的大包链接不再可用，不再声称可从该包复核旧原件。其他源码、诊断原件和硬件未通过事项不因 Git 整理改变；前轮三读仍未实际执行。

提交前本机实际 fmt 通过，根 Clippy 因已有 Linux 专用 API 在 macOS 不可用而失败；未扩大范围修改跨平台代码。Orin 使用最新 nightly b6eeb93c9，按顺序实际执行 `cargo fmt --all` 和 `cargo clippy --all --all-targets -- -D warnings`，均退出0，597项构建输入前后同 ae00cf4f，未改 Rust 源码。本次提交统一收录下述10/13–10/16记录，其中“未提交”为当时状态；没有新硬件测试。

### 前轮三读候选原生软件和运行准备通过，等待本次现场确认（2026-10-16）

下一实际阻塞仍为前轮 right8 的非零速度反馈。复用原 d6b74da1 候选及 b2 冻结工具，只准备 TE 原帧→PV 原帧→普通 PV 三读，原非零/未知粘性封锁保持。Orin 在 tmux 内完成同三个软件方法、默认 CLI exit0、未绑定执行预期 exit2；输入前后相同，runner 自然退出0、PID消失，20份输出已取回并逐SHA核实。与旧本机三项为同组，不累计为六项；这不是实机验证。

现有主机、双USB枚举、保护文件及旧引用检查通过；新增入口只绑定当前 b2 完整 stat，候选源码未改。现成配置的 PLAN exit0，缺现场/GO 时执行请求在启动前 exit2，硬件输出目录尚不存在。见[统一摘要](../../.verify/base_front_pv_compare_2026-10-13_r1/summary.json)及其原始结果引用。已询问本次供电、三轮稳定架空、机械臂支撑和持续看护；当前回执与GO为空，UART0，不复用昨日左1授权。收到确认后只绑定已有入口执行一次三读，不追加其他电机或动作。本轮未改Rust、未运行Cargo、未安装或操作服务，记录未提交；旧超时/±50原因、D15 OPEN/DEFERRED及deployable=false保持。

### 左1握手及模式/使能/速度只读通过，旧超时原因仍未证实（2026-10-15）

按用户要求先完成现有 XLeRobot 最小闭环，暂不为 RP1 扩展设计。R2 失败工具已核为旧 f201；`connect(true)` 中 PING 或型号 READ 的接收超时会折叠为“无响应”，尚未进入 Operating_Mode 读取，现有日志不能判断是否收过部分字节。

用户当次确认已供电、机械臂和底盘支撑稳定、有人持续看护且可立即断电；13:09:57 UTC 记录确认，13:10:34–35 UTC 在 tmux 中以 sudo 执行已准备的单次 f201 `left1 ping`。当前 USB 身份、服务停止、目标串口无占用及四锁前置通过；只发 PING 和地址3/2字节型号 READ，收到两份 ID1、error0、校验正确的回复，型号777。工具及执行壳均退出0，stderr为空；原帧显示串口解除独占并关闭，非 UART 后验确认目标串口/四锁无人占用、所属两个 PID 已消失。

用户随后明确继续仅查左1模式、使能和速度，并确认供电/支撑/持续看护保持；13:19:36 UTC记录本次确认。13:20:53–54 UTC在tmux内复用原工具及维护前置，按顺序完成三项READ：Operating_Mode=0（位置）、Torque_Enable=0、Present_Velocity=0（原字0000）。每项仅PING、型号READ、指定寄存器READ，合计九发九收，ID1/error0/地址/宽度/校验均正确；三个工具及壳退出0、stderr为空。串口正常关闭，非UART后验确认目标串口/四锁无人占用、所属PID已退出；没有追加其他电机、参数写或动作。

两次采集及本次20份原件的两端SHA核对统一见[本轮摘要](../../.verify/left1_communication_2026-10-15/summary.json)，root及独立原帧复核通过。本次指定只读范围已完成，未构建Rust或改正式服务。结论为本次左1通信与三项状态读回通过，旧故障未复现且根因未证实，不外推长期稳定/整机停止。回到既定最小闭环时只选下一实际阻塞，不扩展左1扫描；D15 OPEN/DEFERRED、deployable=false 保持。

### R2重复入口已执行，左1握手失败，右轮未启用；人工收尾已确认（2026-10-13）

用户要求“再来一次，没看清”和“立即执行”。首09:39:27现场确认后的fresh17/23、当前USB无变化、58refs及唯一完整PLAN通过；root准备启动材料耗过原09:49:27期限，旧本地GO未上传或启动，原件保留。用户再次直接确认“现在仍全部就绪，立即执行”，root固定09:50:54至10:00:54新现场事实及独立GO，只启动一次driver669897/@112/%112；复用原main3b/helper70/b2，没有新Rust构建、软件重验或控制逻辑改动。

实际首UART工具调用seq43为左1 `read Operating_Mode`，child669952原exit1、64.489704ms、over_budget=false，stdout0B，stderr72B为“error: bus.connect (handshake) motor_id=1: 握手电机无响应：id=1”。这是握手错误，未取得Mode值；没有本次完整原帧，不能据此判断供电、线缆、电机或具体收发原因。粘性封锁后新UART0；right9 zero_diag NOT_RUN，SYNC_WRITE/WRITE/Enable0，STOP51和完整电机身份UNKNOWN，原run3f804c06/213920B仍NEEDS_MANUAL_POWER_CUT。此次没有新的右轮动作观察结论，也没有重复前轮+50读回。

61直属child/122原流逐SHA/字节核对并真实回收；driver自然dead1，START至同dead态观察结束均在新GO内。首observer绑定误用未加引号None标记在本机assert失败，owner27759随后NOT_LAUNCHED/pidnull；原回执保留，改正后owner27802/SSH27803仅观察同一进程，不重跑硬件。collector仅核终态并回收所属dead pane两个child，无独立17后验或UART。156件唯一原包715be0c1/123007B已收回，[本轮摘要](../../.verify/base_zero_sync_diagnostic_2026-10-13_r2_hardware/summary.json)保留失败与准备分界。

用户直接确认两个电源已切断、机械臂和架空底盘仍稳定支撑，09:54:45UTC收到[断电回执](../../.verify/base_zero_sync_diagnostic_2026-10-13_r2_hardware/followups/manual-power-cut-confirmation-r2.json)。当前OFF_USER_REPORTED、看护结束；实际断电时刻和独立电气遥测UNKNOWN，不回填原NEEDS/STOP/身份。下一步先核对左侧电机供电和总线连接，当前不再自动重试右轮、执行新UART或放宽停止门。新front3候选仍仅本机验证，未用于本轮；SSH已恢复且此前软件质量目录实际不存在，未补跑fmt/Clippy，因此本轮记录未提交。架构、S0/S1、正式安装和服务未改；右H2完整PASS、D15 OPEN/DEFERRED、deployable=false及旧±50原因保持。

### 前轮最小只读候选本机通过，Orin验证因SSH握手超时未确认（2026-10-13）

断电回执已收到，当前OFF_USER_REPORTED，机械臂和架空底盘支撑由用户确认；本轮看护已结束。离线核对原seq311请求及有效RX0032，普通READ与SYNC_READ均使用地址58/2B，未发现此帧的地址、端序或符号转换错误；固件读值、两种读法的现场差异、旧±50原因和物理轮转动仍UNKNOWN。

复用已有readonly12入口，候选d6b74da1仅固定right8三读：SYNC_READ TE、SYNC_READ PV、普通READ PV，同一b2冻结ELF，每次保留strace；任一非零或未知立即封锁后续UART，不为凑比较追加读取。位置名单仅作冻结GO身份元数据，不调用动作；APPROVED_TOOL及当前stat/USB/现场/GO均未绑定。本机3个不同内存方法通过，默认CLI0、未绑定执行2及三个直属child自然wait/reap，五输入当前与原回执一致。独立窄静审无实质发现，root核对候选和回执；不称原生或实机通过。[本轮摘要](../../.verify/base_front_pv_compare_2026-10-13_r1/summary.json)固定证据边界。

一次Orin提交质量启动尝试及一次同阶段只读观察均SSH255，原错误为banner exchange超时；owner10621/SSH10622及owner10865/SSH10866真实回收，无信号。远端目录、pane/driver、源码/编译器窗和fmt/Clippy结果均UNKNOWN，不能据握手失败断言远端未运行。没有重启未知阶段或额外网络修复；本轮没有新Rust修改、GO、UART、动作、正式安装或服务操作。因实际提交前fmt及根Clippy未确认，当前断电记录和候选保持未提交。下一步连接恢复后先读取已有软件阶段状态，再补齐原生验证；实机读取另需当次现场事实和独立GO，当前不要求恢复供电。原NEEDS/STOP51及身份UNKNOWN、D15 OPEN/DEFERRED和deployable=false不变，以下历史原件不回填。

### 本轮人工断电收尾已确认（2026-10-13）

用户直接确认“两个电机电源已切断，机械臂和底盘仍有稳定支撑”；root于08:44:17UTC记录收到确认。当前电源为OFF_USER_REPORTED，实际断电时刻及独立电气遥测UNKNOWN。[断电回执](../../.verify/base_zero_sync_diagnostic_2026-10-13_r1_hardware/followups/manual-power-cut-confirmation-r1.json)单独追加，原1eb89ee run/摘要/帧审查中的PENDING均为当时状态，不改原NEEDS、STOP51 UNKNOWN或身份UNKNOWN。本轮GO已消费、看护结束，回执不构成恢复供电或新读取/动作授权；此后不执行UART。下一步只离线核对前轮+50的普通READ与SYNC_READ路径，并准备最小只读比较，保留停止门和Phase。

### 右轮SYNC零值子项通过，前轮最终+50导致停止未确认（2026-10-13）

用户本次直接确认供电、三轮稳定架空、空载双臂支撑、区域空旷及持续看护可立即断电。root首次观察08:08:00UTC固定原600秒至08:18，不续期；本次fresh17项/23child、当前USB双方changes{}及58refs/唯一完整PLAN通过。PLAN child668879真实0/reap，canonical85a1defa；root和独立窄审均核实。首owner1497因相对input在本机NOT_LAUNCHED/pidnull、SSH0拒绝并完整保留；修绝对路径后owner1560/SSH1561才执行唯一fresh，没有硬件自动重试。独立GO仅一次，START owner2276/SSH2277真实0/reap，driver668951/@110/%110自然exit1后所属dead pane已回收。

本次仅right9/base_right_wheel三写：新冻结b2 ELF发单ID SYNC_WRITE46/2B Goal0，无ACK；原工具写TE1/TE0两份有效ACK。发送完成到Enable实际539.24678ms（≥50ms下界，不称精确50ms，也不据此归因）。0/200/400ms三份SYNC_READ PV原字均0，完成19.731616/211.676957/419.639071ms均在原500ms内，zero_diag记录子项PASS。观察finished-start500.609105ms，另elapsed500.609649ms保留；只证明三个离散读回，不证明连续物理静止、零速启扭通用安全、SYNC指令或等待改变的因果。

最终停止读取在前轮right8 seq311普通READ返回`read Present_Velocity@8 = 50\n`。同份strace保存PING、Model777及地址58/2B请求；TX `ff ff 08 04 02 3a 02 b5`，RX `ff ff 08 04 00 32 00 c1`，ID/长度/实IO字节/checksum有效，原字0032与stdout+50一致，不混同历史8032/-50。有效状态包不回显请求地址，不据单包推定运动或具体原因。原STOP51 UNKNOWN且粘性cleanup FAIL，right9最终STOP未读、完整身份UNKNOWN；zero_diag最终uart_blocked=true，不能外推整轮PASS。原run447e79d5/492509B为NEEDS_MANUAL_POWER_CUT，旧FAIL全部保留。

329直属child/658原流全真实回收；215 UART CLI调用（含三写），最后seq311，此后18条仅非UART，粘性封锁后新UART0。OBSERVE owner2650/SSH2651实际exit1/reap与同driver自然dead1已观察；从实际START入口至死态观察结束的整段壁钟边界均在本次GO内。collector仅核同dead pane并回收两个child，owner2863/SSH2864真实0/reap，不做独立17项后验或UART。701原包9ecb11a6/197876B逐件封存，[本轮摘要](../../.verify/base_zero_sync_diagnostic_2026-10-13_r1_hardware/summary.json)、[root原件审查](../../.verify/base_zero_sync_diagnostic_2026-10-13_r1_hardware/review/root-hardware-incident-review-r1.json)分别记录子项读回和整轮停止失败。

已立即请求现场切断两个电机电源并保持机械臂和架空底盘支撑，实际断电回执PENDING；最后供电只有用户本次ON，独立遥测UNKNOWN。禁止后续UART、动作、GO复用或自动重试；下一步先完成断电收尾，再只准备前轮普通READ与SYNC_READ的窄核，不写Phase或放宽停止门。提交前唯一纯软件门使用原隔离SOURCE，latest nightly实际e9bbadf6更新→fmt→默认根Clippy0/零警告；597源三窗与主树同ae00cf4f，native_git_head=null，来源标签为rootGit5ee8a9c，不称原生Git对齐。5Runner child/10原流、driver669458/@111/%111自然0、自pane回收及owner4280/SSH4281真实0/reap，无信号。34件唯一原包86c4b6b6/138277B保全，[提交质量](../../.verify/base_zero_sync_diagnostic_2026-10-13_r1_hardware/commit-quality/commit-quality-verification-r1.json)不作为硬件停止后验；没有新real-hw检查、测试、release、ELF执行、USB/独立17后验、安装或服务激活。 [独立帧审查](../../.verify/base_zero_sync_diagnostic_2026-10-13_r1_hardware/review/independent-sync-zero-wire-incident-review-r1.json)与root证据审查均通过，但原NEEDS及断电PENDING保持。前一软件提交5ee8a9c三次push因网络断开未完成，最后远端仍67d8b2a。本轮三轮固件/主动运动、腕部和头部长期参数仍OPEN，右H2完整PASS、D15 OPEN/DEFERRED、deployable=false保持。以下历史原件和原失败全部保留。

### 单轮SYNC零值候选软件通过，实机尚未启用（2026-10-13）

原XLeRobot采用地址46、2B的SYNC_WRITE（0x83）发Goal_Velocity，旧R4采用普通WRITE（0x03）；旧栈发送零值后至少等待50ms才Enable。这里只做这两个差异的单轮对照准备，不解释旧±50因果，不改Phase/Lock/Mode/PID或使用厂家另一种地址41/7B接口。既有feetech_poke新增固定`sync-zero`，不接受寄存器/数值参数，沿既有型号握手及sync_write_by_id；明确无ACK，只报告发送，不证明收到、静止或零值启扭安全。无新总线API、依赖、事务框架或生产速度入口。

Orin独立导出源597项三窗与主树逐件同ae00cf4f，最新nightlye9bbadf6实际fmt→默认及real-hw根Clippy零警告、15例子方法（新增2为子集）、三条参数在开串口前原exit2拒绝通过。仅诊断例子release，新ARM64 ELF b2bfedb1/1507224B已冻结，正式产物未替换；原生Git=null，导出基于67d8b2a加单Rustdirty，不称Git对齐。Mac旧080nightly15方法独立保留，不称最新或累计。9原生Runner child/18流及3嵌套参数child/6流、driver665185/@108/%108、owner69718/SSH69719全部真回收；68原包666dc4a7/10232733B已封。

候选main3b681657/27496B、teste97bda49/20991B，仅一次right9单ID SYNC Goal0→发送完成后至少50ms→原Enable→原0/200/400ms三PV及500ms预算→TE0/STOP51与完整身份。首写无ACK，两TE写沿原有效ACK；初始停止/不可信回包/写身份失败粘性禁止后续UART，有效非零PV先原TE0收尾；只数值GO到期保留已Enable的原独立TE0收尾，不借到期掩盖坏帧/坏ACK。三轮固件仍DEFERRED，诊断与整run结果分别保留。Mac最终同6方法child82052真实wait0/reap；旧17c审查发现DEVICE位于argv0字面边界，新一行与既有反例窄补闭环，旧审查/候选/回执全部保留。最后唯一Orin Python批次三个child668608/668609/668610原exit0/0/2真wait，6方法、默认PLAN及缺GO加载前拒绝通过；不累计Mac6或旧轮次数。25原包7bcb8823/45663B、driver668604/@109/%109自然0与owner82495/SSH82496真实0/reap已核。

[本阶段摘要](../../.verify/xlerobot_sync_zero_2026-10-13_software/summary.json)、[原生Rust验收](../../.verify/xlerobot_sync_zero_2026-10-13_software/native/native-verification-r1.json)、[原生候选验收](../../.verify/xlerobot_sync_zero_2026-10-13_software/diagnostic-native/native-verification-r1.json)及两次窄独审均保存。本阶段USB/UART/新GO/动作0；七份未绑定运行草稿已完成：旧55refs只追加helper/main/ELF至58，binary_paths仅追加新ELF；原PTY/c入口和整体shlex.quote保持，所有现场/GO/PLAN/pane动态值None先拒绝。7 body AST、6 bash-n及None首门一轮通过，7验证child真wait/reap；normal依原STOP51/身份门保留diagnostic FAIL partial，manual只2collector/no新UART。root已静核关键门，实际fresh/完整绑定PLAN及GO仍未执行。先前R2只读/STOP51/身份通过已提交67d8b2a，R2看护结束/GO已消费，最新电源仅用户ON、独立遥测UNKNOWN。下一次实际Enable须当次三轮已稳定架空、空载双臂支撑、区域空旷和持续看护可立即断电的新确认及独立GO，无需重复断电或USB重插。腕部/头部长期参数、三轮主动运动及旧±50仍OPEN；右H2完整PASS、D15 OPEN/DEFERRED、deployable=false；架构、S0/S1、正式安装与服务未改。以下全部历史原文保留。

### R2断扭只读诊断通过，三轮主动运动仍待验证（2026-10-13）

用户本次直接确认恢复供电、稳定架空及持续看护。root固定首次观察05:42:50UTC、原600秒截止05:52:50UTC，没有续期；新非UART17项/23child、当前USB（两侧变更字段均空）、57refs和完整PLAN经root/独审通过。maincf651bb2/helper70d777bd使用已核actual fixtures路径，只签一个独立GO、启动一次driver664542/@106/%106，无寄存器写、Enable或动作。

三轮初始和最终共六份普通PV strace全部保存，逐份为PING、READ型号、READ速度三对收发；ID、长度、IO实字节数和checksum有效，型号777、PV原字0000、bit15解码0与原stdout一致。它们证明本次读回和解码一致，不能解释R1/旧R4的±50或证明物理轮转动。右9七项raw preflight实际为Model777/FW3.10/Mode1/Phase12/Lock1/GV0；0/200/400ms三组TE→PP→PV均TE0/PV0，9次读取在原500ms内完成，最后434.377775ms。整段观察finished-start为500.560522ms，另一次elapsed采样500.560906ms，原值全部保留，不称严格500ms内结束。

本轮只读diagnostic、STOP51全整数零、完整身份及独立nonUART17项/23child均PASS；298直属child/596原流真实回收，220 UART调用为204普通read和16read-raw，末seq280后18条仅非UART。唯一tmux启动前至同driver真实dead观察者结束的整段壁钟界限均在原GO时效内。driver自然exit1并回收自dead pane，原整轮run5996ae29/435402B的FAIL仅为BASE_FIRMWARE_CONTRACT_UNVERIFIED：三轮仍DEFERRED，没有主动wheel/pulse；不回填PASS。644件原包3d41791e/268820B永久保存，[本轮摘要](../../.verify/base_feedback_diagnostic_2026-10-13_r2_hardware/summary.json)、[root原帧审查](../../.verify/base_feedback_diagnostic_2026-10-13_r2_hardware/review/root-hardware-readonly-review-r2.json)区分只读通过与整机未闭环。[独审](../../.verify/base_feedback_diagnostic_2026-10-13_r2_hardware/review/independent-hardware-readonly-review-r2.json)与root验收均通过。独审首轮把journal和run.commands要求整dict相等而误拒，原FAIL报告另存；六行ordinary_read_trace是冻结helper在journal落盘后追加的元数据，共享字段逐项一致且六原帧SHA/bytes吻合，只修审查口径，不改原run/journal或重跑硬件。

本机实际fmt0（515af8）后，本轮独立Orin纯软件门最新nightlye9bbadf6→fmt→根Clippy0/零警告通过，597输入三窗与当前主树逐字节及exec-bit同068277d8；Git894+5dirty不称Git对齐。11child/22原流、driver665024/@107/%107自然dead0和owner28913/SSH28914真实0/reap，54原包9c4acea9/135060B已保全。[提交质量](../../.verify/base_feedback_diagnostic_2026-10-13_r2_hardware/commit-quality/commit-quality-verification-r2.json)不覆盖硬件结果；未重跑Rust/31Python测试、release或新控制ELF。

GO已消费，本轮看护已结束；最新供电仍仅用户本次报告ON，独立电气遥测UNKNOWN。正式ca14/六服务沿本轮非UART检查状态，未安装、启动或改生产运行。旧R1 NEEDS及人工断电回执、旧R4实际FAIL/±50根因仍保留，腕部/头部长期参数、三轮主动运动OPEN，右H2完整PASS、D15 OPEN/DEFERRED、deployable=false不变。

原XLeRobot源码采用addr46、2B的Goal_Velocity SYNC_WRITE(0x83)，不是厂家WriteSpe的addr41/7B；与旧R4普通WRITE(0x03)的包格式存在区别。下一步仅另阶段准备单ID SYNC_WRITE/addr46/2B/零值的受监督对照，不写Phase、不换41/7B、不改原期限/停止门；仍只是诊断，不预称现场FW3.10/Mode1/Phase12下零速启扭安全，不复用本轮GO。以下历史原件和原失败不改。

### 右轮只读R1因左轮初始PV非零而停止，人工断电收尾已确认（2026-10-13）

R2正确辅助路径草稿的[窄独审](../../.verify/base_feedback_diagnostic_2026-10-13_r2/independent-runtime-helper-fixtures-review-r2.json)及root验收已通过；软件ready，现场/当前USB/PLAN/GO仍未绑定。下一轮先核新现场及当前USB，仅读一次，原停止门通过后才采右轮三组，不启用电机、不改参数、不自动重试。

本次用户重新确认恢复供电、三轮稳定架空、空载支撑及持续看护。root以04:26:55UTC首次观察直接回复时刻固定原600秒期限，随后新非UART17项/23child、当前USB、本轮56项引用和完整PLAN均通过；独审无硬问题。只签一个GO、启动一次driver663805/@103/%103，没有Enable、参数写入或动作。硬件wrapper仍87e3，原帧辅助仍406，原冻结工具及正式安装未改。

初始停止读取在left wheel `right7` 普通Present_Velocity返回`read Present_Velocity@7 = -50\n`，seq130原stdout30B/SHA bfa326d2保留；此前Mode1/TE0/PP1175实际读到，PV被guard拒后未赋入before，GP/GV未读。right8/right9及右9七项preflight、三组TE→PP→PV均NOT_RUN。148个child及296原流真实回收，最后88项UART均为普通read argv，seq131–148仅18项非UART，粘性停止后新UART0。原辅助只对right8/9普通PV抓strace，本轮没有任何完整TX/RX原帧；不以argv、TE0或进程退出证明物理停止、反馈量化或−50来源。

原run2a3e8f75/289581B为NEEDS_MANUAL_POWER_CUT，STOP51和完整身份后验UNKNOWN；collector仅两child核已退出pane并回收自pane，未做独立17后验或新UART。330原包3ed0ea4d/142598B、9项运行准备原包、现场/GO/实际脚本和六组SSH原件已归档。[R1永久摘要](../../.verify/base_feedback_diagnostic_2026-10-13_r1/summary.json)与[独审](../../.verify/base_feedback_diagnostic_2026-10-13_r1/review/independent-hardware-incident-review-r1.json)分别记录现场失败与证据审查通过。

用户随后直接确认两个电源已切断、机械臂和底盘支撑保持；root04:42:09UTC追加[收尾回执](../../.verify/base_feedback_diagnostic_2026-10-13_r1/followups/manual-power-cut-confirmation-r1.json)，结束R1看护。实际断电时刻/独立电气遥测UNKNOWN，不回填原NEEDS/FAIL，不授权下一轮恢复供电或GO。当前仅继续R2最小软件准备。

本轮提交质量门另一个唯一纯软件批更新latest nightly仍e9bbadf6，实际fmt→根Clippy0/零警告，597源三窗及主树同068277d8；11child/22原流与54原包da0ba617/135030B已核。原本机agent检查器缺旧wait_returncode字段和root误把旧stdout路径当引用字典的FAIL均保留；只适配读取同原件，未重跑原生质量批。本机实际fmt0、无新Rust测试或release，不将质量门当硬件停止证明。

R2仅将同一次普通PV读取的strace匹配由8/9扩到7/8/9并同步旧辅助元数据；main仅新RUN_ID和helper指纹，raw目标仍仅right9，不改任何非零/未知粘性门、GO前后时效、500ms/0-200-400ms或STOP51。helper70d777bd/12220B、maincf651bb2/18648B、test815ec499/37227B，旧87e3/406字节不变。原本机31方法实际通过并已报告，后整个target目录消失，原本机流、回执及原R2独审文件丢失；当前不能从这些原件独核，也不重造或重测。原生同31唯一批已从Orin仅取回现存19原件并核验：child664208原0/reap，driver664204/@105/%105原自然dead0后回收，五输入及七上传件前后同字节；没有重测。原0af7/46557B压缩字节未在Orin持久化且随本机target丢失，新恢复包670fd4ae/46557B另名保全现存原件，不能冒充旧包。新collect owner1442/SSH1443真实0/reap，不回填旧93330/93331本机原回执。首份七runtime草稿在验证前随target消失；永久新份逐SHA同，并单独做语法/None首门，不能声称首份已验证。

本机同时存在带shutil.rmtree及跨Rust目录清理计划标记的进程，具体删除者因果未独证；R1永久原包、Git和597源码不变，未干预其他任务。后续临时工作使用永久证据目录，R2新binding须在旧55refs后追加helper→main至57，新现场事实和当前USB才能新PLAN/GO。root发现Orin实际helper只有`candidate/fixtures/raw_right9_from_r14_r2.py`，已令七草稿同步修正helper绝对路径及内嵌launcher，未额外复制或修改远端源；路径修正前17原件6477647f/29662B及旧独审保留，局部验证器的历史误断言/未持久化PID批次UNKNOWN照实记录，不回填。[目录消失记录](../../.verify/base_feedback_diagnostic_2026-10-13_r1/review/local-target-disappearance-r1.json)、[恢复回执](../../.verify/base_feedback_diagnostic_2026-10-13_r2/native-python/recovery-collection-receipt-r2.json)及[R2摘要](../../.verify/base_feedback_diagnostic_2026-10-13_r2/summary.json)如实保留边界。机械臂、头部长期参数及三轮主动运动未闭环，右H2既有完整PASS、D15 OPEN/DEFERRED、deployable=false保持；以下原历史不改。

### R4人工断电收尾已确认，下一轮只读尚无新现场事实

用户直接确认“两个电机电源已切断，机械臂和底盘仍有稳定支撑”；root独立追加[R4后续回执](../../.verify/base_zero_diagnostic_2026-10-12_r4/followups/manual-power-cut-confirmation-r4.json)，结束R4看护。当前仅用户报告OFF，实际切电时刻及独立电气遥测UNKNOWN；原NEEDS_MANUAL_POWER_CUT、STOP51/身份UNKNOWN和真实±50 FAIL不回填。旧PENDING仍保留在运行、原生软件原件及[答复前软件摘要](../../.verify/base_feedback_diagnostic_2026-10-12_r1/summary.software-stage-before-followup.json)，当前摘要另指后续回执。

本次确认仅完成收尾，不授权恢复供电、UART或新GO。右9只读取证候选的软件门已通过，七份复用已验`-c`入口的准备、启动、观察及收尾草稿现已独审；六项shell语法、六项Python首门和observer未绑定exit2均有本机真实回收回执，仍未上传或执行Orin运行前置。新现场、GO及USB当前绑定均为None；[root草稿验收](../../.verify/base_feedback_diagnostic_2026-10-12_r1/root-runtime-draft-review-r1.json)和[独审](../../.verify/base_feedback_diagnostic_2026-10-12_r1/runtime-preparation/independent-runtime-review-r1.json)仅证明本机草稿条件。

收到本轮新的供电、架空支撑及持续看护事实后，先核USB当前枚举和17项非UART原件/23child真实回收，再绑定新PLAN/GO。旧binding仅允许三项枚举字段变化并追加候选引用，窗口前完整检查、窗口内轻量GO检查和原500ms/600秒门均保持。不要求重插USB、不复用R4现场TTL，不下发Enable或参数写入。目标仍为机械臂、头部与底盘实际成功，长期参数与三轮主动运动未闭环，D15 OPEN/DEFERRED、deployable=false保持。

### 右轮断扭位置/速度原帧候选软件通过，实机未运行（客户端2026-10-12，原生UTC10-13）

继续原整机成功目标，R4真实零目标FAIL与人工断电PENDING保持。本次没有新的现场事实、USB当前绑定或硬件GO，旧用户ON报告不当作当前供电遥测。公开资料仅提示Phase12小单位下raw50名义约0.73RPM，不能证明反馈量化、允许零速误差或解释物理轮转；R4使能后缺位置序列。因此先补无写入的位移取证，不实现Phase专用写入口或新运行时框架。

新单次维护wrapper固定right9，复用冻结d97维护器、406295原帧辅助和13c/f201工具，仅本次加载辅助实例META增加PP56/2B。构造ProbeEngine时即封装只读门，覆盖初始和最终17电机普通停止读回；TE/PV/GV非零、未知stdout、坏帧、目标/前缀越界或GO失效均粘性封锁后续UART。raw只限右9九字段加PP，所有WRITE/Enable禁止，原17×3 STOP51及身份门保持。固定三组请求0/200/400ms，每组TE0→PP→PV，全部原帧完成资格仍为原500ms；保存实际请求/完成时间与lateness，不声称纳秒准时。PP仅unsigned原值，不解码单位、符号、量化或据此宣称物理停止；三轮DEFERRED/firmwarefalse保持，诊断成功不等于底盘主动运动或原整轮PASS。

离线审查抓到并修复三处实际问题：首组/唤醒后真实单调时钟推进被过严时间门误拒；完整非UART门只放在窗口前，窗口内每次UART前/返回及尾部复用当次GO轻量校验；未启动请求不再冒领上一stdout，只有本次sequence与commands双增才绑定原件。有效非零或过期返回帧保留FRAME_VALID_REJECTED，坏帧/未启动UNKNOWN。r1/r2内存PASS不回填为这些问题已验证；初次独审29通过/1失败仅会话工具输出存在，无单独原流文件/回执，报告如实注明，未重造原字节或重跑旧SHA。[独立审查](../../.verify/base_feedback_diagnostic_2026-10-12_r1/review/independent-final-review-r1.json)与原阶段测试回执保留。

最终main87e3c135/18648B、testc20998e1/34451B，本机r3同30方法PASS、child71691真实wait0/reap，五输入首尾相等；root另实际默认PLAN_ONLY0/缺GO2两child自然回收。唯一Orin纯软件batch更新浮动nightly到e9bbadf6824e65b3583c2dda3a0926ec76df9143，随后实际fmt→根Clippy0/零警告，597源三窗及主树逐项同068277d8，编译器首尾相同；080仅历史字段，不作为新硬pin。原生同30方法、默认0、缺GO2及旧binding静态完整PLAN均通过；跨平台不累计60，静态fixture不构成当前USB或硬件ready。

12个Runner child/24原流、4个纯Python嵌套child/8原流及34个外层观察child均自然wait/reap；driver658962/@102/%102自然dead/exit0后仅回收所属pane，owner72015/SSH72016真回收0。等待过程中另一次严格只读观察核实同pane和Clippy659035真实存活，没有重启batch；观察owner72551/SSH72552与单tmux child自然回收。唯一82件原包203f895f/203476B、八上传原件和五候选/fixture输入已逐字节核验，原生Git仍894加五dirty，不称Git对齐。没有UART、硬件动作、GO、Rust测试、release/控制ELF构建、正式安装或服务操作。[永久摘要](../../.verify/base_feedback_diagnostic_2026-10-12_r1/summary.json)及root验收记录保存软件/硬件分界。

已再次询问R4实际断电收尾，仍PENDING，仅收尾确认、不要求恢复供电或重插USB。下一步收到断电回执后，再按新的供电、架空稳定支撑及持续看护事实核当前USB，绑定新PLAN/GO，仅执行本只读取证；不复用R4现场TTL或GO，不自动试动作。R4原0/0/+50、TE0后−50、NEEDS/STOP51未知及所有历史FAIL保留；腕部/头部长期参数和三轮主动运动未闭环，右H2完整PASS、D15 OPEN/DEFERRED、deployable=false不变。以下历史原文全部保留。

### 右轮零目标实际失败，启动入口已通过（2026-10-12，R4人工断电待确认）

用户本轮直接确认“已恢复供电，三轮稳定架空，现场全部就绪，可执行本次右轮测试”；root在06:13:01.689362UTC记录现场事实，原600秒期限没有续期。先核供电后当前非UART17项/23child与USB枚举，均通过且两端身份仍同347a65冻结binding；仅一个PLAN child655208完成新准备。准备SSH遭网络255，原超时输出和真实wait/reap回执保留；随后只读取回已经完成的七原件，未重跑准备或增加UART。root本地metadata生成的TextIOWrapper变量覆盖TypeError及0B pins原件也保留，修复后只执行一次。新canonical PLAN为3fc25e07；签GO时剩16.786409秒，未延长现场TTL，所有已抓36条UART事件都在GO内，末RX距过期9.46815秒；不将本轮失败归因为GO过期。

实际`python -c`入口保留stdin PTY并进入候选：driver655312/@100/%100自然dead/status1，GO.consumed及原run已封存，只回收所属dead pane。右9初始原帧TE0/PV0/GV0/Model777/FW主3次10/Mode1/Phase12/Lock1。仅三条具名WRITE：Goal_Velocity0、Torque_Enable1、Torque_Enable0，三ACK均有效；GV0和TE1原读回正确。原500ms窗口内0/200/400ms三次PV原帧为0、0、+50，第三帧`FF FF 09 04 00 32 00 C0`完整校验通过，观察于410.528625ms结束，三次零值门FAIL。随后立即Disable ACK通过、TE普通读为0，但普通PV为**−50**，响应`FF FF 09 04 00 32 80 40`有效；与前一+50帧分别保存，不推断实际轮转动、方向、速度单位或Phase因果。TE0普通读没有该次完整strace，不冒充原帧证据。

最后UART child为seq217；非零停止读回触发粘性拒绝，后102项电机读取未执行，STOP51和完整身份后验UNKNOWN，原run为NEEDS_MANUAL_POWER_CUT。235不同直属child/470输出、六strace及509件硬件原包逐SHA/字节核实，203 exit0、32空fuser查询为既定exit1，所有child真实wait/reap，无信号、超预算或活跃child。收集仅两个非UART终态/所属pane回收child自然0，没有增加独立17项后验、UART或动作。原硬件包f259c47c/161450B、run0b5b6aa8/427606B永久保留；[R4摘要与原件索引](../../.verify/base_zero_diagnostic_2026-10-12_r4/summary.json)、[独立帧审查](../../.verify/base_zero_diagnostic_2026-10-12_r4/independent-hardware-frames-review-r4.json)与root审查记录分开保存。

root已要求现场立即切断两个电机电源并保持机械臂和架空底盘支撑，实际断电确认仍PENDING。当前最后用户供电报告为ON、独立电气遥测UNKNOWN；进程退出、TE0和ACK不代替物理停止或断电。GO已消耗并过期，没有后续UART、Phase/Mode/Lock/PID/EEPROM写入、自动重试或新动作GO。

仅外网只读核对厂家当前公开内存表：门户响应version201，区别于旧version181和现场FW3.10，原响应/逐字MD及HTTP200回执已保存。[窄候选与官方引用](../../.verify/base_zero_diagnostic_2026-10-12_r4/research/narrow-next-step-candidate.md)记录bit3的零目标含义和Lock1写配置掉电不保存的资料说法；未明确绑定FW3.10/Mode1或证明临时Phase即时采用。Phase12含bit3，仅是待核假设。官方SDK从41写7字节与XLeRobot裸Goal46写不同，不同时引入两处变化。Phase12→4临时对照仅资料候选，没有实现、GO、UART或厂商联系，不放开普通工具Phase写禁令。

提交前root实际fmt0；Orin唯一最新nightly更新（仍080）→实际fmt→根Clippy0/零警告，11质量child/22流真reap。597源三窗及主树逐件同068277d8，编译器两窗相同；原生Git仍894加五dirty，不称Git对齐。53件唯一质量包bf813fd1/134620B已核，软件driver655707/@101/%101自然dead0后仅回收所属pane，owner77375/SSH77376真实wait0/reap。没有新Rust测试、release或控制ELF构建、安装或服务激活。下一步先完成人工断电收尾，只核厂家Phase bit3/Lock1与当前FW组合的窄资料及具体候选，不新增框架。三轮主动运动和零速合同、旧±50、左腕/头部长期参数仍OPEN；右H2完整PASS、D15 OPEN/DEFERRED、deployable=false保持。以下已提交准备态和旧FAIL原文全部保留，不回填。

### R4脚本准备与提交质量通过，等待恢复供电后的当前事实（2026-10-12）

R3断电回执已记录并结束看护，当前仅用户OFF确认、独立电气遥测UNKNOWN；原启动/观察FAIL、driver真实reap NOT_OBSERVED和硬件零速/STOP51 NOT_RUN全部保持。R4逐字复用已验`-c` launcher和body填七pins后整体shell引用的starter，observer/seal只换本轮路径/标签，保留新GO.consumed原件。新软件recipe四个当前原件输入pins均None，只接供电后本次新非UART17 stdout及真实wait/reap回执；以347a65冻结binding为基线，只有两端device_inode/link_inode/usec按新事实更新，其它serial/devpath/tty/rdev/权限、host、55refs、保护和校准固定。一个无execute的PLAN child，不额外运行非UART或创建GO/现场文件。父fd0静态调用链四冻结件已核，未见父stdin关闭/重定向；子进程TemporaryFile/DEVNULL不关闭父0，不宣称修复实际硬件入口已经通过。

本机AST、六语法child自然0/reap、七虚拟pins引用往返及独审通过；R4新的native binding/PLAN准备仍NOT_RUN。root实际fmt0，Orin唯一latest nightly更新（仍080）→fmt→根Clippy0/零警告；11child/22原流真回收，597源三窗068277d8逐份同、两编译器窗相同，原生Git仍894加五dirty。53件唯一质量包b0a28423/134568B已核，软件driver655005/@99/%99自然dead0后只回收所属pane，owner70072/SSH70073真wait/reap0。无新Rust测试、release/控制ELF、旧夹具重跑、GO、UART、硬件动作、安装或服务激活。[R4永久摘要](../../.verify/base_zero_diagnostic_2026-10-12_r4/summary.json)记录准备分界。

新的现场问题已发出：实际恢复供电、三轮架空稳定、空载双臂支撑、接触区空旷及持续看护/立即断电，尚未答复。本轮不由断电回执签GO。收到当次事实后先采当前USB枚举，再生成并核新binding/PLAN/GO，填已验入口，只一次right9 Goal0→Enable→原0/200/400ms三PV→Disable及STOP51；原500ms/三写/三轮DEFERRED不改，不要求重插USB或额外断电。D15 OPEN/DEFERRED、deployable=false及长期参数/旧±50保持，以下历史原文不改。

### R3后续人工断电确认，下一轮先按供电后的当前枚举绑定（2026-10-12）

用户直接确认“两个电机电源已切断，机械臂和底盘仍有稳定支撑”；root已记录收到确认并结束R3看护。[独立后续回执](../../.verify/base_zero_diagnostic_2026-10-12_r3/followups/manual-power-cut-confirmation-r3.json)单独保存，实际断电时刻及独立电气遥测UNKNOWN。更新当前供电为OFF_USER_CONFIRMED；原启动/观察FAIL、driver真实wait/reap NOT_OBSERVED、硬件零速和STOP51 NOT_RUN保持，原事件pending字段仍是当时原件，不回填为PASS。

本回复只完成收尾，不构成新GO。R4仅准备已验`-c`入口和本轮独占观察/封包副本，不重新运行夹具或改候选。下一次在实际恢复供电和持续看护确认后，先用已有非UART门取得USB当前枚举，再生成新binding/PLAN/GO并只做一次right9零目标；避免在断电期间绑定后又因恢复供电发生枚举变化。不强制重插USB或额外断电，原500ms三PV、三写及STOP51门保持。以下历史原文不改。

### 底盘启动入口修复软件通过，零目标实测仍未运行（2026-10-12，R3已终止）

用户本轮直接确认恢复供电、三轮架空稳定及看护就绪。R2实际starter在USB前置断言拒绝，record/tmux/candidate均未到，UART和电机写0；只读17项/23child核实左侧5A7C121427的serial/devpath/tty/rdev不变，仅device_inode1544→1552、link_inode1547→1555、USEC_INITIALIZED3368981464912→3536208877616，右端全部不变。R3只更新这三字段并使用新独占binding/PLAN，55refs/保护/校准和冻结f59455候选、406295 helper保持。一次PLAN child654519自然0/reap，复用同一17项原件，不重跑或扩大诊断。沿同一人类确认只签新的plan GO，原600秒现场期限没有续期；R2前置FAIL完整保留，[R2追加摘要](../../.verify/base_zero_diagnostic_2026-10-12_r2/summary.json)。

R3仅一次tmux创建回执返回654721/@96/%96，实际launcher七pins逐字核实；但driver.log仅18B `no such pane: %96`，随后observer也失败。output/run/GO.consumed均未创建，结合冻结执行顺序本轮未进入UART和电机操作；零速诊断及STOP51都NOT_RUN。driver后续已未见不等于真实wait/reap，保留NOT_OBSERVED；不将软件入口故障写成零速电机失败或清场PASS。22件原包8b01f11a/95821B及原启动、观察、独审证据永久保留。

此次软件对照补上旧夹具未覆盖的stdio差异：旧here-doc stdin为pipe，stdout/stderr重定向后复现`no such pane: %97`、marker未到；改成同正文`python -c`，stdin保持PTY，新软件driver654935/@98/%98自然dead0、marker同PID，单option child654938自然wait0/ECHILD，六collector child全0/reap，只回收所属软件pane。首collector第31行身份断言FAIL及旧start/observe回执未落盘完整保留，没有重启oldcase；只读收old原件后首次执行new-c。机制与[tmux3.2a PTY错误删除逻辑](https://raw.githubusercontent.com/tmux/tmux/3.2a/window.c)一致，但未采历史直接HUP/内核时间线，不宣称全部历史根因闭环。新软件原包20件/f435d6cb/11224B。future R4 launcher只保留stdin；starter先在Python正文填七pins，再整体shell引用，避免repr单引号破坏-c参数。实际pins全部None，静态引用/独审通过，没有R4 GO、现场准备、UART或硬件动作。

本机实际fmt0及Orin经既定代理最新nightly080更新→fmt→根Clippy0/零警告通过；11质量child/22原流与597输入三窗068277d8逐份同，原生Git894加五dirty，不称Git对齐。没有新Rust测试/release/控制ELF构建、正式安装或服务激活。[R3永久摘要](../../.verify/base_zero_diagnostic_2026-10-12_r3/summary.json)保存软件通过与硬件未运行的分界。

root因进程观察失败已通知现场立即切断两个电机电源并保持机械臂/架空底盘支撑；本轮实际断电确认仍待答，最后直接用户供电报告为ON、当前物理状态及独立遥测UNKNOWN。GO已过期、本轮终止，不复用现场或旧GO，不再硬件重试。下一轮先在当次供电/架空稳定支撑/持续看护确认后核USB当前枚举，再签新binding/PLAN/GO，仅一次right9裸Goal0→Enable→原0/200/400ms三PV→Disable/STOP51；不要求为本软件修复重插USB。长期左腕/头部参数、三轮主动运动/零速合同与旧±50仍OPEN；右H2完整PASS、D15 OPEN/DEFERRED、deployable=false保持。以下全部旧交接字节保留。

### R2零目标诊断软件准备通过，尚无新GO（2026-10-12）

R1用户已确认两个电机电源切断、机械臂及架空底盘支撑保持，root结束本轮看护；后续回执单独保存，原启动/观察FAIL及driver真实wait/reap未观察到不回填。当前保持断电，以下软件准备未打开电机串口。

复用固定f59455候选、406295 helper和55refs binding，profile run_id沿r1表示同一程序版本；本次round2使用新独占output/launch/GO/field/fullPLAN，PLAN仅output变，canonical3f840ab0a74f2ebedd561abd299a027c256e01bc315486ef5546eeb2159fd143。launcher保留已验证-p，四份实际启动/观察/收集草稿只变本轮路径/标签，全部现场与终态待root绑定；ready入口新现场/GO三pins仍None。原样保留三base DEFERRED及firmware_contract_verified=false；zero_diag若通过也不改整轮FAIL为PASS。

唯一Orin软件准备仅两个直属child654192/654193，完整PLAN与非UART17/23命令均自然exit0/reap、四原流核验，七新原件tar56abcb6c/49007B。R1同SHA各平台10内存方法/default0/missingGO2和-p纯软件fixture复用，不计新方法。首本地验证换行断言及root只读body/rstrip断言FAIL保持；实际recipe正文逐字同，原生没有重跑，详永久root核验记录。

提交前本机实际fmt0、Orin独立latest更新→fmt→根Clippy0/零警告，11质量child/22流及53原件bc5644aa/134613B通过；源597三窗逐项同068277d8，编译器两窗同080，原生Git仍894加五dirty。无新Rust测试/release/控制ELF、GO、UART、动作、安装或服务启动。[永久R2摘要](../../.verify/base_zero_diagnostic_2026-10-12_r2/summary.json)及48件逐份核验。

下一步软件已就绪；实际试验需当次恢复供电、三轮架空稳定支撑、空载双臂支撑、区域空旷和持续看护/立即断电的新确认。只安排一次right9裸Goal0→Enable→原500ms内0/200/400ms三个PV原帧→Disable/原STOP51；不复用R1或断电回执作为新GO，无需USB重插或重复断电。头部/左腕长期参数和底盘零速/旧±50仍OPEN，D15 OPEN/DEFERRED、deployable=false。以下全部原交接字节保留。

### R1后续人工断电确认（2026-10-12）

用户直接回复“两个电机电源已切断，机械臂和底盘仍有稳定支撑”；root记录收到确认并结束本轮看护。实际断电时刻及独立电气遥测未知，[后续回执](../../.verify/base_zero_diagnostic_2026-10-12_r1/followups/manual-power-cut-confirmation-r1.json)单独保存，不回填原启动/观察FAIL、driver未观察到真实wait/reap或实机NOT_RUN。永久130件已核；以下R1原记录中的pending为当时状态，完整保留。该回复仅完成收尾，不授权新动作或恢复供电；下一步保持断电，仅准备R2独占PLAN及使用-p修复的启动/收尾脚本，实际运行前再绑定当次现场事实及新GO。

### 底盘零目标诊断启动失败，实机诊断未运行（客户端本地2026-10-12，R1已终止）

仅准备right9/base_right_wheel裸Goal_Velocity=0→Enable→固定500ms内0/200/400ms三次PV原帧→Disable的独立诊断，可能转动，要求当次三轮实际架空稳定支撑和持续看护。原STOP51、身份及四锁门保留，三轮仍DEFERRED/NOT_RUN、BASE_FIRMWARE_CONTRACT_UNVERIFIED；没有改模式、Phase、PID或接入生产路径。本机和Orin各10个Python内存方法、默认零动作、缺GO预期exit2、55refs/完整PLAN与17项非UART软件准备通过，不跨平台累计为20。原生准备唯一30件原包12701a0f/110013B保留，五个软件child及十份输出自然回收；准备不等于实机零速语义通过。

本轮收到实际架空、稳定支撑和看护就绪答复后，root仅启动一次。starter返回driver653725/@91/%91，但实际launcher在候选exec之前执行`tmux set-option -w -t $TMUX_PANE remain-on-exit on`失败，driver.log仅20B `no such window: %91`；随后observer报`no such pane: %91`，观察FAIL保留。2520B实际launcher与已验starter七项pins替换结果逐字相同；run、动作output及GO.consumed均不存在。结合冻结d97先创建output/consumed、再创建RealExecutor及进行UART的执行顺序，本次程序路径UART和电机写均为0，zero诊断NOT_RUN；不记零速合同、STOP51或物理轮停止PASS。后续只读快照中driver进程已未见，但真实自然wait/reap仍NOT_OBSERVED，不能写成driver回收通过。R17实际launcher同式命令曾成功，本轮具体tmux目标解析/状态差异UNCONFIRMED，不把推测写成根因。

失败原件唯一42件tar为a216fb8bbdb735184c6dc0a0dd2cd9a5d543579d3faf7f99f098279c5764e6c6/152527B，候选f59455a6、helper406295f0及原件保持字节不变；[独立启动事件核验](../../.verify/base_zero_diagnostic_2026-10-12_r1/review/independent-startup-incident-review-r1.json)保留完整边界。root已要求现场立即切断两个电机电源并保持支撑，实际断电确认仍待答；本轮GO已过期、尝试终止，不复用已用现场答复或GO，不自动重试。

提交前root本机实际fmt退出0；Orin经10.0.1.80:10808代理实际更新nightly频道，结果仍为080cc27146389dbb5c8772c476b1937be9c6110f，随后实际fmt→根Clippy零警告通过。11个不同Runner child/22原流与597输入三窗逐项核验，聚合068277d843b191bc8283bab38e667b0f6a4d30e296e8c5a99a97cfe19654fcf8；driver653914/@92/%92自然dead/status0后仅回收自有pane，owner39082/SSH39083自然wait0/reap。53件唯一质量原包6605b6b5/134604B，[质量核验](../../.verify/base_zero_diagnostic_2026-10-12_r1/native-quality/quality-verification-base-zero-r1.json)通过；native HEAD894加五dirty，不称Git对齐或clean。该质量检查无新Rust测试、release/控制ELF构建、UART、GO、服务启动或实机launcher重跑。只将launcher的-w改为-p，Orin tmux3.2a单例纯软件fixture已通过：654024/@93/%93自然dead/status0，option读回on，随后只回收自有deadpane；六外层/三option内层child均真实wait/reap。五件fixture原包f8f2da30/2887B，仍未执行硬件候选或新GO；固定新模板七项pins均None，旧actual失败launcher不改。永久129件逐份核验，[本轮摘要](../../.verify/base_zero_diagnostic_2026-10-12_r1/summary.json)。

下一步收到实际断电状态后，使用新独占output/launch/GO/fullPLAN及当次现场事实，准备一次right9零目标实测；本段不构成动作授权。头部P32正/负向各单次功能已通过并恢复P16，左腕临时I1通过后恢复I0；长期参数设置仍OPEN，单次功能不证明PID因果、即时采用、重复性或掉电持久化。底盘零速合同/主动运动及旧±50仍OPEN，不扫参、加框架或改变模式/Phase。原R17及以下HANDOFF整段原文保留，右H2完整PASS、D15 OPEN/DEFERRED、deployable=false不变。

### 头2临时P32负向单次往返功能通过（客户端本地2026-10-11，R17已收尾）

本次独立GO仅执行一次left8 P16→32→16、−24往返，D43/I0不写。锚2090、目标2066；出程14样本，130.567212ms首次±4、末2069/+3，观察结束800.062744ms不延原800ms资格期限；回程3样本2087/−3于130.425830ms通过。实际负向位移21，原±4/800ms、±32/min12保持，反馈与原READ一致、未镜像。头2本次功能PASS；原整轮仍FAIL，仅因BASE_FIRMWARE_CONTRACT_UNVERIFIED及三轮DEFERRED/NOT_RUN。

七次具名写仅left8；P16/D43/I0/Lock1/CW1/CCW1恢复、STOP51整数零、85项身份及独立非UART17项/23child通过，manualcut=false。383直属child/766原流核验并自然回收；351 exit0、32空fuser为预期idle exit1。53refs收集前后完整stat一致，入口33完整stat/20历史最小绑定的边界保留。30次read-raw保存29字段（两次Lock1原输出均保留）；右8/9四次普通PV完整原帧均正零，不解释旧±50或证明物理轮静止。driver652798/@88/%88自然dead/status1后只回收自有pane。collector r2实际仅走四行形态，一行FAIL分支未实机触发，不记动态PASS；首独审误要求vanished为空的Assertion及PID651761原观测保留，按既有clean规则核验，不推断该PID用途或退出原因。

9619B独立硬件报告及root验收通过。本机实际fmt、Orin实际fmt→根Clippy零警告；11个不同Runner child/22原流/53质量原件、597输入068277d8及最新nightly080首尾一致，原生HEAD894加五dirty，不称Git对齐。无新Rust测试、release或控制ELF。永久87件逐份核验，三个原包分别41/808/53原件，[摘要入口R17](../../.verify/position_return_2026-10-11_r17/summary.json)。本轮GO已消耗、看护结束，供电仅用户报告ON、独立电气遥测UNKNOWN。下一步仅准备right9的XLeRobot裸Goal_Velocity=0单轮诊断；用户仅确认可安排三轮架空稳定支撑，实际到位和新看护/GO未确认，无新UART。左腕已恢复I0、头部已恢复P16，长期参数未实施，单次功能不证明P因果、即时采用、重复性或掉电持久化。底盘主动运动与零速合同、旧±50仍OPEN，不扫参或加框架。R16及以下原文全部保留，右H2完整PASS、D15 OPEN/DEFERRED、deployable=false保持。

### 头1临时P32负向单次往返功能通过（客户端本地2026-10-11，R16已收尾）

本次独立GO仅执行一次left7 P16→32→16、−24往返，D43/I0不写。锚2132、目标2108；出程14样本，190.781587ms首次进入±4，末2112/+4，观察结束800.063911ms不延长原800ms资格期限。回程3样本2128/−4于130.582209ms通过。实际负向位移20，原±4/800ms、±32/min12保持；各样本与原始READ输出相同，反馈未镜像。HEAD1_REVERSE_SINGLE_FUNCTION_PASS与原整轮FAIL分开：整体仍为BASE_FIRMWARE_CONTRACT_UNVERIFIED，三轮DEFERRED/NOT_RUN。

七次具名写仅left7：P32、GP2132、TE1、GP2108、GP2132、TE0、P16。P16/D43/I0/Lock1/CW1/CCW1恢复，STOP51均整数零、82项身份及独立17项非UART清场/23child通过，manual_power_cut_required=false。383直属child与766原输出逐SHA/尺寸核验并自然回收；351 exit0、32空fuser/sudo fuser为预期idle exit1，不称全部exit0。driver651861经同tmux@84/%84观察自然dead/status1后仅回收自有pane，不冒充root直接waitpid远端driver。本轮GO已消耗、看护已结束；供电仅用户报告ON，独立电气遥测UNKNOWN。

p_trial保存29个raw字段，实际read-raw调用30次：Lock恢复前后各一次，两份原输出均保留，不是动作重试。右8/9前后四次普通PV READ的完整ping/model/PV原帧均核验为正零，不解释旧±50或替代物理轮静止证明。50refs收集前后完整stat一致；入口binding中33项完整stat、17项历史path/SHA/bytes，不回填为全部入口已有完整stat。run中四条普通READ附加ordinary_read_trace元数据，journal没有该字段；共同字段一致，不称两表逐字相同，原PENDING_ROOT_REVIEW_OF_COMPLETE_STRACE未改。独立硬件验收报告9477B已通过。

质量依据root-actual-quality-review-r16.json：Orin在nightly080cc27146389dbb5c8772c476b1937be9c6110f下实际fmt于2026-10-12 03:26:08.822277→12.345701UTC、随后根Clippy于03:26:12.568688→13.404522UTC零警告通过。11质量child/22原流及53质量原件验收通过；597输入主树与原生前/after-fmt/后三窗逐项一致，聚合068277d843b191bc8283bab38e667b0f6a4d30e296e8c5a99a97cfe19654fcf8。native HEAD894加五dirty，不称Git对齐或clean。没有新Rust测试、release/控制ELF构建、正式安装或服务激活。永久入口：[R16摘要](../../.verify/position_return_2026-10-11_r16/summary.json)，83件逐字复制与manifest完整覆盖已由root验收；硬件808件、软件准备39件、质量53件各保留唯一原tar。

R13正向与R16负向各一次当前起点功能通过，不证明同一姿态重复性、P因果、PID即时采用、长期参数或掉电持久化。左腕临时I1已恢复I0、头部临时P32已恢复P16，长期参数未实施；底盘三轮主动运动、Mode1/FW3.10/Phase12零义与旧±50原因仍未闭环。下一步只补头2P32−24同类单次，R17本地候选开发中，尚无新GO。R15及以下HANDOFF原文全部保留，右H2完整PASS、D15 OPEN/DEFERRED、deployable=false保持。

### 底盘右7/右8六项固件原值只读通过（客户端本地2026-10-11，R15已收尾）

本次独立新现场GO仅覆盖right7、right8各Firmware_Major_Version@0、Firmware_Minor_Version@1、Phase@18，全部1B，各一次。实际六项均通过：两轮均FW3.10、Phase12；六项SYNC_READ请求及完整响应的ID、地址、宽度、status、checksum、payload和unsigned原值逐项核验。复用冻结13c原帧工具，没有新ELF；寄存器写、Enable、动作均0，没有自动UART重试。本次没有完整17电机STOP51，不能把六项只读、既有停止反馈或非UART清场当作物理停机证明，也不能据此确认启扭时零目标的固件语义。

156个直属child与312份原输出逐项核验并自然回收：116 exit0、40空fuser/sudo fuser为预期idle exit1，不称全部exit0。末5项非UART后验及独立17项清场/23个child通过。driver646895由同tmux@81/%81观察自然dead/status0后，仅回收该自有pane，不冒充root直接waitpid远端driver。2026-10-12 02:34UTC已完成收尾（客户端本地日期2026-10-11）；本轮GO已消耗、看护已结束，供电仅USER_REPORTED_ON、独立电气遥测UNKNOWN，无新UART或动作GO。

软件准备两端各8个不同内存方法通过，同名方法不累计为16；默认PLAN exit0、缺GO预期exit2、完整PLAN及原生51 refs通过。原生准备的5个软件child、17项非UART/23个child已验收；这些软件事实不替代真实六项采集。必要原件入口：[R15摘要](../../.verify/base_firmware_read_2026-10-11_r15/summary.json)，66件永久归档覆盖与原件字节相等已由root验收；软件33件、硬件324件、质量69件各保留唯一原tar。首未执行starter旧run-id/status修r2，最终config独立新文件，原稿与diff保留，实际只启动一次；首quality AST解析假设错误在本机审查修正，无额外SSH/Cargo或原件改动。

提交前两端nightly实际更新6f5→080cc27146389dbb5c8772c476b1937be9c6110f。Orin通过指定10.0.1.80:10808代理更新，update PID647215于02:42:03.485508→26.887474UTC自然exit0/reap；新版唯一fmt PID647270于02:42:27.425930→30.949027、根Clippy PID647281于02:42:31.170137→02:47:00.218648UTC零警告通过。11 Runner child及独立update child共12直属、24原流逐SHA/尺寸核验并自然回收，owner79416/SSH79417及同SSH四child自然收尾；driver647199在@82/%82自然dead/status0后只回收所属pane。597源码主树与原生前/after-fmt/后三窗逐项一致，聚合068277d8，实际080编译器首尾一致；native HEAD894加五dirty，不称Git对齐。root另实际新版fmt退出0；旧6f5 fmt不冒充新版检查。没有新Rust测试、release/控制ELF构建、UART/GO、正式安装或服务激活；r2质量运输草稿未执行，实际仅已审f4 recipe一次。

厂家资料窄核仍未明确Mode1/FW3.10/Phase12组合的零目标语义：厂家表bit3和地址46没有Mode0限定，官方Mode1零停示例没有Phase/FW适用声明；不能据此判现场必然最高速或已经可靠零停。底盘零速合同仍OPEN，不盲改Phase、不放宽原停止门、不自动驱动三轮。R14/R13头部P32各仅单次当前姿态功能PASS，收尾已恢复P16，不证明长期参数、重复性或掉电持久化；左腕I1单次通过后已恢复I0，长期左腕和头部参数、三轮主动运动未闭环。旧±50原因OPEN、原FAIL不回填，右H2完整PASS、D15 OPEN/DEFERRED、deployable=false保持。R14及以下历史原文继续保留。

### 头2临时P32单次往返功能通过（2026-10-11，R14已验收收尾）

本次独立GO仅执行一次left8 P16→32→16、+24往返，I0/D43不写。锚2086、目标2110；出程14样本，131.118331ms首次进入±4，末2107/-3于801.213057ms读完，原±4资格仍须在800ms内，未延长判据。回程3样本，2088/+2于138.449869ms通过。P16/D43/I0/Lock1/CW1/CCW1恢复，七次具名写仅left8：P32、GP2086、TE1、GP2110、GP2086、TE0、P16；配置区实际临时写入，不称EEPROM写0。HEAD2_SINGLE_FUNCTION_PASS与原整轮FAIL分开，BASE_FIRMWARE_CONTRACT_UNVERIFIED及三轮DEFERRED/NOT_RUN保持。

STOP51均整数零、82项身份、50个bound refs前后一致及独立非UART17项清场通过，manual_power_cut_required=false。50项实际收集前后完整stat一致；入口binding中33项完整stat、17项历史path/SHA/bytes，不回填成全部入口已绑定完整stat。383直属child/766原输出、独立清场23个child均自然回收；p_trial保存29个raw字段，实际read-raw调用30次，包含Lock恢复前后各一次，不称30个新测量字段或重试。右8/9前后四次普通PV READ原帧均为零，不解释旧±50或替代物理轮静止证明。driver642733经同tmux@78/%78观察自然dead/status1后仅回收该自有pane；本轮看护已通知结束，GO已消耗，供电仅用户报告ON、独立电气遥测UNKNOWN。

Orin唯一一次实际fmt于04:28:50.960902→54.448085UTC、根Clippy于04:28:54.671883→55.447179UTC零警告通过；10质量child/20原输出及同SSH四个child自然回收，owner38089/SSH38090自然exit0/reap，driver643297的@79/%79自然dead/status0后仅回收所属pane。主树前后及原生前/after-fmt/后三窗597输入逐项相同，聚合068277d8，已冻结最新nightly6f5首尾同；native HEAD894加五处dirty，不称Git对齐或clean。root实际fmt退出0并复核597项与原生逐项相同。没有新Rust测试、release/控制ELF构建、nightly更新、正式安装或服务激活。必要原件入口：[R14摘要](../../.verify/position_return_2026-10-11_r14/summary.json)，84件永久归档覆盖与原件字节相等已由root验收，软件准备、硬件808件、质量61件各保留唯一原tar。原准备FAIL、本地派生解析FAIL及质量review路径前置拒绝保留；后二者发生于SSH/Cargo之前。首未执行collector草稿误假定probes为1，实际为位置1加三轮DEFERRED；修r2仅收同一run，没有新UART、动作重试或质量重跑。审查脚本字段及正则假设错误另存，不改原件或重采。

一次当前姿态功能通过，收尾已恢复P16，不证明P因果、PID即时采用、重复性、长期参数或掉电持久化；R13及更早记录在下方原样保留。长期左腕和头部参数、三轮主动运动未闭环，旧±50 OPEN、右H2完整PASS、D15 OPEN/DEFERRED、deployable=false保持。下一步仅right7/right8各Firmware_Major_Version、Firmware_Minor_Version、Phase共6项只读；目前软件准备进行中，无新GO/UART。

### 头1临时P32单次往返功能通过（2026-10-11，R13已收尾）

root于03:54:50.976641UTC记录人类“现场仍就绪，可执行本次 P=32 往返”，签独立600秒GO后仅执行一次left7 P16→32→16、+24往返，I0/D43不写。锚2128、目标2152；出程14样本，130.620547ms首次进入±4，继续观察至同一原窗口800.065599ms，末2150/-2 PASS；回程4样本，2132/+4于190.561352ms通过。原±4/800ms、±32及最小12保持，没有目标补偿或精度门续期。P32写ACK和原值32、P16恢复及D43/I0/Lock1/CW1/CCW1读回通过；仅七次具名写入P32、GP2128、TE1、GP2152、GP2128、TE0、P16，均为left7。配置区实际临时写入，不称EEPROM写0；未写Lock、保存、Phase、死区或底盘。

HEAD1_SINGLE_FUNCTION_PASS与原整轮FAIL分别记录：原run779e0942/546834B仍为BASE_FIRMWARE_CONTRACT_UNVERIFIED，三轮DEFERRED/NOT_RUN，不改为整体PASS。STOP51均整数零，79项身份、独立17项非UART清场及47个bound refs完整前后身份一致；manual_power_cut_required=false。384直属child及768原输出逐字核验，352 exit0、32空fuser/sudo fuser预期idle exit1，全部自然回收。29项具名raw TX/RX和右8/9前后四次普通PV READ的完整原帧独核均匹配，四次PV原字均0，不解释旧±50或替代物理轮静止证明。driver641514经同tmux@74/%74观察自然dead/status1后仅回收该自有pane；本机启动owner28274/SSH28275、观察owner28289/SSH28290真实wait/reap，不冒充root直接waitpid远端driver。

必要原件入口：[R13摘要](../../.verify/position_return_2026-10-11_r13/summary.json)，软件准备、实际动作、质量和首质量前置失败各保留唯一原tar，不重复展开。两端各10不同内存方法、默认零动作、缺GO预期exit2、完整PLAN和原生非UART17项通过。首collector误用旧报告字段在清场/独立17项之前拒绝，修字段后仅收同一原run，未重启动作；首质量脚本错名清单在Runner/Cargo之前拒绝，后续实际fmt→根Clippy仅执行一次。原FAIL及本地展示解析错误保留，不称单轮全过。

提交前Orin实际fmt于04:05:03.659791→07.183856UTC、根Clippy于04:05:07.404375→08.184472UTC零警告通过；10质量child/20输出自然回收。597输入主树及原生前/after-fmt/后三窗逐项一致，聚合068277d8，nightly6f5首尾相同；native HEAD894加五处dirty，不称Git对齐。root另实际fmt退出0并复核同597。未新增Rust测试、release/控制ELF构建、正式安装或服务激活；实机沿历史冻结f201位置和13c参数工具，完整身份前后相同。

本轮GO已消耗，看护已通知结束，最新供电仅本轮用户报告ON、独立电气遥测UNKNOWN。一次当前姿态P32功能通过，收尾已恢复P16，不证明P因果、PID即时采用、重复性、长期参数或掉电持久化；R11及更早回程FAIL完整保留。下一步仅头2left8同类P16→32→16、I0/D43不写、一次+24往返，原门与停止链不变；R14本机/Orin软件准备通过，完整PLAN/starter待root审查及新的当次现场确认，尚无GO/UART。左腕长期参数、头2及三轮主动运动未闭环；右H2完整PASS、D15 OPEN/DEFERRED、deployable=false及旧±50 OPEN保持。

### 三处关节12项运行限制实机只读通过（2026-10-11，R12已收尾）

root于03:37:33.720347UTC记录本会话直接人类答复“现场仍就绪，可执行本次12项只读采集”，仅签固定left5/7/8各TE/PV、Acceleration41/1B、Torque_Limit48/2B各一次的600秒GO。03:41:04.021→03:41:11.234UTC实机12项均通过：三处各TE=0、PV=0、Acceleration=0、Torque_Limit=1000。工具实际捕获请求SYNC_READ和完整响应，12帧ID/地址/宽度/status/checksum/payload与unsigned值逐项独核；请求外握手不冒充只发生12个TTY帧。只报告原值，现场固件单位与控制环即时采用仍未另证；未见低扭矩限制设定，不据此宣称已关闭摩擦或PID根因。

run99c56522/420119B保持PASS_READONLY12_CAPTURE，270直属child/540输出逐项字节与SHA通过并自然回收；206 exit0、64空fuser/sudo fuser为预期idle exit1，不称全部exit0。末五项非UART后验、独立17项清场通过，manual_power_cut_requested=false。driver640784由tmux@71/%71观察自然dead/exit0后仅回收自有pane；启动owner24234/SSH24235及观察owner24251/SSH24252均真wait0/reap，不冒充root直接waitpid远端driver。全程无参数写入、Enable、动作或底盘命令，未做本轮17电机STOP51，不把三目标TE/PV零读回扩成整机物理停止证明。

必要原件入口：[R12硬件摘要](../../.verify/position_return_2026-10-11_r12/summary.json)，575件原tar唯一保留、不重复展开，现场、GO、最终config/PLAN及软件准备原FAIL另存。首native配置帮助脚本tuple/list比对FAIL发生在已保存原config、上传部分原件之后，未替换finalconfig/启动PLAN/UART；仅修序列化比较后续完。root首ownedSSH帮助脚本因相对input路径前置拒绝，child=null/SSH=0，保留原FAIL；随后使用绝对路径只启动一次实际采集，不算硬件重试。

本轮看护已通知结束，最新供电仅本轮用户报告ON，独立电气遥测UNKNOWN；GO已消耗，无新UART或动作授权。回程精度、旧±50、D15 OPEN/DEFERRED、deployable=false及右H2完整PASS保持。下一步仅准备头1 P16→32→16、I0/D43不写、一次+24往返，复用原800ms出程观察和±4/800ms、±32/min12、STOP51门；P24首次出程190ms为不同起点功能事实，不据此证明P已采用或因果，不保证P32通过。左腕长期参数、头2与三轮主动运动仍未闭环。

### 回程诊断增加两个运行参数读取（2026-10-11，R12软件已验收，硬件未执行）

仅增加Acceleration@41/1B、Torque_Limit@48/2B的具名unsigned原值读取，两字段按厂家表属于SRAM；本项目继续将其归为诊断只读。既有四个写API、共用绝对期限写准备入口及poke打开串口前均拒绝写入，缺值或坏值也不能绕过。新增原帧golden和高位保留检查；未加入单位换算、目标补偿或新的控制框架，S0/S1控制路径不变。

本机与Orin各218库方法+13工具方法=231不同方法通过，重命名及参数展开不另计。Orin实际fmt于03:19:35.487→39.010UTC、默认根Clippy于03:19:39.228→03:24:09.023UTC、real-hw根Clippy于03:24:09.044→56.094UTC零警告通过；独立poke release、help及两个新名称的预开串口拒写均完成，CLI预期exit2。597输入主树与原生各源码窗口逐项一致，聚合068277d8，编译器最新nightly6f5首尾一致。native HEAD894加五处dirty，与本机62a4096不同，不称Git对齐或clean；17 Runner、3 CLI及4 same-SSH child自然回收，owner14804/SSH14805自然0/reap，tmux自有dead pane收尾。

新ELF15ab06ca/1511200B仅冻结为独立诊断工具，不替换f201位置控制、13c旧参数读回或正式ca14。工具链更新及质量两份原tar保留原字节，不重复展开。软件入口：[R12摘要](../../.verify/feetech_runtime_limits_2026-10-11/summary.json)；首本地生成与远端目录前置FAIL发生在实际update/tmux之前，保留原件，不算硬件重试。

厂家协议与官方SDK表明普通WRITE不要求ACTION，Lock1为掉电不保存语义；公开资料没有证明现场FW3.10何时把PID写入采用至控制环，ACK/原帧不能补此证明。下一步只读左腕、头1、头2各TE/PV、Acceleration和Torque_Limit共12项，补齐此前未读的运行限制事实。固定12只读候选仅绑定新诊断ELF后，本机与Orin各9内存方法、默认零动作、缺GO预期exit2及原生完整PLAN通过，17非UART检查与历史完整USB/protection一致；候选827e8337、绑定d6660df1、未授权配置3dbda683。42件原生准备原tar与真实自然回收记录保留，GO与现场引用仍为空。root已询问当次现场状态，尚未收到答复。本轮未签GO、未UART、未做硬件动作、全workspace测试、RT/H2、控制ELF构建、正式安装或服务激活。R11回程+6及旧±50原FAIL保持，旧看护已结束，新的现场条件尚未确认；左腕长期参数、头1/头2精度及三轮主动运动仍未闭环，D15 OPEN/DEFERRED、deployable=false、右H2完整PASS保持。

### 头1临时D0功能试验仍回程 +6（2026-10-10，R11实机已收尾）

本次独立GO仅允许left7临时D43→0→43与一次+24往返，P16/I0、Phase、死区、Lock均未写，原±4/800ms、±32/min12及17×3停止门保持。候选复用既有R6参数恢复、R8出程观察和d97回程/收尾；本机和Orin各10个不同内存方法、默认零动作及缺GO真实exit2通过，不跨平台累计为20。原本机r1的9PASS/1ERROR是冻结路径命名空间夹具被正确拒绝；原首native FAIL是准备脚本错名旧GO夹具。后续修正仅对应夹具，控制候选未改，原FAIL分别保留，不称单轮全过。

真实锚点2122、目标2146：出程14样本，251.024002ms首次进入±4，继续观察至同一原窗口800.063598ms，末位置2142/-4 PASS。回程14样本，目标2122、末位置2128/+6，原800ms内没有合格样本；末样本792.370511ms，随后原d97的50ms sleep使失败判定完成于842.434602ms，不代表精度门续期，也不称实际恰800ms结束。原整体FAIL与POSITION_ACCURACY_TIMEOUT保持。D0写ACK/原帧0及Lock1通过，finally恢复D43真实读回通过，收尾P16/D43/I0、Lock1、CW/CCW死区1、TE0/PV0。

仅七次left7写入：D0、GP2122、TE1、GP2146、GP2122、TE0、D43；使用原13c参数/raw工具与f201位置工具，底盘写、Load采样、相机和自动重试均0。STOP51解析整数零、76项身份和独立非UART前后各17项通过，manual_power_cut_required=false。394直属child/788实际输出已逐件核SHA/字节并自然wait/reap，30条raw TX/RX独审通过；四次right8/9普通PV strace的24个完整TTY帧均PV0000，不外推STOP51全部都有原帧。driver628379由tmux%67观察自然dead/exit1后回收自有pane，本机owner89725/SSH89727于11:37:54.830→11:38:07.326UTC自然exit0/reap，不冒充root对driver直接waitpid。必要原件：[R11摘要](../../.verify/position_return_2026-10-10_r11/summary.json)；824件硬件原tar及58件质量原tar各保留原字节，不重复展开。

Orin实际cargo fmt --all于11:36:14.321→17.844UTC通过，随后根cargo clippy --all --all-targets -- -D warnings于11:36:18.066→18.838UTC零警告通过，10直属质量child/20实际输出及同SSH四个child自然回收。native HEAD8940847加五处源码dirty，与host c64cfde不同，以597输入主树及原生三窗逐项bytes/SHA/executable一致绑定，聚合3a4cc12e、nightly32d首尾一致；root另即时复算当前主树相同。本轮没有新Rust测试、release/控制ELF构建、正式安装或服务激活；两个本机只读展示/归档KeyError原文本保留，未因此重跑远端门。

厂家表将D22列在EPROM/EEPROM配置区，本轮实际临时写D0及D43，Lock1且没有Lock/保存写入；不能称EEPROM写0或已验证掉电后不持久化。官方已存表第25、41–43、69行说明配置区、系数和保存语义，没有明确证明FW3.10在Lock1写读回后何时进入控制环。正确ACK/原字不是即时采用证明，本次FAIL也不能反证未采用。不同起点/方向只作原精度功能试验，不与R10作D因果比较。下一步先明确参数生效条件，暂不继续盲调PID或扩大设计；左腕仅临时I1 PASS后恢复I0、头2旧FAIL、长期参数及三轮主动运动仍未验收。旧R10/R9/R8/R5/R6及±50原FAIL保持，D15 OPEN/DEFERRED、deployable=false、右H2完整PASS不变。GO已消耗、看护已解除，无新GO/UART；最新供电仅为本轮用户报告ON，独立电气遥测UNKNOWN，正式ca14及六服务沿既有状态，不称新的正式后验。

### 头1负向出程 +8，平台驱动输出原字44（2026-10-10，R10实机已收尾）

root收到本轮“现场仍就绪，可执行本次头1测试”，仅签一次left7原P16/D43/I0负向24尝试与平台最多一次Present_Load原帧。候选最终r3在本机/Orin各14内存方法、默认零动作、缺GO拒绝与完整PLAN准备通过；原r1的12方法PASS、r2的14方法PASS保留，r2随后发现普通Load故障被包装为精度错误的分类问题，经r3修正为ACTION_FAILED后验收，原结果不回填、不累计不同修订的方法数。原±4/800ms、±32保护及STOP51保持，不续期限或改参数；相机未重复采集。

本次锚点2138、目标2114，约190.721ms起停在2122（+8）；14个位置样本未进入原±4门，出程窗口800.059801ms后FAIL，回程NOT_RUN。平台首次符合采集条件时仅执行一次left7地址60/2B read-raw，约出程313.371ms启动、8.378ms返回，原响应`ff ff 07 04 00 2c 00 c8`，ID/长度/status0/checksum与unsigned44一致；夹前后PP均2122/+8。厂家把此字段名义描述为当前驱动占空比，非零只支持该瞬间仍报告驱动输出，不能换算成实测扭矩、确认整个窗口持续驱动，或证明摩擦/齿隙/负载/安装等机械根因，现场FW3.10单位语义未另验。

仅left7四次写GP2138、TE1、GP2114、TE0，PDI、底盘与相机启动均0，无自动重试。最终17电机STOP51整数零、74项身份和独立17项非UART后验通过，manual_power_cut_required=false；STOP读回不替代实际断电事实。348直属child/696输出已核并自然wait/reap，driver627613由tmux观测自然dead/exit1，自有dead pane终态后回收；本机owner83539/SSH83540自然exit0/reap，不冒充root对driver直接waitpid。整体run33c89057/542385B与精度FAIL保持，必要原件：[R10硬件摘要](../../.verify/position_return_2026-10-10_r10/summary.json)；唯一原tar含732件，归档不重复展开或复制ELF。

本轮GO已消耗，root已告本轮看护收尾，不构成再次动作授权。最后供电事实是用户报告已恢复、当次现场持续看护确认，独立电气遥测UNKNOWN；没有因STOP失败要求人工断电。前述“没有卡住”只支持未报告明显硬卡，仍未闭环根因。本次没有新Rust测试/构建、正式安装或服务激活；Present_Load软件验收与本次候选/硬件分包，Rust43件归档保持原字节。R9负向出程+6/回程NOT_RUN、R8回程+7及更早FAIL、旧±50 OPEN均保留；左腕仅临时I1 PASS后恢复I0、头2旧FAIL、长期参数及三轮主动运动尚未验收，D15 OPEN/DEFERRED、deployable=false不变。

### Present_Load只读小件软件验收，真实采集未执行（2026-10-10，R10）

现有表新增Present_Load@60/2B unsigned，仅read/read-raw保留原word；四具名写API及poke在开串口前复用DiagnosticRegisterWriteForbidden。未加入符号metadata、单位换算、EEPROM或位置控制行为。READ1/SYNC3 golden校验分别0xba/0x22；厂家表名义为当前驱动占空比，不等于已验证扭矩或现场FW3.10语义。

本机fmt→crate Clippy、217库与13工具方法通过，实际拒写CLI exit2且自然回收。首分类脚本误要求英文diagnostic，实际中文“本项目诊断专用寄存器禁止写入：Present_Load”正确，原batch FAIL保留；后继reconciliation只解析原stderr并补编译器尾部身份，没有重跑上述门。Orin实际fmt→根Clippy、同217库+13工具、独立release及help/Present_Load写入两个真实ELF预期exit2通过。每平台去重230，不累计为460；24 Runner/48输出、两个真实ELF child、同SSH四个child与本机owner77380/SSH77381均自然wait/reap。nested Cargo PID未导出，无TTY系统调用trace，不宣称系统调用级观测。

两端597输入与原生三窗口逐项相同、聚合3a4cc12e，nightly1.101.0/32d首尾相同；native894加五处源码dirty，与host HEAD不同，以同源窗口绑定。新诊断ELF2949ccb2/1514376B仅独立诊断，不替换冻结f201位置控制或13c原始读回工具；本批没有完整workspace测试、RT/H2、正式安装、服务激活或新的正式四件后验。root第一次本地归档窗口名解析失败与本机摘要KeyError均为本机只读解读错误，原FAIL/工具输出保留，未重跑远端recipe。必要原件：[R10只读软件摘要](../../.verify/present_load_read_2026-10-10_r10/summary.json)；唯一136件原tar、本机原FAIL/reconciliation、owner原三流和现场回复均保留，不重复展开或复制ELF。

现场对机械检查请求回复“没有卡住”，只支持未报告明显硬卡；未提供实际断电确认，不能排除摩擦、齿隙、负载或证明根因。随后回复“刚插上的电源”，最新仅为用户报告恢复供电，独立电气遥测UNKNOWN，不能替代新的持续看护确认或动作授权。R10薄候选已完成两端14方法与默认零动作/缺GO及PLAN准备，另属动作准备，不计入本Rust软件包；本软件阶段记录时，新现场看护问询仍待答，尚无新GO、真实Load/UART读取或动作；其后的独立R10实机事实见上方最新阶段。原P16/D43/I0保持，D0对照未执行；原±4/800ms和STOP51未改。R9出程+6 FAIL/回程NOT_RUN、R8回程+7、R5/I2与R6/P24回程+6、旧±50 OPEN保持；左腕仅临时I1 PASS后恢复I0、头2旧FAIL、长期参数与底盘主动运动未验，D15 OPEN/DEFERRED、deployable=false不变。

### 头1反向出程 +6，同期图像支持实动（2026-10-10，R9已收尾）

用户确认照明和背景静止后，三路相机短时取图通过；录图期间原17项非UART门通过。随后收到本轮“现场仍就绪，可执行本次往返并同步拍摄”，root于09:49:03.426329UTC记录当前现场事实，只签一次原P16/I0头1负向24动作及独立三路录图。候选仅适配负向目标范围和最小实际行程，六个内存方法、默认零动作及缺GO拒绝通过；原±4/800ms、±32保护、停止和身份门保持。

锚点2668、目标2644，320.061097ms起连续停在2650（+6），原800ms内从未合格；末采样于802.327053ms完成。出程FAIL后没有执行回程，不能称往返完成。仅left7四次写入GP2668、TE1、GP2644、TE0，PDI和底盘写入均0；STOP51整数零、72项身份及独立非UART后验通过，无停止失败导致的人工断电要求。347个直属child及694原输出已核，driver625927/%62自然exit1、相机supervisor625880/%61自然exit0，SSH64403自然exit0回收，两自有dead pane已清理；无重试或终态后UART。

三路相机在独立进程中按原格式和帧率录图，201/201/601有效帧，约20秒完整覆盖本次动作；取图、编码均未进入运动采样窗口。六张图片分别选首次TE1前、出程末采样前的最后有效帧，D435对应时间差11.906/27.411ms，两USB各46.203/99.232ms、88.825/42.323ms；回程图明确NOT_RUN。root实际查看原图：D435中的桌沿、沙发和柱子整体移位，两USB背景基本不变，与头1编码器2668→2650一致，支持本次发生真实运动。相机安装位置尚未人工确认，图片不能测出精确6个计数误差，也不能判定线束、壳体、支撑、摩擦或齿隙的根因。完整录像raw约102.94MB暂留Orin，永久入口保留六图、原始时间元数据和必要日志：[R9摘要](../../.verify/position_return_2026-10-10_r9/summary.json)。

R8正向出程-4、负向回程+7与R9负向出程+6共同支持方向相关的残余误差，问题不只发生在折返；暂不继续盲调P/I或放宽判据。已请求现场切断两个电机电源、保持支撑并近距离检查头1线束/壳体/支撑是否接触或拉紧，不强扳关节；断电及所见报告待答。最后供电事实仍为本轮用户确认ON，独立电气遥测UNKNOWN，本轮动作GO已消耗。

提交前主树实际fmt退出0；Orin实际fmt09:56:05→08UTC、随后根Clippy09:56:08→09UTC零警告通过。两端及原生三窗口597输入同913f4353、编译器nightly32d首尾相同，10质量child/20输出已核并自然回收。原生仍为894加五处源码修改，不贴当前文档HEAD；没有新Rust控制产物、测试重跑、release、正式安装或服务激活。R8及更早精度FAIL、旧±50 OPEN保持；左腕I1仅临时PASS且恢复I0，头2旧FAIL、长期参数和底盘主动运动尚未闭环，D15 OPEN/DEFERRED、deployable=false保持。

### 头1延后折返仍回程 +7（2026-10-10，R8实机已收尾）

root于09:10:42UTC收到“现场仍就绪，可执行本次头1往返”，只执行一次left7原P16/I0的+24往返。出程第一次进入原±4容差后继续观察至同一个800ms期限，再使用未改的回程判据；候选六个内存方法、默认零动作及缺GO拒绝已软件通过。没有扩大行程、延长期限或增加PDI参数写入。

锚点2661、出程目标2685，251.205164ms首次到2681（-4），随后10个连续样本仍为2681，观察总时长800.063244ms。回程目标与读回均2661，190.823823ms起停在2668（+7），期限内10个连续样本为+7，末次采样完成于801.146ms；原±4/800ms精度FAIL保持。延后折返没有消除偏差，不能再将提前折返视为充分解释；目前没有证据确认摩擦、负载或固件策略中的哪一项是根因。

仅left7五次Goal_Position/Torque_Enable写入，PDI和底盘写入均0。STOP51整数零、身份及独立非UART后验通过，无人工断电要求；右8/9初始及收尾四次普通READ的PV原帧均0000，非全51项原帧，不能解释历史±50。363个直属child及726原输出已核，driver624741/%59自然exit1、观察SSH55539自然exit0回收，自有dead pane已移除。原run08a3a5cd/544340B保留整体FAIL，必要原件见[R8摘要](../../.verify/position_return_2026-10-10_r8/summary.json)。本轮动作授权已消耗，无自动重试或后续UART；现场供电最后为用户确认ON，独立电气遥测UNKNOWN。

提交前主树实际fmt退出0；同597输入的Orin实际fmt09:15:07→11UTC、根Clippy09:15:11→13UTC零警告通过。三份完整源清单与主树逐项一致、聚合913f4353，编译器32d首尾一致；10个质量child及20输出自然回收并核验。本批没有新Rust测试、release构建、正式安装或服务激活。下一步先核现场头部线束牵拉、壳体接触或支撑施力，不新增Current/Load框架或盲调P/I。左腕I1仅临时PASS且恢复I0、头2旧FAIL与底盘主动运动待验保持，D15 OPEN/DEFERRED、deployable=false不变。

### 左腕与头部15项参数原帧只读通过（2026-10-10，R7实机）

收到新现场答复后，root签独立15项只读GO；仅新562诊断ELF各一次读取left5/7/8的TE/PV和地址24/25/30，不调用f201、不写寄存器或启用电机。六个TE/PV原word均零，九个参数原word各目标均16/0/1；15请求/响应ID、地址、宽度、error、checksum与原stdout一致。三参数没有目标间差异，不能解释头1+6或证明回程已修复；厂家表积分限制0关闭限幅，不代表关闭积分控制。

run4a85ec35/542801B保持PASS_READONLY15_CAPTURE，327个直属child、654原输出已核，非UART后验及独立17门通过，driver624034/%56自然exit0、SSH52565真实回收，dead pane已回收。必要原件见[15项只读摘要](../../.verify/position_return_2026-10-10_r7/summary.json)。此结果不是17电机停止验收，不覆盖旧±50或精度FAIL；无正式安装、长期参数写入或底盘动作。本轮现场授权已消耗。下一步只针对头1在P16/I0下保持出程观察至原800ms窗口后折返，核对“首次进入容差即反向”是否相关；+24/±4/每段800ms保持，尚未执行或签新动作GO，D15 OPEN/DEFERRED、deployable=false不变。

### 回程诊断增加三个只读原字段，Orin软件验收通过（2026-10-10，R7）

头1I1/I2/P24的回程均停在+6，继续增大P/I缺少依据。仅补现有控制表的Minimum_Starting_Torque@24、Integral_Limit@25、Angle_Resolution@30，均1B unsigned；复用诊断拒写门，四具名写API及poke开串口前拒写，普通read/read-raw保留完整原字节。厂家名义值只作字段说明，不换算或保证现场语义；积分限制值0表示关闭限制功能，不等于关闭积分控制。未修改Phase、PDI控制行为、精度或停止门，未新增框架。

本机真实nightly32d、fmt→定向Clippy、215库与13工具方法通过，首轮测试函数too_many_lines原FAIL保留。Orin独立源为894加五处修改，主树effe与原生597输入逐项相同、聚合913f4353，三份完整首尾窗口及实际fmt→根Clippy通过；default/real-hw库各215重叠，工具13，去重228。独立release诊断ELF5621358b/1514432B，help及三名拒写均真实exit2；28个Runner child和四个实际ELF child自然回收，SSH与dead pane已收尾。必要原件在[只读参数软件摘要](../../.verify/feetech_return_metadata_2026-10-10/summary.json)；两个本地归档解析KeyError保留，均未改或重跑远端recipe。本批无UART、硬件动作、完整workspace测试、H2或正式安装。下一步仅左腕/头1/头2固定15项只读采集，尚无新GO；原精度FAIL、旧±50 OPEN、D15 OPEN/DEFERRED、deployable=false保持。

### 头1临时 I2 往返仍回程 +6（2026-10-10，R5）

当次现场确认和独立GO后，仅执行一次left7临时I2、+24往返并恢复I0。锚点2648、目标2672，出程2668（-4/251.226524ms）PASS；回程800ms内8个样本停在2654（+6），原±4门FAIL。I0/P16/Lock1/D43/CW1/CCW1恢复、STOP51整数零、身份和非UART后验PASS，384个直属child及768份输出原件已核。右8/9初始及停止四个普通READ PV响应原帧均为0，完整ID/LEN/checksum/bit15与stdout一致，不能称全51停止字段都有原帧；旧±50原因仍OPEN，不回填旧FAIL或推断物理轮运动。底盘主动写0，无重试或人工断电要求；原run整体FAIL保持。[R5必要原件与摘要](../../.verify/position_return_2026-10-10_r5/summary.json)。

### 头1临时 P24 对照仍回程 +6（2026-10-10，R6）

新现场确认和独立GO后，仅执行一次left7在I0下P16→24、+24往返并恢复P16。锚点2655、目标2679，出程2675（-4/190.497993ms）PASS；回程800ms内13个有效样本中11个停在2661（+6），仍FAIL。P16/I0/Lock1/D43/CW1/CCW1恢复、STOP51整数零、身份和非UART后验PASS，383个直属child及766份输出原件已核；7次写仅left7，底盘主动写0，无重试或人工断电要求。[R6必要原件与摘要](../../.verify/position_return_2026-10-10_r6/summary.json)保留整体FAIL。头1I1/I2/P24回程均+6；R5/R6合计8个右8/9 PV原帧零不解释旧±50。左腕I1仅临时PASS且已恢复I0，头2仍旧FAIL，长期参数未设置、底盘动作未验，D15 OPEN/DEFERRED、deployable=false保持。两轮未新建Rust控制产物；R6收尾时地址24/25/30只读工具尚在实现，后续软件验收见上方R7。本次root已核两轮原件、独审及R7同源Orin fmt→根Clippy；尚未执行15项参数实机采集。

### 右8、右9普通 READ 原帧通过，旧异常原因仍待闭环（2026-10-10）

用户先确认已恢复供电、支撑和持续看护就绪；两个软件入口分别因启动稿 SyntaxError、必要引用漏列而拒绝，均在实际 UART 前退出，原 FAIL 保留。准备耗过原600秒现场门后，root于07:59:01UTC记录用户“现场仍就绪，可继续看护并立即断电”，重新绑定当次回执，不续写旧时间、不要求额外电源复位或USB重插。

本次仅右8→右9各一次冻结f201普通READ速度，strace保存相同opcode的真实收发。每个CLI原有PING、READ型号、READ PV，共6请求，无寄存器写、Enable或运动；两PV响应分别为`ff ff 08 04 00 00 00 f3`、`ff ff 09 04 00 00 00 f2`，原word0000、bit15解码0与stdout一致。完整TTY帧ID、长度、实际IO字节数及checksum通过，型号均777、error0；状态响应自身不含请求地址/opcode/代次，只按本次紧随收发顺序关联，不解释历史±50。

driver619231由tmux记录自然exit0，80个直接child及160输出原件已root逐项核，直属strace自然回收；nested f201没有独立wait导出。六项非UART后验通过，SSH30432自然exit0/reap，自有dead pane后回收。只是两次只读采集通过，不是17电机停止验收，旧±50/NEEDS及精度FAIL保持；现场无法确认旧轮运动。本次复用历史控制ELF，没有新Rust产物、正式安装或服务激活。仅安装strace5.16并用普通uid1000执行无UART的true探针，未做系统整体升级。

必要原件入口 [.verify/position_return_2026-10-10_r4](../../.verify/position_return_2026-10-10_r4/summary.json)，原采集、入口失败、strace安装三包与小回执共67文件/494849B，未重复保存183文件完整解压树。READ正常结束不推定断电或解除看护，也不构成新动作GO。下一头1临时I2一次往返现场确认待答；候选只补存既有右8/9 PV读取的原帧，不增加UART次数，原±4/800ms、恢复I0及17×3停止门保持。左腕I1单次通过但已恢复I0，长期参数未设；头1/头2精度与底盘主动运动仍待闭环，D15 OPEN/DEFERRED、deployable=false不变。

提交前Orin实际更新浮动nightly仍为32d，08:08:35→38UTC执行fmt，随后08:08:39→40UTC根Clippy零警告通过。19个不同直接child与38输出原件自然回收/逐项核验；原生894与本机76b提交身份不同，597输入两端及格式化前后同5970c9be，主树当下逐项复核一致。原件在 [.verify/position_return_2026-10-10_r4_quality](../../.verify/position_return_2026-10-10_r4_quality/root_review.json)，首次本地解析键名KeyError保留，只修只读解析，未远端重跑；本批无新Rust测试或release构建。

### 头1临时 I1 回程失败，头2出程及整机停止失败（2026-10-10，本轮已收尾，用户确认断电）

root于07:16:21UTC记录用户“现场仍就绪，可以继续看护”，本次明确覆盖头1一次I1恢复0及头2一次原参数往返，底盘不动作。头1候选仅将已验left5逻辑固定到left7、实测19项备份与motor7原帧/ACK身份；最终3c44源的8个内存方法与默认零动作已root实际复验，不将同方法重复累计。

头1锚点2642、目标2666，出程最后2662（-4/250.466292ms）通过；回程目标2642，期限内10个样本停在2648（+6），原±4/800ms判据FAIL。I1写/读回、I0恢复及Lock1/P16/D43/双死区1全部通过，17×3停止51项整数零、身份及独立非UART后验通过。driver617600及observer自然exit1/reap，385个不同直接child与770输出原件已核，原精度FAIL不覆盖。

root在头1安全收尾与后验通过后，沿本次覆盖两目标的现场事实签了独立head2原参数GO，无头1重试。头2仅4次left8写：GP2087、TE1、GP2111、TE0；出程目标2111但平台2106，-5超门，回程NOT_RUN，没有I/Phase/底盘写。最终51停止字段中right8 Present_Velocity=-50、right9=+50，二者TE0/GV0，其余整数零；原NEEDS_MANUAL_POWER_CUT与STOP FAIL保持。异常速度只有f201普通读解析整数，没有对应原UART帧；不因编码器读值相同或只差1就推断实际轮静止/转动、单位或根因。driver618150和observer自然exit1/reap，310不同直接child及620输出原件已核；非UART后验通过，所有自有pane确认自然dead/exit后回收、SSH全部回收，不以这些PASS覆盖停止FAIL。

原失败收尾时已要求现场立即切断两个电机电源并保持支撑，原件确认PENDING不回填。root随后收到用户“两个电机电源已切断，机械臂保持支撑”，记录于2026-10-10T07:29:36.929775+00:00；最新供电USER_CONFIRMED_OFF，实际切断时刻/独立电气遥测UNKNOWN，本轮看护结束。这不是新GO；停止所有后续UART、动作或自动重试。头1固定临时I2候选fa97仅本地软件准备，8个内存方法、默认零动作和0/2写白名单mock通过，未上传或签GO/执行。左腕I1单次精度已通过但恢复I0，长期参数仍未设置；头部精度未闭环，三轮主动运动仍未验，D15 OPEN/DEFERRED、deployable=false保持。证据入口 [.verify/position_return_2026-10-10_r3](../../.verify/position_return_2026-10-10_r3/summary.json)。

本次提交前质量门与r2共用同597输入：Orin实际更新nightly后仍32d，fmt07:14:17→21UTC、根Clippy07:14:21→22UTC零警告，四源窗/两端逐项一致、19直接child及38输出原件已root核。native源HEAD894和hostHEAD406不同，Rust输入聚合相同5970c9be；后续只有本轮Python候选/原件及文档变化，Rust源不变。没有全workspace测试、新release、正式安装或服务激活。


### 左腕临时 I1 对照通过，长期参数与头部仍待验证（2026-10-10）

用户先确认两个电机电源已切断并保持支撑，随后确认“已恢复供电，所有现场条件就绪”；root于07:03:54UTC记录本次供电、空载支撑、活动区空旷、支撑不挡回程、持续看护与立即人工断电事实。首次非UART入口因左USB的device/link inode变化拒绝，UART启动0；核得序列号5A7C121427、devpath、ttyACM0与rdev一致后，按root明确授权使用新身份绑定，不要求再次重插。旧None现场负控和该入口FAIL均保留。

只执行一次35项原帧诊断：17电机Torque_Enable均为0，right7读到777/FW3.10/Mode1/Phase12，Goal_Velocity和Present_Velocity为0；五组顺序位置/速度原帧均为358/0，35个工具child自然exit0/reap。这里只证明当次采集通过，不能覆盖旧right7解析整数+50的停止FAIL、补造其原UART帧，或证明实际轮不动。

随后仅执行一次left5临时I1对照，候选fbac与维护d97、原+24/±4/800ms位置门保持。锚点973、出程目标997，最后993，误差-4/499.499503ms；回程目标973，最后976，误差+3/491.765066ms。两方向、I恢复、17×3停止、身份后验及独立非UART清场均通过。I1写ACK及原帧读回1/Lock1，恢复I0写ACK及原帧读回0/Lock1/P16/D43/CW1/CCW1；7次写仅left5，384个不同直接child自然回收，无后续head/base UART。停止51项是f201普通读的整数零，不宣称51项原帧。

driver616776和observer自然exit1回收；整轮run仍因三轮BASE_FIRMWARE_CONTRACT_UNVERIFIED/DEFERRED保留FAIL，不改为整体PASS。本轮没有设置长期I1，当前已恢复I0；单次对照通过不等于断电后配置或长期精度已闭环。正式ca14四件、六停止服务、两USB及12个保护文件保持，底盘未驱动。证据入口 [.verify/position_return_2026-10-10_r2](../../.verify/position_return_2026-10-10_r2/summary.json)。该左腕阶段结束时，头1I1恢复0候选与头2原参数检查正在准备，后续新现场答复与两个独立GO见上述头部阶段；D15 OPEN/DEFERRED、deployable=false保持。


### 左腕原参数出程与整机停止失败（2026-10-10，本轮已收尾，用户确认断电）

root于06:17:24UTC收到用户“已供电并就绪，支撑不阻挡回程”；本次仅授权左腕及头部的小幅位置测试，底盘暂不动作。维护GO改为当次支撑、空载、活动区、持续看护和立即断电五项事实，不再强制电源复位、USB重插或底盘落地事实；底盘分支仍固定DEFERRED，原精度、停止、身份、锁与GO有效期门未变。本机与Orin同源46个维护测试方法通过。

实际只读74次均自然退出并回收：17电机Torque_Enable为0；left5/7/8均Model777、FW3/10、Mode0、Phase12、P16/D43/I0、Lock1、双死区1。各自内置限位覆盖当前起点及+24目标。位置执行件复用f201、原帧读取件复用13c/c800-a30；本次未重新编译这些冻结件。

随后仅执行一次左腕原参数基线，driver615633、pane%42：锚点953、出程目标977，期限内停在972，误差-5，未进入回程。四次写入仅left5的GP953、TE1、GP977、TE0，没有I、头部或底盘写入。最终17×3停止字段有50个整数零，right7的Present_Velocity为+50、Torque_Enable/Goal_Velocity为0，停止FAIL，原run为NEEDS_MANUAL_POWER_CUT。该+50只有旧位置工具的解析整数，没有对应原UART帧；原因、单位与实际轮运动未证实，不回填旧right9的-50或左腕回程+6。

左腕断扭ACK、62项身份后验、310个不同自有child自然回收及独立非UART清场均通过，不能覆盖精度/停止FAIL。06:36:26UTC非UART收尾完成：仅移除已记录dead1/exit1的自有pane，正式ca14四件未变、六服务停止、两USB/四锁无持有者、无已知控制或读回进程。当时已要求现场切断两个电机电源并保持机械臂支撑，断电确认PENDING记录保持。root于06:52:28UTC收到用户“两个电机电源已切断，机械臂保持支撑”，最新状态为USER_CONFIRMED_OFF，本轮看护收尾结束；实际切断时刻/独立遥测UNKNOWN。新回执为本轮manual_power_cut_confirmation.json，只更新最新现场状态，不回填原NEEDS/STOP FAIL或tar原件，也不构成新GO；截至该轮断电收尾，没有后续UART或动作。

该原参数阶段的固定left5临时I=1对照仅为本地候选：复用原+24/±4/800ms位置门，入口及恢复前要求目标TE/PV为0、恢复写前Lock1，最后恢复I0并核P/D/死区；停止或恢复未知即封锁后续UART。最终8个内存方法及默认PLAN_ONLY通过，未上传、签GO或执行参数写入，不能称为偏差修复。证据入口 [.verify/position_return_2026-10-10_r1](../../.verify/position_return_2026-10-10_r1/summary.json)。右H2本次完整PASS与旧失败分别保留；头1旧+8、头2/三轮主动运动待验、D15 OPEN/DEFERRED及deployable=false保持。

本机实际更新nightly、执行fmt退出0；Orin本次实际更新nightly→fmt→根Clippy零警告通过，编译器仍32dba69d6。主树与原生597项Rust输入逐项同字节/权限，格式化前后及批次首尾保持；19个直接child及3份SSHowner自然exit0回收，不外推Cargo内部未导出的PID。原件在本轮commit_quality/；没有新release、完整workspace测试、UART、安装或服务操作。

### 右 H2 完整通过（2026-10-10，本轮已收尾）

root于04:26:45UTC收到用户当次“全部已就绪，可持续看护并立即断电”确认；新的alignment/七项现场事实/600秒GO分别绑定本次runtime894、run02。实际EXEC_HANDOFF于04:28:59.799200UTC核验，在GO有效窗内消费；authorize/launch各1，没有整轮重试。

本次原run/report均PASS：四组各100预热+1000正式，4000正式/400预热/4432原attempt；分类重采0/11/0/21共32保留。原P99/RT/authority/trace门、最终非dry Disable Ack/Disarm全部通过，工具605107及driver605045自然exit0回收，原pane为%39。两个观察SSH均自然exit0/reap，未知进程门未修改；此次通过不证明旧585588的归属或具体原因，旧FAIL原件保持。

四组A P99依次23.008/23.264/19.872/19.840微秒，B P99为10.039744/15.941728/9.909024/16.058784毫秒；两个idle的原B max54.796672/60.363744毫秒仍保留，验收判据为原P99。最终trace valid/children_stopped/final_events_drained均true，dropped0。

独立前11清场、17电机34项原帧Torque_Enable/Present_Velocity整数零、后11清场及最后17项非UART身份/占用者核验全部PASS。40个读取/前置child、14个最终非UARTchild及硬件阶段9份SSHowner均自然回收，无信号；两USB/四锁无持有者，正式ca14四件未改、六服务停止、保护状态保持。头部/底盘的停止读回不能当作主动运动通过。

本次runtime为8940847，冻结控制/测量build仍a96c34a，597项Rust输入聚合5970c9be、实际冻结nightly32dba69d6；独立停止读回ELF仍来自c800/a30，不重贴最新编译器。证据入口 [.verify/right_h2_2026-10-10_r2](../../.verify/right_h2_2026-10-10_r2/summary.json) 与旧FAIL包分别保存。05:31:39UTC已通知可在机械臂保持支撑下关闭两电机电源、结束本轮看护；实际断电时刻/遥测UNKNOWN，此后没有新UART/动作或正式部署。

提交前本机实际rustup update nightly与fmt退出0，Orin实际nightly更新→fmt→根Clippy通过，仍为32d；13条原生命令及6个受记录质量SSHowner均自然exit0回收，597份输入主树与原生首尾逐项相同。质量原件在本轮commit_quality/，非完整workspace测试或新release。早期两份上传回执仅有exit0、缺PID/reap记录，不由后续回收补写。

按用户“旧target及时清理”授权，仅删七个旧source解包副本，约202.1MiB；每个590文件与永久归档逐项同字节，删除前无占用，归档前后SHA不变。必要产物、实时H2原件与其余缓存保留，清理回执随本轮保存。

下一步仍是左腕回程+6、头1回程+8及头2/三轮主动运动闭环，完整整机目标未缩减。头1目标2634与期限内2642编码/算术均正确；旧超期采样等待修复不解释+8，左腕仅有历史夹具不能推定稳定平台。底盘通用Mode1零停合同已有厂家教程/SDK和XLeRobot源码支持；Phase bit3在现场FW3/10、Mode1、Phase12、单addr46写入中的适用范围仍有文档冲突，保持保守拒绝、普通Phase写禁令及原精度/停止门。D15按用户决定OPEN/DEFERRED，deployable=false保持；新物理动作另需当次现场事实与具体GO。

### 右 H2 通知式观察候选及最新 nightly 冻结（2026-10-10，软件已验收）

未改生产driver、Rust源码、P99/RT/authority或未知进程FAIL门。仅把一次性SSH观察副本的2秒tmux/date/sleep轮询换成pane局部`pane-died`通知和每观察者独立channel；保留`exec`启动链、仅一次launch/断线后只观察、原最终run终态门。候选仍绑定旧a96和旧基址，不具备新现场事实或GO，不能直接用于新真实H2。

Orin tmux3.2a无害夹具六类场景PASS：退出0/1、退出后续接、双观察者、提前/错误通知拒绝、退出先于wait注册。9个observer实际自然wait/ECHILD，driver仅以tmux真实pane_dead/status证明退出；首夹具r1 FAIL保留。实际继承的pane-died没有命令，不外推未来配置；未主动注入真实SSH断线或tmux服务器重启。

另一次纯软件机制实验自然运行32个自有child、501次读取，其中188次exe ENOENT为161次R→R及27次R→Z；另1个受控child独立记录，33个均自然wait0/ECHILD且后验缺失。该机制可自然复现，不证明旧PID585588归属或具体根因。入口为 [.verify/h2_tmux_observer_2026-10-10](../../.verify/h2_tmux_observer_2026-10-10/summary.json)，原件按字节保留，不新增框架/index/verifier。

本轮实际浮动nightly更新至32dba69d6，原生fmt→根Clippy及13条命令自然回收通过；主树352与原生a96的597项逐项相同、聚合5970c9be。六配方/生产4测量3/七help/默认PLAN_ONLY已实际通过；新测量件短mock4组各2样本及tonic6方法9场景、全部server joined通过，分别33及12个直接child exit0自然回收。四份597源码、编译器、七件/两manifest、正式四件/六服务及控制进程前后保持；实际build仍a96，运行文档352祖先继承已核，不改贴HEAD。
新原包release-post-32d-r2-originals.tar.gz为425017字节/254文件、SHA8d420169…，仅保存必要原件，不含七ELF、完整源码或共享Runner正文。未执行的f92草稿与唯一实际5f5稿均保留；后者仅同步post回执中的CARGO_HOME/proxy记录，V2构建六变量合同保持。独审与root软件验收通过，范围/环境展示及复用边界见入口review.md；无新硬件或完整Rustworkspace测试，软件PASS不作为H2成功。

root于03:37:12UTC收到用户“两个电机电源已切断，机械臂保持支撑”，本轮看护收尾结束；实际断电时刻/独立遥测未知，不回填原H2 FAIL或STOP34 NOT_RUN，不构成新现场准备/GO。确认后无新增UART/动作、安装或服务操作；旧左腕/头1 FAIL、头2/三轮动作待验、D15 OPEN/DEFERRED及deployable=false保持。

同轮独立只读核对底盘参考，不计入H2软件门：[官方XLeRobot固定b017的stop_base](https://github.com/Vector-Wangel/XLeRobot/blob/b017b5e6354bd9f61f4247a920c72622ca0aade0/software/src/robots/xlerobot/xlerobot.py#L634-L636)确实对三轮写Goal_Velocity=0。[飞特官方教程](https://www.feetechrc.com/Data/feetechrc/upload/file/20220618/%E5%85%88%E7%9C%8B%E8%BF%99%E9%87%8C-%E5%85%A5%E6%89%8B%E6%95%99%E7%A8%8B2-1.pdf#page=5)第5页§10先设Mode1、第6页明确零速停止；完整PDF SHA1db8cf7421b61c45f8d271ef8bb6fa58bb8fbbaa5e307a1b63b3c705ad328438。[飞特SDK示例](https://github.com/ftservo/FTServo_Linux/blob/06fd3356dbd7bccd886b5a70d7ae0fccc6c76d38/examples/SMS_STS/WriteSpe/WriteSpe.cpp#L17-L26)同样零速停止，但实际WriteSpe还写ACC/位置/时间，与单2B速度事务不同。
这些来源没有绑定现场右9的FW3/10、Phase12；已归档portal201内存表bit3也未明确限定仅位置模式。通用Mode1零停意图已证实，现场组合和bit3模式适用仍UNKNOWN，不由断扭零反馈推出启扭安全，不改profile/参数/门。本核对未访问机器人。

### 右 H2 测量通过、未知进程观察 FAIL（2026-10-10，本轮已停止）

本次仅一次真实右H2，runtime/build为a96，597项Rust输入仍为5970c9be；authorize/launch各1，四组4000正式+400预热、4428原attempt及测量report PASS，最终非dry Disable Ack/Disarm通过。
工具自然wait0/reap；driver整体FAIL/退出1，本机重连observer自然wait1/reap，不能以工具PASS覆盖。PID585588的exe读取ENOENT/errno2，同PID/start335412890、PGID571196、remaining R，当时归属与argv未证实。
后续vanished或PGID leader为bash不用于推断该PID身份或追补PASS；首owner相对路径NOT_LAUNCHED、实际SSH首次自然255及只重连观察原pane的回执均保留，没有再次启动H2。
两批非UART清场各11项只读门PASS；原整体cleanup均继承driver FAIL/退出1。final_buses17项/14child及本机SSH自然0通过，正式四件/六服务前后未变，重复11项不累计。
17电机34项STOP寄存器读回本次NOT_RUN：root已要求立即断两个电机电源并支撑，其后不新增UART；实际断电确认PENDING、扭矩状态UNKNOWN，未记录解除看护。
原件永久入口为 [.verify/right_h2_2026-10-10](../../.verify/right_h2_2026-10-10/summary.json)：4510文件原包SHA4286825d…、六owner目录及实际control/inputs/physical/GO与失败/最终run均按字节保存；不新增框架/index/verifier、源码tar或七ELF本体。
旧H2/左腕/头1 FAIL、头2/底盘主动运动未验、D15 OPEN/DEFERRED及deployable=false保持；等待实际断电确认，本轮停止、不自动重试。
提交前原生软件包已补：实际更新浮动nightly至32dba69d6，fmt→根Clippy及源码/编译器/Cargo后验通过，13条child自然exit0/reap、无信号/残留；主树与原生首尾597项逐项相同。硬件实际冻结build仍a30，不贴32d；无新增测试执行/release/UART/服务操作，软件PASS不覆盖硬件FAIL。两次独审与root核验范围见上述入口的review.md，原件commit-quality-originals-r1.tar.gz保留，不称4510文件全量封装审计。
软件静查已定位扫描门：先详细读取同UID进程，完整身份后才分类related；585588在exe ENOENT而stat仍在时先进入unreadable。观察脚本有短进程churn，但原件未绑定该PID父子关系，不能确证自干扰或按不同PGID放行；未改代码/验收门。当前H2复授权仍按已批准D9执行电源复位与USB重插，普通连续调试不一概要求重插；新简化路径未批准/实现。

### 自有 H2 退出观察修复后的 V2 准备（2026-10-09，软件原件已保存）

原生源码/实际产物 HEAD 为 `a96c34ae26b56fc9e8d2deef17c3b829c3d87d0d`；597项Rust输入聚合 `5970c9bed0ab02f99b47d88d47eb3718c6a748155ca4fafd5c18050a68ec6cf7`，实际nightly `a30aa9064`。
六条实际release配方、七件冻结、33个直接命令exit0自然回收、七help及默认PLAN_ONLY通过；七ELF与旧c800同字节，不称全量fresh编译、完整Rustworkspace或新H2通过。
r2实际fmt→根Clippy及r4完整124Python按同源复用，本轮未重跑；独审与root已核软件原件，正式四件和六服务前后未变。
16项只读预检与当时两端同HEAD/clean/源alignment通过；本归档新增本地文档/证据，下一批须重新核当次alignment。
简洁永久入口为 [.verify/h2_owned_child_v2_2026-10-09](../../.verify/h2_owned_child_v2_2026-10-09/summary.json)：149文件原包、直接JSON和一次性prepare/control原件按字节保存；不含七ELF本体、完整源码或旧Runner正文。
root于2026-10-10 02:02:47UTC收到用户本次“全部已就绪，可持续看护并立即断电”确认；仅一次右H2四组、Disable、17电机34项停止读取及清场的现场授权已生效，新GO尚未生成、新硬件NOT_RUN。
旧H2/左腕/头1 FAIL、头2/三轮主动运动待验、D15 OPEN/DEFERRED及deployable=false保持；仍须当次alignment核验和独立GO。本记录步骤无远端/UART、测试、服务操作或提交。

### 自有 H2 工具退出观察修复（2026-10-09，软件验收）

仅在现有/proc探针中保留FileNotFoundError原errno/filename，并对唯一自有Popen、tool PID、完整launch/before与remaining同代次的PID相对字段ENOENT做一次真实wait；成功后单独保存原snapshot全文及退出观察，通用unreadable与其他PID/权限/错代次/仍活/未知字段仍严格拒绝。supervise/execute/report/cleanup、失败粘性及P99/RT/authority门不改，非零工具退出仍FAIL；旧硬件run没有新增字段，不追补PASS。

原单例red→green和Mac定向负控通过，Mac原58平台batch FAIL保留。Orin r2实际nightly更新、fmt→根Clippy通过；r4同源完整124方法（H2 61、artifact19、维护44）0失败/0skip，新9为61子集。同一Linux方法的额外采集不累计：无害child的真实exe ENOENT、同PID/start_ticks残stat为Z、Popen真实wait0及随后waitpid ECHILD均已核，不称精确复现旧R瞬间。

两批597编译输入与两Python overlay首尾相同，实际a30；本机更新nightly/fmt及完整597与Orin逐项绑定通过。原QA r1更新路径FAIL、r2权限夹具umask FAIL、r3可选字段KeyError保留，验收组合为r2 Rust门+r4 Python/机制，不称单批全过。原件见 [.verify/h2_owned_child_exit_2026-10-09](../../.verify/h2_owned_child_exit_2026-10-09/summary.json)。本阶段无新硬件、六配方release或完整Rustworkspace测试；同步提交后准备新V2，新现场事实/GO后再做一次右H2。旧H2/左腕/头1 FAIL及头2/三轮动作待验保持，D15 OPEN/DEFERRED、deployable=false不变。

### 右 H2 四组测量通过、退出观察 FAIL 与停止读回完成（2026-10-09）

本次用户重新确认电源/USB复位、空载支撑、活动区域清空和持续看护；runtime为aa03、七件build仍c800，597项Rust输入及三个H2跟踪helper满足既有继承门，实际nightly为a30。首120秒GO在启动前过期被拒，driver/工具/UART均未启动，原exit2保留；新600秒GO在有效窗内消费，仅一次真实右H2，不是硬件重试。

四组各100预热+1000正式样本按原P99门通过，4430次原attempt保留，两组inflight各15次分类重采样。RT/authority/trace门、最终非dry Disable Ack/Disarm、S0/gateway退出和工具真实wait0均通过。driver在唯一自有tool退出窗口读取/proc遇到FileNotFoundError、同代次stat仍R，约7.686456ms后真实wait0；原driver整体FAIL及后续不可覆盖FAIL保持。旧异常未保存具体缺失字段，不推断一定是exe缺失，也不由测量PASS回填整轮PASS。

独立11清场、17电机34项原帧Torque_Enable/Present_Velocity正零、读取后的11后验及两USB/四锁17项核验通过。40个读取/前置child、14个最终非UARTchild及实际SSHowner均自然回收，无信号；右9旧-50此次未复现，旧FAIL保持。正式ca14四件未改、六服务停止、保护状态保持、无控制进程或占用；未安装/激活服务。本次看护已解除，已通知在支撑下可关闭两电源，实际断电仍UNKNOWN。

原件与构建/工具来源见 [.verify/right_h2_2026-10-09](../../.verify/right_h2_2026-10-09/summary.json)。下一步只修复自有child退出观察的窄软件路径，再做新右H2；新批次须新现场事实和GO，不自动重试。左腕+6、头1+8及头2/三轮主动运动待验仍保留，完整双臂/头部/底盘目标不缩减；D15 OPEN/DEFERRED、deployable=false不变。

### 维护位置读取到期收尾（2026-10-09，软件修复）

原头1记录中，回程目标2634已读回；190.694–801.681ms连续11样本均为2642，原+8精度FAIL保持。另定位到维护脚本在读取已经到期后仍固定等待50ms的独立缺陷。本次只在原PASS判据后增加到期失败立即退出采样的三行分支；±4/800ms、正常采样间隔、断扭收尾与停止读回门不改，不宣称取消跨期IO或解决实际位置偏差。

本机和Orin各44方法（原40+新4）通过，原实现red及旧硬件FAIL保留。本机实际fmt、Orin实际fmt→根Clippy通过；两份Python脚本的完整SHA首尾一致，原生质量源码为c800加这两份dirty，浮动nightly实际a30。597项Rust编译输入仍与c800冻结件逐项相同；H2沿原有祖先提交和三个跟踪helper字节不变的继承门准备，build_head仍c800，不重贴运行提交。原件见 [.verify/maintenance_deadline_2026-10-09](../../.verify/maintenance_deadline_2026-10-09/summary.json)。原生clone退出文件为空的回执缺口另保留；实际pane0和独立Git身份不用于回填该文件。

此前c800/a30六配方七件、独立停止读回工具、16项只读前置和新产物AC-12短mock四组各两样本均已通过，原件暂保留target/h2_native_prepare_2026-10-09，待实机批次一起收口。本次没有UART/动作、正式安装或服务激活；本次现场七事实仍待用户答复，尚未生成新GO。旧右H2/左腕/头部FAIL、D15 OPEN/DEFERRED及deployable=false不变，完整双臂/头部/底盘实机成功目标未缩减。

### 头部与右轮只读诊断（2026-10-09，实机事实）

用户本次确认恢复供电、支撑/看护就绪及可立即断电后，在原Orin仅执行一次40项只读：17个电机Torque_Enable均0；左头7与右轮9型号777、固件主/次原字节3/10、Phase12。头部Mode0、P/D/I=16/43/0、双向死区1/1、Present_Position2642、复位后的Goal_Position0、当前速度0；右轮Mode1、Lock1、位置3494、目标/当前速度均为原帧正零0x0000。40条SYNC_READ帧独立核验通过、40工具child及SSH真实回收；120条请求包含未捕获的握手，不称120份原帧。

原-50此次未复现，具体根因仍未知；本次没有回程，不能关闭头部+8。厂家当前通用内存表门户版本201，bit3说明与保存181相同，和Phase12存在零速语义风险；未找到FW3/10在Mode1下的官方例外，也未实测启扭验证，断扭零反馈不作为零目标安全证明。未修改PID/Phase/Lock/模式或EEPROM，未启扭/运动，不改原验收门。

前后非UART核验通过，正式ca14四件、保护文件及USB身份保持，六服务停止，无控制/读回进程，两USB/四锁无占用。本次读取已结束，已通知可在保持支撑下关闭电源并结束看护；新的实际断电确认尚未收到。数据见[只读结果](../../.verify/hardware_readonly_2026-10-09/summary.json)。旧H2/左腕/头部FAIL、D15 OPEN/DEFERRED及deployable=false保持；暂停草稿不恢复，无安装、服务启动或push。本机实际fmt、同597项源码的Orin实际fmt→根Clippy已通过，浮动nightly更新至a30aa9064；代理10.0.1.80:10808及tmux已实测用于更新/检查。原下载超时与rustup自更新失败保留。现有H2七件待重新构建，本轮尚未做新H2。

### 未实现控制模式不再假成功（2026-10-08，软件修复）

`FeetechAdapter::write_command` 原先对 Velocity、Torque、ServoPosition 不发送指令却返回成功；现明确返回 `Unsupported` 并保留原诊断。Position 与 Stop 行为不变。仅改两份源码，S0 生产逻辑逐字不变；新增软件回归验证拒绝时无租约/无 TX，以及 S0 错误后实际调用 mock 断扭和确认。Mac Feetech 库215项、Orin Feetech库及 S0 周期控制17项通过，根Clippy通过；准确原件与提交前质量门见 [.verify/feetech_command_mode_2026-10-08/summary.json](../../.verify/feetech_command_mode_2026-10-08/summary.json)。本修复仍未实现轮速执行，不关闭头部/右轮/右H2原FAIL；本轮无UART、硬件动作、安装或push，现场供电与看护答复仍待确认，D15 OPEN/DEFERRED、deployable=false保持。

### 收缩后续设计范围（2026-10-08，具体硬件诊断优先）

用户指出过度设计后，root 暂停尚未整合的 P3-A 打开会话框架和 P3-B 事务/RX 代次框架，保留两工作树未完成草稿；两者均未执行 fmt/Clippy/test，不能称完成。第三件绝对 READ 合同暂缓、未派实施。先前“会话/USB与预算接缝”为历史计划，本节取代其自动续作安排；已有软件提交不回退，完整底盘运行与机械臂/头部/底盘实机成功目标不缩减。

当前仅给现有 `feetech_poke` 补 `read-raw` 只读诊断，已整合并软件验收：复用已有固定缓冲 raw-by-ID，成功时保存同次 SYNC_READ 请求和经校验回包原字节及 unsigned word，旧 read/write/ping 保持。Mac与Orin各13个工具测试方法通过，7新增为13子集；Orin实际fmt、两根Clippy及独立release通过，三类参数在开串口前拒绝并实际退出2。首次离线缓存缺Arrow的resolver101/owner退出1保留，恢复此前真实离线CARGO_HOME后只续未过检查，不称单批全过。主树与Orin前后四份597源码清单逐项一致、聚合03a11e78…、实际nightly1d81；root实际fmt/静审、原生stdout/wait及AArch64产物SHA核验通过。[简洁软件回执](../../.verify/feetech_poke_raw_2026-10-08/summary.json)保留必要原件，无新封包框架或源码tar入库。

新工具仍未做实际UART读取，新增记录不能回填历史 read 单 READ 的原帧；状态回包不回显寄存器地址，输出的地址仅是请求字段。头部回程 +8、右轮终读 -50 的具体原因仍 OPEN，原 ±4/800ms 和停止门不放宽。下一次读回重点是实际 Firmware/Phase/PID/死区与原始位置/速度字；没有事实前不调 PID、不写 EEPROM、不推定单位或真实转动。现场供电/本次看护事实已发单次询问，答复前不触UART或动作。

用户已确认两个电机电源切断、手臂保持支撑；本次无新 GO/UART/动作、安装、部署或 push。D15 OPEN/DEFERRED、旧左腕/右H2 FAIL 与 deployable=false 保持。完整执行链仍须实现和验证，不能把诊断工具通过宣称为整机成功。

### 固定缓冲原始读回（2026-10-08，已整合软件组件）

主树新增三缓冲raw-by-ID读取：单租约覆盖整组请求/回包，MAX32固定暂存，所有帧合法后一次提交；保持原word、输出尾槽和旧读接口。具名寄存器仍仅1/2B，4B只验证共享私有核心。Mac与Orin各230不同方法通过，13新增为lib213子集；Orin本轮15child、32完整源清单、7本机owner及两真实8B负控完成，main597/`5c8c4c04…`与实际fmt绑定本轮原生质量门。平台回执差别及原Mac r1编译FAIL/r2PASS保留见[本件证据入口](../../.verify/feetech_wheel_raw_read_2026-10-08/summary.json)。

下一步是三轮专用wheel adapter的实际会话/USB与请求预算接缝，当前仍是函数计划；raw read只沿旧调用预算，不提供来源绝对READ截止、USB认证或Enable授权，尚无S0/S1/轮速TX接入。无需重跑本件已过门。人工断电确认后无UART/新GO；旧头1/右9停止、左腕/右H2 FAIL，D15 OPEN/DEFERRED、deployable=false保持，无新硬件、完整workspace/release/RT/H2、安装、部署或push。

### Feetech 来源绝对期限写入（2026-10-08，同源软件验收）

六份Feetech源码已整合来源绝对期限写入：`SourceWriteDeadline`借用`RobotClock`并保存固定source期限，共享核心保留transport调用的固定预算，首写、partial-write、timeout安装后和返回后均严格以`now >= deadline`拒绝，不续期、不整帧重放。固定错误保留wanted/got、前后观测时刻、是否尝试写入和当次IO错误kind；非法count优先，合法进度记录后按时钟回退/source/call到期顺序判定。新普通写API必须显式提供probe，真实性由调用方保证；无probe在lease/transport前拒绝，未知transport默认Unsupported。旧Position/Safety/Confirm调用保留原API，未改冻结架构。

Mac与Orin分别通过217个不同方法：完整库200加集成12/5，0failed/ignored；默认/real-hw库同名200、29新增和24过滤子集不重复累计，负控child不另计方法。Orin r2实际fmt→默认根Clippy→real-hw根Clippy/default库200均通过，批次因Linux负控固定`(core dumped)`显示后缀被误拒而保持原FAIL/owner exit1；r3只运行剩余real-hw200和12/5门并通过。两批19个原生命令、42个596项源码窗口及实际浮动nightly `1d81eb4ad`一致，所有自有child真实自然wait/reap；这是同源两批合并软件PASS，不称单批全过。root整合后实际主树fmt0与596输入聚合`bfe2318fd0bbac6e5f3c34766237ceca634f72331091053b7d2450e5b0ee0f40`逐项相同，复用上述原生质量门，未称Mac根Clippy通过。Mac旧helper/lint/compile/allocator失败、原生准备失败及r2原FAIL完整封存。

本lib真实allocator已复验新路径和旧热路径；Mock捕获`drain_tx`在guard外保留容量，接口自身仍分配，foreign backend自行构造String错误的分配未由fixture证明零分配。clock回退检测限于core观测窗，call预算从transport start计；native timeout不能撤回已发字节。本件未接S0/S1、新鲜度事件桥、真实轮速TX或Enable授权，无完整workspace/release/H2六配方、新硬件动作、正式安装、部署或push，无syscalltrace。头1回程+8、右9停止字段-50、左腕和右H2原FAIL保持；用户已人工切两个电机电源并保持手臂支撑，未核独立电源遥测且无新GO。现场Firmware未读、Phase12未改，D15 OPEN/DEFERRED、deployable=false保持。

[准确名单、两批原件与主树质量绑定](../../.verify/feetech_absolute_write_2026-10-08/summary.json)。

### 底盘输入组件（2026-10-08，软件通过，真实执行链另验）

本阶段合入三项纯软件组件：样本系统header的完整128bit发布Node/Port身份、显式米/米/弧度每秒轮速参数，以及底盘输入的固定容量状态机。身份不是密码学认证；当前一组和退休一组来源不构成无限历史注册表。状态机要求原生产者时间/严格TTL、同Node不同Port的完整心跳与命令pair、checked非零接收计数、零Ack committed投影之后的新输入及当前Enable committed投影；Lost只凭心跳不能恢复，Ready恢复仍待Enable完成。票据/计数耗尽原因粘性保留，时钟回退检查限于已保存生产者时间与绑定边界。只有Active且新鲜的gated_command可构造固定POD；生命周期字段仍只是受信运行时投影，不能代替真实authority/硬件结果。

Orin独立596项编译输入聚合`b919cd320dd81693ae49612dcd0778b98e6ee97191d2344bdf87cefd69c2eed0`，实际浮动nightly为`1d81eb4ad9cd207e3e638bd32b17ec4fce8412a6`。fmt、默认根Clippy及rt_guard+Feetech real-hw组合特性根Clippy通过；core147、transport默认129、状态机guard37共313个不同Rust测试方法通过。mock61和状态机默认36为上述子集，guard内部重复调用也不累计；另1项compile_fail文档测试通过，原2项库/集成ignored和2项doc ignored及其原因保持。18个原生命令、5个本机SSH/SCP child均自然wait/reap，38个源码窗口和编译器首尾一致；主树精确整合11源、实际fmt与整合后596逐项相等单独闭环提交前质量绑定。原两轮Clippy101及r4名单展示夹具FAIL/零测试执行完整保留；新通过不覆盖旧失败。

本件未接入生产IPC/RPC、实际连接代次、唯一writer或Enable链；FeetechAdapter Velocity仍无TX。下一件已定案为来源绝对期限写入的首字节/partial-write检查，当前仅函数方案，不是实现。无完整workspace/release/H2六配方或真实RT/硬件验收，无正式安装、服务激活或push。人工断电收尾已独立确认，此后没有UART/新动作；现场Firmware尚未读、Phase12未改，底盘动作DEFERRED，左腕/头1/右H2原FAIL、D15 OPEN/DEFERRED及deployable=false保持。

[原件、准确名单与整合验收入口](../../.verify/b1_ingress_components_2026-10-08/summary.json)。

### 固件合同与人工断电收尾（2026-10-08，软件通过）

七份Feetech源码新增固件主/次版本地址0/1的原始无符号只读名，四个写API与poke在获取连接/打开串口前拒绝写入；既有Phase/CW/CCW诊断写禁令保持。纯轮速profile逐项绑定前8/左7/右9、型号777、模式1、明确固件原字节、完整Phase和1..32767的raw上限；Phase bit3=1的零即最高速配置拒绝构造，版本相符不等于厂家兼容性认证。速度采用厂家名义0.732/0.0146RPM与bit15符号幅度，超出单轮上限直接拒绝，由上层统一比例限速；细单位不宣称精确步数每秒。本件无串口、Enable权限或运行时接入，当前FeetechAdapter Velocity仍未完成TX。

Orin独立591项源码输入聚合`ddad9513566f51fecf915ac7f39e0cccc66b8ae93732847b70ac7c0315458294`，实际最新浮动nightly为`1d81eb4ad9cd207e3e638bd32b17ec4fce8412a6`；fmt、默认根Clippy及仅Feetech real-hw特性根Clippy通过。real-hw库171和poke6共177个不同方法全部通过，0failed/ignored，新增12为其中子集；131072个wire样本是一个测试内部输入，不另累计方法。独立release构建冻结ELF183 `bde890bb1bd0ea20f63d5267f158a311edd539eaedbc7f04cf13f248b5272835`，1470648B；help与两固件名写入探针真实预期exit2、具体错误及不存在设备路径首尾一致，13外层质量命令和3内层探针共16不同自有child自然wait/reap，源码/工具链/产物首尾相同。Orin未装strace，零UART依据早退控制流、只读拒写测试和不存在路径探针，不宣称系统调用跟踪。非完整workspace、H2六配方或真实RT/硬件门；无正式安装、服务启动或push。Mac首轮Clippy101与后续未运行门保留，原失败不回填。

现场用户随后确认“两个电机电源已切断，机械臂保持支撑”，root于2026-10-08 06:35:40UTC记录收到回复；实际断电时刻与独立电源遥测未测。此新收尾确认结束本轮看护，不修改上一轮head1位置FAIL、right9停止FAIL或原NEEDS_MANUAL_POWER_CUT，不构成新硬件GO；确认后未执行UART/动作。现场固件版本未读、Phase12未改，旧左腕与右H2仍FAIL，D15 OPEN/DEFERRED、deployable=false保持。下一步只推进发布者身份/显式轮速配置/新鲜度状态机软件候选，真实执行链与硬件门另验。

[原件、准确名单及软件验收入口](../../.verify/feetech_firmware_contract_2026-10-08/summary.json)。

### 固定目标维护（2026-10-08，软件PASS，硬件NEEDS_MANUAL_POWER_CUT）

固定位置目标选择的本机与Orin同源40项软件测试通过，新增5项是40子集，不累计重复轮次；默认/三目标PLAN_ONLY均零动作，重复或未知目标拒绝。此前B0工具独立READ通过：17电机×10项共170唯一寄存器请求、211个自有child真实回收；连接有额外只读握手帧，不称170为全部wire帧。本次位置动作使用历史`f2014ee4`维护ELF（1469904B），重新核本体身份，未称最新nightly新Rust控制产物或使用B0工具代替位置工具。

root逐项审核具体计划与当次GO后，仅运行一次右5→左7→左8。右腕往返PASS（875→895，目标899；回程879，终值878/goal875）；头1回程2642/goal2634、+8超原±4/800ms门，位置FAIL；头2、左腕及底盘主动动作NOT_RUN，底盘DEFERRED/主动写0。最终17×3停止字段有50个整数零，右9唯一Present_Velocity=-50、Torque_Enable0/Goal_Velocity0，STOP FAIL，原报告**NEEDS_MANUAL_POWER_CUT**保持。右5/左7断扭ACK、359个driver child真实wait/reap、52个外层child真实回收及216项独立非UART身份后验PASS不能覆盖失败；ca14 clean、四正式SHA、六服务停稳、四保护锁释放与保护refs不变。

人工断电确认**PENDING**，已通知现场切两个电机电源，禁止后续UART/动作/自动重试；不推定右9读值的来源、单位或实际轮转动。厂家STS内存表（门户version=181）定义Phase bit3=1时零目标为最高速度，现场Phase12包含该位；现场固件主/次版本未读取，厂家文档版本不是现场版本。本轮无Phase/Lock/PID/模式或EEPROM写；后续仅继续固件READ保护/typed profile等独立纯软件小件，真实硬件门另验。左腕历史FAIL、右H2整体FAIL、D15 OPEN/DEFERRED、deployable=false保持。

[本轮原件、分类及逐SHA索引](../../.verify/hardware_selected_targets_2026-10-08/summary.json)；准备封装参数失败、漏fixture的6ERROR及原硬件失败完整保留，不回填PASS。只读分诊关联head1稳定+8的原样本和right9命令339的实际设备/ID/PID/stdout SHA；UART原始帧未捕获，工具输出字节不当作寄存器原字节。

### Velocity 入站与 SerialTick 归零基础（2026-10-08，Orin 软件门通过）

当前六源候选基于主树`8fb17b153f4eb85cb1b88ca935cc115087784256`加六处dirty；Orin为独立导出编译输入快照，`native_git_head=null`，不是带该HEAD的原生Git checkout。590项源码聚合`43b9d67550d885549f18cc43e5afd748a587f3d57a8800a61ab6f216dcff46c3`，各门前后与整个批次首尾一致。Velocity独立校验数量、有限值/f32范围、时间窗与Zero策略，拒绝不更新latest-slot/commands_applied；SerialTick以Forward/Skip/VelocityZero分流，同数量栈上归零不修改原命令，Revoked后仍复检原安全门并独立申请PeriodicRead。

r7实际更新浮动nightly至`1d81eb4ad9cd207e3e638bd32b17ec4fce8412a6`；fmt/默认根Clippy/`robot-s0/rt_guard,robot-s0/test-comm-fault-hook,robot-s0/xlerobot-real`组合根Clippy通过。S0默认lib641、guard+hook lib734、serial_rt_e2e5全部串行通过，0failed/0ignored；按方法名单库去重734加另5集成方法，共739，24新增为库子集不累计。r7远端13条原命令均自然exit0/真实reap且无信号/残留，源与实际编译器窗口通过。

旧本机r7 owner36112/SSH36113在turn中断后实际已缺失，工具句柄不可用、原status仍RUNNING且result缺失；真实wait/reap保持UNKNOWN。新只读SSH核主机/boot、远端原结果及PID缺失，新本机观察/收集/下载各自真实回收；这些事实不能回填旧本机owner。本阶段[归档入口](../../.verify/xlerobot_velocity_foundation_2026-10-08/summary.json)保存七轮原native tar/index/metadata而不重复解包590×7源码，另有620份本机原件和自索引；648份固定输入封包前后身份逐项一致。归档独审/root与提交前主树fmt→Orin同源根Clippy通过，仅阶段提交、不push。367条typed引用中366条原件逐项归档闭合，旧ELF仅历史身份及真实执行回执、未封存其字节；追加质量包95份原件可独核，封包器完整历史stat名册未持久化。

原r1测试lint/Clippy101、r2旧RR测试640PASS/1timeoutFAIL、r3两处logging文档lint101、r4liveness test模块位置lint101、r5更新自然exit0但3393.452秒超过原300秒预算/窗口错误/后六门NOT_RUN、r6serial_services test模块lint101与后四门NOT_RUN均保留；网络255及独立keep-going101不回填。r7只将原serial_services test模块逐字移EOF；liveness原test模块和断言也仅移动，logging只补两处反引号。旧RR夹具仅等真实tick/心跳/pending就绪，原3秒预算与生产不改；具体旧超时根因仍UNCONFIRMED。旧ELF精确单项PASS仅一次诊断，不计739；resume_r1摘要KeyError原FAIL另存，resume_r2仅按实际成功回执可省略runner_error读取None，非零和超预算仍严格匹配原错误。

这不是完整底盘B1：当前probe期限用于存活检查，尚无真实wire写入前/途中期限拒绝；固件Phase/速度单位/零速合同未验证。P1A发布者身份与P1B轮速配置仅孤立Mac软件候选、未整合，不作为本阶段已验收实现。用户现已重新确认两个电源/USB复位、空载支撑与持续看护，现事实与旧run分开，不回填旧NEEDS_MANUAL_POWER_CUT/FAIL；本软件阶段未下发新硬件GO或执行硬件动作。无正式安装/服务激活、完整workspace测试、release或push；右H2整体FAIL、左腕回程FAIL、D15 OPEN/DEFERRED、deployable=false保持。本阶段未新核正式安装件及服务健康，不以历史ca14快照证明当前后验。下一步推进身份/显式配置及完整B1；硬件验证另行绑定新原生冻结与当次现场事实。

### Feetech B0 诊断元数据（2026-09-30，原生软件门通过）

Phase18/CW_Dead_Zone26/CCW_Dead_Zone27以1字节原始无符号值读取，不解码Phase、推定单位或死区归因。厂家可写EEPROM与本项目只读写策略分开；四个具名bus写入口在查表/参数/租约/IO前拒三个诊断名，poke仅这些write在open前拒绝，其他有效/坏/unknown参数和原do_read/do_write行为不改。
主树基于d5ac7aa，五源已格式化整合；590输入聚合3d8491ee。首Orin QA根Clippy实际E0004/101（漏既有diagnostic穷尽分类）及后续NOT_RUN保留，最窄补诊断参数类及原编码(8,1,0)，原分类literal方法加三名断言、不增方法。第二轮四条cfg(test) lint/101及后续NOT_RUN保留，仅改结果消耗和空TX断言、生产前缀不变。第三轮新同源Orin最新nightly5c、fmt/两根Clippy、库160+例子5、优化工具release及九次NX拒写通过；18新增是165子集，三条新增分类literal不增方法。九拒写保真实exit2/回收及精确类型错误，编译器/590源码前后保持，[软件验收入口](../../.verify/feetech_diagnostic_metadata_2026-09-30/summary.json)已归档、独审/root通过。94份固定原件/副本另有自索引，后产独审附件单独引用，不回填三轮原包及原summary。r1 CLI普通坏参数提前拒绝导致的scope失败及r2注释比较器误拒原件保留。
本小件不触UART/上扭/模式或EEPROM写，不接S0/S1，不改正式安装或推送；实际Phase/速度单位未知。人工断电待答，旧右H2整体FAIL/左腕回程FAIL、D15 OPEN/DEFERRED及deployable=false保持。

### XLeRobot 三轮纯运动学（2026-09-30，Orin 软件门通过）

按现场上游布局实现具名SI机身速度与轮角速度，电机顺序固定前8/左7/右9；正逆变换留在 `robot-kinematics-xlerobot::base`，没有默认几何或实测轮速上限。超限三轮使用同一比例，浮点舍入略超上限时统一比例保守下调一ULP，重算仍超限即拒绝；不逐轮裁剪。所有输入及中间非有限值、非法几何/顺序和比例下溢0均显式拒绝，原手臂IK算法与接口不变。
新Orin隔离候选基于1c5758，仅两源变化；590项输入聚合 `0e580bd954d29014534c6d04707873c8e779e75ffb7b93add2a5095d136d4eaa`。实际更新浮动nightly仍5c543b0b8，fmt/根Clippy及完整crate lib默认并行30项通过、0failed/ignored；21新底盘是30子集，另9为原手臂，非全workspace或H2六配方。
首轮Clippy similar_names/101及30项NOT_RUN保留；仅改cfg(test)局部命名后新候选通过，不回填旧轮。主树相同两源已整合，提交前实际fmt→根Clippy与590逐项绑定通过；[独审/root与归档入口](../../.verify/xlerobot_base_kinematics_2026-09-30/summary.json)保存本机37原件/自索引及115原件/自索引的两轮Orin统一外包。首本机normalize与动态审查器runner_error口径误拒原件亦保留；封包后独审另附，不回填原包/summary。
该阶段不含raw轮速换算、Phase/零速语义、里程计、通用BaseKinematics trait或S0/S1执行链。1c的旧H2七件与本候选分开；以新HEAD做H2须另行同源原生冻结。后续B0已整合诊断元数据，软件动态门及实际固件合同分别验收，不由运动学通过自动关闭。
没有UART、上扭、模式/EEPROM写、正式安装/服务启动/push或新GO；人工断电确认PENDING，旧右H2整体FAIL、本次左腕回程FAIL、D15 OPEN/DEFERRED及deployable=false不变。

### 维护精度/停止分类与 H2 新 Orin 冻结（2026-09-30，纯软件门通过）

新增维护入口 `scripts/run_hardware_maintenance.py`，默认 PLAN_ONLY。分别记录位置精度、17×3 停止寄存器及身份后验；精度 FAIL 不短路每件保护/引用、每个服务、进程、USB 占用和身份核验。停止字段必须完整且为 int0，收尾 ACK/读取失败保持粘性 NEEDS；后来的零值不覆盖旧失败。原 +24raw、±4raw、800ms 保持，超过期限才返回的合格读值也拒绝。
本机和 Orin 同源 35 方法、默认零动作入口通过；root 另跑同35，不累计为105。r4 独审发现 NOT_RUN 三轮误作已测试目标，r5 修复并验证非目标 +4 接受/+5 拒绝，原 r4/开发失败原件保留。旧 run02 的 NEEDS_MANUAL_POWER_CUT 和回程 +6 FAIL 原件不改；新分类软件通过不确认当前物理断电。
现场 Phase/零速语义/速度单位尚未核实，新 V2 维护计划固定三轮 DEFERRED/BASE_FIRMWARE_CONTRACT_UNVERIFIED，完成四位置项后首次 wheel 调用前拒绝；伪 verified 字段无效。完整假执行保留204次快照READ，底盘WRITE/enable为0，整体仍FAIL；不是新硬件动作。三轮运动学按前8/左7/右9及统一比例限幅在独立工作树实施，尚未接入S0/S1。

H2另使用固定 build HEAD `1c5758f97b7c502a38c850e8352ce970216bea53` 完成新 Orin V2 六配方/七冻结件。589项输入前后聚合 `83048e983bb7a13c2f75ae23e593ec301f129b10b412db8f4f80176ca434f62f`；浮动nightly实际更新仍5c543b0b8。原生fmt/根Clippy、冻结源既有71 Python、7help、mock四组各2、tonic6测试/9场景与12owner定向测试通过；12为历史194子集，本轮不是194或完整workspace。新维护35与既有71分属候选，不混计或回填V2构建来源。
本轮提交前再次实际更新nightly并完成fmt→根Clippy；同589 Rust输入未改变。正式ca14 clean、四安装件SHA、六服务inactive/dead/MainPID0和无控制进程状态保持，保护12件与本轮维护绑定逐字段一致。[软件验收与逐SHA归档入口](../../.verify/hardware_maintenance_classification_2026-09-30/summary.json)包含49份本机原件及自索引、两份不可变Orin外包（V2 116原件、追加57原件，均另含自索引）。封包后产生的独审另存附件，不回填原包或原summary；无正式安装/部署/push/UART或新GO。
当前人工断电确认仍待答，供电UNKNOWN；旧右H2整体FAIL、本次左腕精度FAIL、D15 OPEN/DEFERRED和deployable=false保持。下一硬件步骤仍须当次事实与独立GO，不能由上述软件通过自动重测。

### 双臂 / 头部 / XLeRobot 底盘维护验证（2026-09-30，动作 FAIL、软件收尾完成）

用户重新确认双臂空载支撑、电源/USB复位、地面底盘空间与持续看护，随后要求底盘按 XLeRobot 模式。现场上游原件及实际寄存器确认前8/左7/右9，三轮既有 Operating_Mode=1；未写模式或 EEPROM。正式 ca14 执行面仍为双臂，头部/底盘未接入 S0/S1，本次仅独立裸wire维护，不能关闭 H2/D15 或宣称整车运动学通过。
冻结维护脚本本机/Orin各19项确定性测试、默认PLAN_ONLY和独审通过；两端是同19项，不累计为38。新GO逐次绑定主机/boot、USB代次、工具SHA、保护文件及标定；持四合作锁，异常仍真实等待自有child并独立断扭。
首run01被uv父壳命中严格进程门拒绝，12命令全部为前置查询，UART读写/上扭均0；原FAIL保留。启动方式改为直接exec由uv管理的解释器，未放宽进程门，新run02/GO重新绑定。
run02仅执行左腕ID5：锚点947、目标971，实测到967（+20）；回程953（+6）未在800ms内进入±4门，**整体FAIL**。最后Torque_Enable=0写ACK成功，235次UART读取/5次写入、300个自有child全部真实回收；17电机终读的Torque_Enable/Present_Velocity/Goal_Velocity均0，其余16个电机六寄存器不变。
原run因位置精度门先拒绝，后续refs/服务/USB核验未执行，并保守记NEEDS_MANUAL_POWER_CUT；root立即通知人工切断电机电源，未自动重试、加速或扩大容差。软件断扭读回不等于物理断电。右腕、头部两轴及底盘三轮动作均NOT_RUN。
独立无UART后验28命令通过：runner退出、双USB/四锁无持有者、六正式服务停止、ca14 clean及四产物SHA/保护refs不变。首后验误要求fuser退出0而FAIL，原件保留；按既有空占用退出1合同复核通过，不修改原硬件FAIL。当前人工断电确认仍待用户答复，物理供电UNKNOWN；本轮无后续动作，不能宣称现场安全确认已经结束。
[证据与逐SHA索引](../../.verify/hardware_maintenance_2026-09-30_r2/summary.json)。下一步先完成断电确认，分别核对舵机位置死区与维护工具的精度/停止分类；头部和底盘动作留待新的现场事实与GO。旧右H2整体FAIL、D15 OPEN/DEFERRED、deployable=false不变，无正式安装/部署/push。

### H2 非 RT 轨迹归属与尾部采集（2026-09-30，软件已验收）

仅改共享 SLA 证据工具 `comm_fault/sla.rs`：整次 Arm/Ready owner 按真实 u64 sample_seq 登记，旧事件保留原 owner；两个 child 真实 stop/reap 后只补一次尾 drain。
原 raw、capture、owner spool 保留；新 run 的 CSV 按 epoch 稳定排序派生，完整 CAS 分布按原 matched/warmup 正式集合重算，原采集统计另存。未知 owner/kind、故障、错代次、重复成功及容量溢出仍 sticky FAIL。
新增工具容量门为 44,000 owner、每 epoch 8,192 事件/每行 256 bytes、每组每类 8,192 retry bins；有界读取和 checked 计数，不截断或回退。此门也作用于共享 SoftwareProbe/SoftwareLoad，旧长负载未重验。
不改 RT trace ring/authority、原 100+1000、SLA A/B 数值与 P99 预算、Python 严格 reporter 或 final_stop/停止顺序。
最新浮动 nightly 5c543b0b8 下，隔离 Lima Linux ARM fmt/根及 feature Clippy、默认并行 feature 194 项通过，0 failed/ignored；12 项新 oracle 为其子集。
实际同步方法验证 4,401 epoch/4,000 正式成员，排除预热及分类不匹配的显著迟到值；未改 Python 门验证旧 CAS 拒绝、新完整分布通过、5 个严格反例拒绝，A/B 与尝试集合不变。
无 real-hw 的冻结三件短 mock 四组各 2 样本、最终 Disable/两 child 回收及一次尾采集通过；不是新硬件或旧长载通过。
589 输入聚合 `83048e98`，相对 S1 阶段仅 sla.rs 变化；独审/root 同源核验及提交前 fmt→根 Clippy 通过，Python 合同另按 uv 复验并保留原直跑回执。
两轮 feature lint FAIL、首准备 NOT_LAUNCHED 及独审自索引名误用 FAIL 保留；[软件证据入口](../../.verify/h2_trace_owner_2026-09-30/summary.json)。
本阶段未做 Orin 新 V2 六配方、完整 workspace、真实 RT 环暂停或新 H2；旧整体 FAIL、34 Unknown 未证实归因、D15 OPEN/DEFERRED 与 deployable=false 保持。
用户随后重新确认现场条件并要求硬件验证；独立维护层准备与本软件阶段分开，不以其寄存器/最小动作结果关闭 H2 或宣称头部/底盘已接入 S0/S1。

### S1 默认并行事件夹具修复（2026-09-30，软件已验收）

仅改 `events_drain/runtime.rs` 的 cfg(test)：独立 Node/端口 RAII 代替 leak，publisher ready 后先刷新 subscriber 连接，再发送事件；生产前缀 25,621 bytes 逐字不变。
两个原终态正例改为 send→真实 join→首次业务 receive；同 topic 双 subscriber 负控要求同一次终态在刷新端解码正确、未刷新端 None，排除零投递假过。20 ms grace、2 s 接收预算和默认并行保持。
Mac 定向默认并行 15 项 / lib Clippy 通过；隔离 Lima Linux ARM 更新浮动 nightly5c543b0b8 后，fmt / 根 Clippy、S1 完整 lib 默认并行 287 项通过，0 failed / 0 ignored，无 test_threads/env 覆盖；不是 Orin 验证或完整 workspace 测试。
589 输入前后聚合 `72ac7a50`，只有该 cfg(test) 文件相对4a8基线变化；root 最终集成同源核验及提交前 fmt/根Clippy 再验通过。
旧 s49 两例超时、Mac 既有 Linux-only handler lint FAIL、Linux 首缺 rustfmt FAIL、独审/root 包读取错误全部保留；历史两次具体根因仍 OPEN_UNCONFIRMED，不追认。
[证据入口](../../.verify/s1_parallel_events_2026-09-30/summary.json)：82 原件 + 自索引，内嵌 Linux 747 原件，独审/root 软件验收通过；未安装/部署/push 或重跑硬件。
本次右 H2 仍整体 FAIL，D15 OPEN/DEFERRED、deployable=false；下一步仅非RT SLA工具的跨轮归属/尾部采集软件修复，不修改RT或Python原严格门，不自动复测硬件。

### 右 H2 本次完整复测（2026-09-30，整体 FAIL，收尾完成）

本次依据用户当前“现场条件可以测试”确认生成独立 physical/GO；17:59:27–18:55:49 UTC 只执行一次，authorize/launch 各 1，无整轮重试。
实际 runtime HEAD `4a8e171f27bf18625973aa4f3a6097e9b9fbed18`，固定 build `b01603b56117a35279407de0c201bcd91bb6e5b2`；589 编译输入聚合仍为 `c0388807`，使用已验收的新 V2 冻结件。
四组各 100 预热 + 1000 有效正式样本，SLA-A P99 分别为 0.025536 / 0.025632 / 0.021120 / 0.020864 ms；SLA-B 为 10.292192 / 15.490912 / 10.255968 / 15.334496 ms，均通过原门。
顺序为 topic-idle、topic-inflight、external-idle、external-inflight；第四组另有 1 次分类不匹配，完整保留，不计入 1000 有效样本。
最终非 dry-run Disable 返回合法 Ack，Disarm 无 sticky failure、children_stopped=true；工具 exit0 真实回收。
但 driver 的严格 `verify_trace` 拒绝“trace混入故障/未知事件/另一轮”，driver/SSH exit1 真实回收，**本次整体 FAIL**；不得以工具四组及收尾 PASS 覆盖。
只读定位为第一组 3 条 IpcClaim 的 sample_seq 与 attempt 标签跨轮，尚待上下文打标路径闭环；这不是未知 kind，不先宣称测量误报或修改原 trace。

独立清场 11 项通过；随后逐条读回左 1–8 / 右 1–9 共 17 电机的 Torque_Enable / Present_Velocity，34 项均为 0，所有 child 真实回收。
读回后再验 11 项清场及双总线 16 项追加门：正式 ca14 四 SHA 未改、六服务 inactive/dead/MainPID0，无控制/读回进程，两 USB 及两侧锁普通/sudo 持有者均空。
右 marker/latch 保持 Dirty/LATCHED，左保护文件与准备基线相同；19:01:19 UTC 最终门完成，已明确解除本次值守，此后现场电源/支撑状态 UNKNOWN。
头部/底盘此次通过寄存器通信与抽样静止状态检查，**未测试主动运动**；不外推连续安全输出、D15 或无人值守。
[本轮证据入口](../../.verify/right_h2_2026-09-30/summary.json)保存原 FAIL、原件与当次绑定；历史右 H2 FAIL 和 34 条 Unknown 未证实归因均保留。
本轮无正式安装/部署或 push；D15 **OPEN/DEFERRED**、`deployable=false` 不变。下一步先闭环轨迹上下文软件问题，新的硬件整轮须重新准备并取得当次重置/支撑/在场事实与独立 GO。

### 周期恢复源码的新 H2 V2 准备（2026-09-30，软件已验收）

周期恢复 `b01603b` 已快进整合到开发分支；Orin 使用新的隔离源码与 release-r1，固定 build HEAD 为 `b01603b56117a35279407de0c201bcd91bb6e5b2`。
使用前实际更新浮动 nightly，仍为 `rustc 1.101.0-nightly (5c543b0b8)`；589 项输入两端及构建前后逐项一致，聚合 `c03888074cbd4005837bc0291aafdf72fff4a59f473d75f52298b1da42182a96`。
本轮实际执行 fmt / 根 Clippy、71 Python、六条 release 配方 / 生产四件与测量三件冻结、七 help；均通过。
冻结测量件 mock 四组各 2 软件样本、同源 tonic 6 测试 / 9 场景通过，包含 2.1 秒合法 Ack 与普通 2 秒/最终 3 秒期限反例。
S0 lib 710 与 serial_rt_e2e 5 只引用周期恢复阶段同源原件，不计本轮重跑或累计；本轮未跑完整 workspace。
原生 release/post 独审与 root 软件验收通过，入口为[本轮证据](../../.verify/h2_serial_recovery_2026-09-30/summary.json)。
source_before JSON SHA：`38958610ca73ec30c0623ec221666dafb61d69bbf7c053f4d06b3a010ea6f280`；production / measurement 清单 SHA：`b33933680a19c867c322f5b1053475a46ae7e013a389d9a1f49ec5b0d1f10605` / `bae41b6feaceb0f7fa5c0a1c5c1565bb5867a619b32346739853a6f3a461c7e2`。
原生原件包共 239 份 / 14,288,202 bytes，SHA：`f7283639df1317eb2593e52cae220532ae9e8582e1054a9630081139fdd23770`；首次源码 bundle 准备命令 exit128 原件保留，归类为准备失败，不计测试失败。
两次审查端 FAIL 原件保留：归档索引路径误传及安装件列表形态误用，修正审查调用后独审通过；生产实现与测试未改。
正式 ca14 四安装件、六服务与控制进程状态在批次中/后只读比较一致；正式后验时刻 `2026-09-30T16:47:09.620402+00:00`。
同源读回工具新冻结 SHA：`f2014ee4008311e7a8725235a39f0d1b3913391f17cbd977b7441f120539114b` / 1,469,904 bytes / ELF 183；34 条计划 SHA：`1966dc076d3b8f32d63ddbd173e4865c640f43e82779d8cadfbe0b89dad11f87`，未执行工具。
三个 helper 上传/读回与最小迁移静审通过；只读 prepare/alignment 模板和 34 条寄存器读回计划保留为未授权准备，未生成 physical/GO、未创建真实 H2 输出。
本轮未安装、未部署、未 push 或执行硬件；历史右 H2 整体 FAIL、34 条 Unknown 未证实归因、D15 OPEN/DEFERRED 与 `deployable=false` 保持。
下一步阶段文档提交后远端独立 source 快进对齐，589 再次逐项同源，再生成新 alignment；只接受该对齐时刻之后的本次现场事实，由 root 独立 GO 后执行一次右 H2 完整复测。

### SerialRT 周期恢复（2026-09-30，独立软件候选）

周期 Motion 写仅在 `Aborted(Revoked)` 后重检原急停/超时/收敛门，再由 PeriodicRead
独立申请真实读租约；其他写错误仍早退，不放宽写权限。外层 deadline 保留原相位，
直接跳到严格未来的首个周期点，不补跑错失 tick；零周期拒绝启动，溢出沿原出口移交 pending。
原事务预算、急停优先级、RT setup 与停止门不变；长事务后的等待和实际 H2 SLA 须分别验证。

两组 Orin red 的旧实现断言反例已独审核实，原 FAIL 与首次联网 setup 的 SIGINT/exit130 保留。
最新浮动 nightly `5c543b0b8` 下，本机 fmt、原生 fmt / 根 Clippy、调度 6、诊断 7、
`rt_guard + test-comm-fault-hook` S0 lib 710、`serial_rt_e2e` 5 均通过，全部 0 failed / 0 ignored；
调度 6 与诊断 7 均为 710 的子集，不累计。两端 589 项构建输入逐项一致，验证后复核一致，聚合为
`c03888074cbd4005837bc0291aafdf72fff4a59f473d75f52298b1da42182a96`；三处变更源码 SHA 在批次首尾一致。
定向软件门、独立审查与 root 验收通过，48 份原件已归档并逐项读回核验；入口为
[周期恢复证据](../../.verify/serial_periodic_recovery_2026-09-30/summary.json)。未跑完整 workspace 或 release 构建。
该周期恢复候选当时未部署、未 push；当时 `539d832` H2 冻结准备不含这些补丁。
同日上节的新 V2 软件阶段已按新源码重新原生构建与冻结，未复用旧 V2 清单。
既有 H2 来源只读 r4 的 16 项通过、授权/启动均为 0，正式安装件、六服务及保护文件未变，
仅作为未部署旁证，不构成硬件 PASS。
历史右 H2 整体 FAIL、34 条 Unknown 未证实归因、D15 OPEN/DEFERRED 与 `deployable=false` 保持。

### H2 / D15 闭环路径（2026-09-30，原生软件准备已验收）

当前先推进右 H2 完整复测；D15 按下述用户决定暂缓，两者分别裁决。**H2** 沿通信候选 AC-12：
topic / external × idle / inflight 四组，每组 100 次预热 + 1000 个正式样本；
SLA-A P99 ≤2 ms，SLA-B idle P99 ≤20 ms、inflight P99 ≤75 ms，原 RT、
负载和真实采样边界不变。还必须最终 Disable 返回合法 Ack、工具/driver 真实退出回收、
断扭读回及清场完整通过，不能用四组测量 PASS 覆盖最终收尾 FAIL。
`59120e34` 已完成 Orin 六条原生 release 配方、生产四件与测量三件冻结，release 独审通过。
浮动 nightly 实际更新为 `1.101.0-nightly / 5c543b0b8`；589 项编译输入前后聚合 `48360e9f` 一致。
V2 两套产物共用真实编译器前后回执及身份，V1 仅显式历史只读，新 execute 拒绝 V1。
fmt / 根 Clippy、71 Python、默认 PLAN_ONLY、helper 14 项真实机制和七个 help 通过；
冻结测量件 mock AC-12 四组各 2 个软件样本、同源 tonic 6 测试/9 场景通过，
含 2.1 秒合法回执、普通 2 秒与最终 3 秒期限的超时反例。本轮非完整 workspace，亦未重测硬件 H2。
release/post/archive 独审与 root 软件验收通过；253 份原件及本机证据已归档至
[最终证据入口](../../.verify/h2_nightly_v2_2026-09-30/summary.json)。
原 macOS 平台失败、辅助驱动修订及首次只读取证失败保留。
04:29:11 UTC 正式后验为 ca14 clean、四 SHA 不变、六服务 inactive、无控制进程；
本阶段无安装/push，不从软件快照推断物理电源、扭矩或支撑状态。
新右 H2 仍需当次供电/USB 重置、支撑/清场/持续值守事实与独立 GO，不自动重试。

**候选 E D15** 的已批硬门是 AC-E25：Enabled 真机 `kill -9 s0_daemon` 后 **<500 ms** 进入
torque disable / zero / brake，并有真实硬件及测量证据；D15-R 只是受监督 demo
例外，人工值守、EStop 发布成功或更快 systemd 重启均不能代替闭环。
主机冻结/失效、USB 断链以及重力/夹持物掉落的安全输出与独立测量属于**建议新增的
验收覆盖，须另行拍板**；不能冒称原 AC-E25 已批准这些场景或已经验证。
2026-09-30 用户确认**无独立自动断电保护**，决定暂缓 D15 实施与验收；状态为
**OPEN/DEFERRED**，不记 PASS。受监督 demo 沿既有 D15-R 继续；供电分域仍未知，
本轮不再追问、选型、采购或接线。[D15 闭环工作单](d15_hardware_fail_safe.CLOSURE.md)
保留 CodeMap、建议故障矩阵及原始证据格式，非 Spec Approved 或硬件实现；未写 no-op watchdog。
依据：[通信 AC-12](post_e_comm_fault_hardening.md)、[候选 E D15 / D15-R](v0_5_dimos_replace.md)。
下一步为右 H2 完整复测，原整体 FAIL、`deployable=false` 不变，历史原件保留。
本次仅调整范围记录；提交前本机 fmt、Orin 根 Clippy 通过，589 项编译输入仍与
已验原生源码逐项相同，见[文档阶段检查](../../.verify/d15_deferred_2026-09-30/summary.json)。

### 已验收的 Orin Rerun 软件阶段

**2026-09-30 Orin 原生 Rerun 定向软件已验收归档，独审与 root 通过。** 严格 SSH 已恢复；
独立目录使用 `a6957cc` 的 620 项源码快照，浮动 nightly 更新为
`2026-09-30 / rustc 1.101.0-nightly (5c543b0b8)`。工具链日期用于溯源，默认仍跟随最新，
仅真实编译回归才按 `CLAUDE.md` 登记临时日期回退及解除条件。

已确认 fmt、根 Clippy、17 项 viz 单测、9 项镜像测试（另 1 项 ignored 子夹具不计通过）、
6 项显式 SHM 集成及独立 `release-tools` 优化构建/help 通过。
独立构建与首轮六项集成实际使用的桥 SHA 不同；原测试可执行文件已针对最终桥
`0097228bb6cf42c8f580752620837017c6e79e65e5f0160c0817b95d087b90eb`
补跑同六项，严格 6/0/0，Popen 395114 实际 wait=0/reaped；源码与进程边界、
测试和桥两个产物的前后 SHA 全通过。共 32 项不同测试加同六项复验，不计为 38 项。
最终桥正常记录 1000 帧、10 类、11 实例，gap/drop/reject 为 0；官方同版 RRD verify
报告 `1 file verified without error`。本轮 SIGINT 夹具为 0 帧，只证明生命周期收尾，
不挪用下节历史 80 帧 live GUI 结果。Orin 定向软件门及最终独立核证通过。
本轮没有重跑完整 workspace 或下述 Lima RT/退出专项，不混用平台、工具链和产物身份。

新[摘要](../../.verify/robot_viz_orin_2026-09-30/summary.json)与
[原件包](../../.verify/robot_viz_orin_2026-09-30/evidence.tar.gz)已归档。
最终只读快照于 02:52:27 UTC 完成，比对通过：正式 ca14 clean、四安装件 SHA 未变，
六服务 inactive/dead/MainPID0、无控制进程。旧 nightly 备份保留于本轮隔离根目录的
`nightly-before`，未完成的 9/29 工具链已移除，原有 9/10 工具链保留。
未部署新发布者或执行新硬件动作；软件快照不推断物理电源、扭矩或支撑状态。右 H2 整体 FAIL、D15 OPEN、
`deployable=false` 和正式退役边界不变。旧 FAIL 原样保留。
本阶段仅本地提交，不 push；下一步真实发布者观测另按独立部署流程验证，
硬件门仍须当次现场事实与独立 GO。

### 9/29 Rerun 软件阶段（以下为该阶段事实）

**2026-09-29 Rerun 软件接入已验收。** 独立 `robot-viz` 支持十类 pub/sub 的文件录制和
实时 Viewer；有界非实时观测镜像保留原主通道 QoS、返回值和接收者计数，S0/S1 不链接
Rerun SDK。原始布局、时钟、实例和拒绝/丢失统计保留，Unknown 不表示物理安全。

macOS/Linux 分别通过 17 项转换/布局单测与 6 项显式 SHM 集成，真实 Viewer 0.38.1
展示曲线、事件、图像、点云及 Tensor，已核时间游标和实际 live Recording。
正常录制 1000 帧、10 类、11 实例、gap/drop/reject 为 0；有数据后 SIGINT 为 80 帧/10 类、
67 gap，worker 正常回收。真实断连负例失败退出并回收；观测有损和首连接饥饿限制保留。

Linux r6 的根 fmt/Clippy 与完整串行 workspace、13 项退出通过；r4 的 609 项 S0+rt_guard
及优化构建/真实 help 启动通过，r3 的 transport 默认/mock 与可视化专项通过。
620 输入最终 manifest `abb7029420438ed69d12b0aab541cea9558f027485af68c196d4342359f430d2` 前后相同。
r3 后仅修三处 Linux 测试：RPC/S1 Node 生命周期、退出日志颜色和父监督阶段计时；
生产 10 秒预算及 9.75–10.1 秒退出判据未改。原 Clippy、golden、夹具、节点和退出 FAIL
全部保留；不把串行通过用于关闭 S1 既有并发根因，ignored 和托管 CI 均不冒称通过。

两份接收器中断捕获缺 footer，官方 verify FAIL；桥自身正常录制 verify PASS，分别记账。
Viewer 实时加载初期一次 Timeline 警告保留。媒体仅软件夹具、GPU/字体/范围限制详
[V1–V8](../../docs/10_verification.md)。使用命令见[上手指南](../../docs/02_start_here.md#rerun-观测工具当前开发入口)，
证据入口为[摘要](../../.verify/robot_viz_2026-09-29/summary.json)、
[索引](../../.verify/robot_viz_2026-09-29/archive.json)和[原件包](../../.verify/robot_viz_2026-09-29/evidence.tar.gz)。

该阶段仅本地提交，不 push；Orin 的 SSH 信任条目当时待核，未传输/正式安装或新硬件动作。
严格 SSH 与 Orin 后续验证状态见页首；通信硬件门和 D15 退役决策仍待推进，
可视化先行完成不自动关闭这些条件。

**2026-09-29硬件小幅检查后的诊断分类软件门通过。** 两侧各一次腕部往返功能探针通过，
结束后17电机分总线读回断扭/零速、EStop双写恢复1，正式服务停止；头部/底盘只有状态
检查和guard安全写，未测主动运动。实际硬件产物仍为ca14冻结件，本地c6及本次诊断修复
未做硬件回归。原件和边界见[硬件检查及后续排查](../../docs/handoffs/hardware_smoke_2026-09-29_summary.md)。

每侧17条site5 Unknown由“约16微秒内16条＋Disable附近1条”组成；日志分类缺陷已确认，
Revoked权限窗口解释高度吻合，但缺原AbortReason/Enable提交时戳，不能逐条确证。
按用户要求新增穷尽类型分类，明确通信、设备、中止、参数、目标读回及重复连接，统一编码，
不改权限、故障返回和调度。本机fmt/定向全feature Clippy、147库＋5集成通过；原风格lint
及测试收集器首次惰性分配失败保留。隔离Lima Ubuntu 24.04 ARM/nightly-2026-09-29第四轮
fmt→根Clippy、Feetech147+5+12+3及SerialRT42通过，605项来源前后核验一致。
CLI/S0三处lint等价修正；前三轮lint/格式FAIL保留。原生快照不含Rerun/镜像。
该诊断阶段Orin因本机known_hosts缺失的两主机记录而被严格SSH拒绝，未传输/安装或新硬件动作。
读早退/过期tick、右H2、D15保持OPEN。
可恢复的软件验证入口为[诊断分类摘要](../../.verify/hardware_smoke_2026-09-29/diagnosis_summary.json)
和[本机证据包](../../.verify/hardware_smoke_2026-09-29/diagnosis_local_evidence.tar.gz)、
[Linux ARM软件证据包](../../.verify/hardware_smoke_2026-09-29/diagnosis_linux_evidence.tar.gz)。

### 同日先前软件阶段（已验收，以下为该阶段事实）

**2026-09-29软件阶段已验收：补齐最终Disable的真实tonic延迟回执回归。** 用户重新要求按进度继续开发，并允许直接在Orin编译测试。共享`robot_tests::comm_fault_rpc`现在同时供Linux工具与回环测试调用；业务2000ms、gateway grace 500ms、最终传输3s、普通RPC 2s均保持。同步测量工具的三项feature闭包校验，并完成4处等价nightly Clippy修正；冻结设计、S0/RR安全语义和硬件SLA未改。

源码基线`5213848`加本次变更；Orin独立目录`target/post_e_h2_rt/s49/rpc_transport_20260929_01`逐项核验603个来源（含辅助文件及符号链接），清单SHA `5fcbda255164cf91455d76183f2579abe737a2706c43ec36e634027220752539`。后续本地CLAUDE只更新状态文档，13项软件变更均与已验快照一致。未用本轮dev产物替代正式release测量件批准。

本机窄feature的2项新单测、6项回环及定向Clippy通过。Orin最终源码fmt、实际重新检查的全workspace Clippy、测量feature Clippy、lib 4项（含新2项）、工具182项、6项回环/9场景、66项Python及工具构建通过。正例实际等待2101190176ns后收到合法Ack；普通2秒连接负控、3300ms回执及错误Ack/状态均拒绝，9场景的服务均已join且active为0。绑定本轮冻结测试peer后，完整workspace串行回归通过；显式网络回归与默认忽略项分别记账。模拟服务不证明真实gateway grace、RR/Lower退休或物理断扭。

原失败保留：macOS全workspace存在Linux API限制，扩大Python测试也受Linux接口限制；首Orin默认workspace缺S1 `test-fault-backend`，两例失败。另一次默认并行S1库284 PASS/2 FAIL发生事件接收超时；14项隔离串行及完整串行回归随后通过，但具体并发根因仍OPEN，不能称默认并行全绿。同步tar的旧mtime可能命中缓存，0.44秒根Clippy不作为最终来源证明；刷新隔离源mtime后实际重查通过。

正式Orin源码仍为`ca14f0e`且clean，四个原安装件SHA、六服务inactive/dead/MainPID0及无控制进程的前后快照相同；未访问真实串口、正式安装或重测硬件。marker/latch本轮未重读，现场电源、支撑与接触区UNKNOWN。**右H2原run/report仍整体FAIL，D15 OPEN、`deployable=false`**。下一真实尝试仍需新源码/产物绑定、当次现场事实和独立GO；旧输出目录与旧GO不可复用。

本轮原件入口`target/post_e_h2_rt/s49/delayed_ack_20260929/`，持久摘要及独立小证据包见[软件验收摘要](../../.verify/post_e_comm_fault_hardening/S49_final_stop_transport.json)与[H.json](../../.verify/post_e_comm_fault_hardening/H.json)。旧H大包及历史FAIL保持原样；本轮按阶段规则提交，不push。

### 2026-09-25历史交接（原事实与失败保留）

**2026-09-25 交接状态：按用户要求暂停开发，仅完成本次 handoff/证据提交并推送。** 工作分支为`feat/post-e-comm-fault-hardening`；继续开发或启动真机测试须后续明确指令。左H1/H2/H3与右H1/H3已通过；右H2仍为**整体FAIL**，D15 OPEN、`deployable=false`，没有正式安装或退役。六个正式服务在最后一次Orin软件验收前后均为inactive/dead/MainPID0；这只是当时快照，不代表未来现场状态。

2026-09-23右H2真机本轮整体**FAIL**：runtime `1ac4548`、实际build仍`5952498`；四组各100预热+1000正式测量文件逐组PASS，SLA-B P99依序10.168864/6.811968/10.145344/6.914688ms，但最终非dry-run Disable约2.001秒返回`Cancelled: Timeout expired`，故原`run.json`/`report.json`均FAIL，不称右H2通过。EStop/Disarm及GetState安全态3、final_trace有效均为局部事实，不能代替DisableAck或物理断电。
本次authorize/launch各1，工具/driver/SSH均exit1且真实wait/reap；退出后独立只读清场PASS，六服务inactive/PID0、串口无占用、正式安装件不变，右双rx marker与右EStop latch均为`1\n`。清场时电源UNKNOWN；用户先回复“已物理断电”，随后澄清“已通电，电源和 USB 都重插”。**这是2026-09-23最后一次用户报告，截至本次交接物理电源现状未知**；实际动作时间未知，双臂支撑与接触区未重新确认，此重插不构成新H2授权。首udevadm瞬态FAIL、漏传alignment的零启动前置FAIL及其内部cleanup FAIL完整保留；补传alignment、重新签GO后的本次真实H2仍FAIL。
原件入口`target/post_e_h2_rt/s48/right_h2_closeout/evidence_summary.json`及其逐SHA引用；软件修复`ca14f0e`已提交推送。Orin源码ff-only到该提交，571输入聚合SHA为`ae5a04ff7243e9b6ae49dcc187e20e0f3db747f2d60cea426f8314eed4c9deae`；原生生产4件、测量3件构建通过，冻结测量件mock AC-12通过（最终Disable Ack），final定向9/9、Endpoint重跑1/1、Lower诊断和日志各1/1。首次Endpoint `--exact`只匹配0项，原件保留但不计通过；安装/受保护文件、USB映射、服务和进程前后快照一致。软件验收摘要见`target/post_e_h2_rt/s48/right_h2_software_ca14f0e_20260923T082518Z/acceptance_summary.json`。**仅软件验收通过**：未测2.1秒延迟tonic端到端、未访问真实串口/电机、未正式安装或硬件重测，不能判右H2通过或证明当前物理扭矩状态。
当前H包SHA `3337206531273c274c050875ab0545e2a98e5c172e0f5f050591b19d100fafab`，234996374 bytes、66143 members/66142索引。旧65918索引前缀保全，依次新增右H2原件115件、断电回执2件、通电澄清3件，再新增ca14软件原件104件（含7 ELF及sidecar；排除重复worktree）。软件归包回执在`target/post_e_h2_rt/s48/right_h2_software_ca14f0e_20260923T082518Z/post_archive_review/archive_receipt.json`，为包外后生件；本次最终独审与提交回执也由下一次归包承接。
下一开发者先从[H.json](../../.verify/post_e_comm_fault_hardening/H.json)及[H_evidence.tar.gz](../../.verify/post_e_comm_fault_hardening/H_evidence.tar.gz)核实失败和软件来源；若要重测右H2，应先补足2.1秒延迟回执的可控端到端覆盖并评审，之后重新核对Orin源码/产物、双臂支撑、负载与接触区、电源和USB物理边界、现场持续值守与断电手段，再取得**本次**独立GO。旧GO、重插答复和输出目录均不可复用；不因通信硬化自动关闭D15。

### 上阶段右H1/H3（以下为当时事实）

2026-09-23新右H1/H3各100预热+1000正式测量通过，运行HEAD `3790fd9`、实际build `5952498`、571输入aggregate `e05a2ca…ce0ff`；未重新构建或正式安装。
H1完整PING+Model_Number READ的P99/max为1.147712/1.171040ms；H3六ID断扭广播主机调用P99/max为0.256192/0.879104ms，1000正式样本均≤10ms。Enable帧0、drop/truncated0；H3不证明UART完成、机械断扭或物理断电。
H1真实reap及清场后才H3；两工具exit0/reaped，五次内部清场与退出后实际UID/串口/服务清场通过。driver通过远端shell直接exec及SSH exit0真实wait收口，未及时捕获独立driver PID，不补造。
唯一保护变化为右`rx_trust.lock`首次由原工具创建（0600、UID/GID1000、无活持有者）；右marker仍缺失、双臂latch与左保护身份不变。正式安装件未变、六服务停止。
首readonly命名冲突FAIL、首次setsid父wrapper被识别related的前置拒绝FAIL完整保留；前置拒绝H1/H3零启动，外层退出后清场通过，不回写原cleanup FAIL。本次另经用户继续和新root GO，非自动重试。
本轮原件入口`target/post_e_h2_rt/s48/right_h13_run/precheck_20260923T061818Z_06b35bdf/exec_delivery.json`；独审/root已验收并完成增量归档。
用户已撤销周余15%停线并确认本次右臂现场就绪；本轮值守已解除、电源UNKNOWN。右H2仍NOT_RUN且未授权，左H2历史PASS及c5daa67/fb600fd不变，D15 OPEN、deployable=false；每阶段独审/root验收后commit，不push。

本轮H包SHA `3a7aa0f7e0bbcccdc9ef9fb2e6c5f1412100ff3821dcc4c9fc294f69bc897f77`，208520620 bytes、65919 members/65918索引、5569545267 logical bytes；旧65607索引完整保全，新增311件。
已承接上一native/post_archive_review及commit；本轮`right_h13_run/post_archive_review`及随后commit为包外后生件，下包承接。

### 已收口右H13原生软件（3790fd9，以下为当时事实）

右H13准备已收口`9afdc92`。新软件验证运行HEAD仍为该提交，实际Rust build仍`5952498`、571项aggregate `e05a2ca…ce0ff`，没有重新构建或替换产物。
首Orin uv Python3.12.13缺`os.pidfd_open`，22方法3FAIL/3ERROR原FAIL保留。最新脚本为`target/post_e_h2_rt/s48/right_h13_native/implementation_r3/`：仅两pidfd API均可调用才启用；接口缺失走已审自有未回收Popen child路径，资源真异常仍FAIL。
r3本地27方法、独立5方法及新版Orin27方法/默认PLAN_ONLY入口通过；child/owner/SSH真实wait/reap，前后安装及安全状态相同、实际UID清场、收养child为空，不称单轮全过。
原件在`right_h13_native/software_20260922T154412Z_1417961a/`及`software_r3_20260922T155141Z_0ffb13e6/`；本轮证据已增量归档，提交身份由随后真实commit回执记录。
仅软件；右H1/H3/H2 NOT_RUN、未安装/部署。左H2历史PASS及c5daa67/fb600fd不变，电源UNKNOWN、D15 OPEN、deployable=false；用户暂不能现场配合，继续软件至周额度剩15%，每阶段验收commit、不push。

本轮H包SHA `616b81eac359e0bc99a9821df8e5cf6c4a2acde4ddd8ca3e880d5894b625cfcd`，206486632 bytes、
65608 members/65607索引、5561014616 logical bytes；旧65452索引完整保全，新增155件。
上一preparation包后/commit已承接；本轮`right_h13_native/post_archive_review`及随后commit回执为包外后生件，下包承接。

### 已收口右H13本地准备（9afdc92，以下为该阶段事实）

当时右H13独立操作脚本已完成本地软件验证：开发22方法及独立12方法PASS，未改既有三Python或Rust。
脚本默认plan-only；执行另需本次右现场事实与绑定输入，H1真实reap/原件核验/清场后才H3，180秒原阶段门不变。
SIGKILL后仍未reap会记录需介入并继续持有wait，不伪造完成。原始CSV/帧oracle与无害子进程反例均已独审。
开发round6因新增过期反例未建临时输出目录而FAIL，修夹具后的最终轮通过；首两轮仅log/工具终态，缺独立PID/wait与源码快照回执，原边界完整保留，不称单轮全过。
证据入口`target/post_e_h2_rt/s48/right_h13_preparation/`，本阶段证据已归档；最终源码身份见implementation_receipt_r2及独审回执。
右TRUST目录仅经授权从0755修为0750，marker/locks仍未创建；用户暂不能现场配合，右H1/H3/H2均NOT_RUN，未安装/部署。
左H2 PASS及runtime c5daa67/build fb600fd保留，收尾电源UNKNOWN；D15 OPEN、deployable=false。持续推进至周额度剩15%，每阶段独审/root验收后commit，不push。

本次H包SHA `178a5501086efa9bb5511e45c4174bfa55dd1a9caae9b7baeeae421000743403`，206206186 bytes、
65453 members/65452索引、5559918998 logical bytes；旧65321索引完整保全，新增131件。
该preparation阶段的post_archive_review及commit回执已随本轮native包承接。

### 已收口原生软件阶段（4441f59，实际build仍5952498）

左H2证据已提交`10cff7a`；右工具`0047920`与W1测试`2e5b515`已合并为`5952498`。
合并态本地64项Python、完整最终SHA的latch默认并行10项、fresh fmt/根Clippy通过，命令真实wait/reap。
本次10项绑定最终`7bf685e8…a8288`，不覆盖旧测试后等价lint记录或旧偶发失败的OPEN_UNCONFIRMED结论。

Orin新软件source/build均`59524984368ad74b17384ca805b105102a29caac`，571编译输入aggregate
`e05a2ca189b0f85873d3f780c94329ec03e739028031eb3b36fd5babdd9ce0ff`。6条release recipe产出生产4/测量3，
7help、64Python、默认并行latch10与唯一AC12 mock四组各2通过；18次completion均真实退出回收，122原件逐SHA/bytes核验。
final_trace在双child停止后valid且与报告严格一致，非dry-run DisableAck/Disarm、实际UID清场、来源/脚本/安装件/左状态不变均通过独审。
旧57项本轮未重跑；ELF本体未回传，身份按远端冻结原件及Cargo JSON/profile/features/sidecar核验。
独立复核初次误用Physical sequence读取Mock epoch的KeyError保留，随后用明确epoch映射验证原trace，未改原件或判据。

证据入口为`target/post_e_h2_rt/s48/h2_arm_native/`的root_raw_checks和review/final_review；
本轮native与`h2_arm_integration/`、上轮`h2_retry/post_archive_review`及commit回执已增量归档：
SHA `72fff7b1a42ca4879f6a25d319d21d0c5a0a300bba49204300223007b8560898`，205856277 bytes、
65322 members/65321索引、5558673967 logical bytes；旧65096条完整保全，新增225件。
该阶段已收口`4441f59`；`h2_arm_native/post_archive_review`及commit回执已随本次右H13软件包承接。
左真机runtime `c5daa67` / build `fb600fd`及其PASS不变；新版只做软件mock，未安装或操作硬件，右H1/H2/H3未测，D15 OPEN、deployable=false。
用户暂不能现场配合，当前只继续软件准备，右硬件不启动。用户恢复工作并将停止线由25%改为周额度剩15%；每阶段独审/root验收后commit、不push，新的物理动作仍须当次现场事实。

### 已收口左H2（10cff7a，以下保留该提交当时的事实）

2026-09-22本次新左H2已由独审/root验收：runtime `c5daa67`、实际build `fb600fd`，
571编译输入aggregate `52b1832…58d17`及三Python身份按原门继承。12:17:08至13:13:12 UTC，
约56分钟，单次authorize/launch各1；四组各100预热+1000正式全部通过，4400 attempts、
290158 events、6834条InjectedDelay无短于请求，分类重试0/invalid0。背景66986唯一新写、恢复8800写。

| 顺序 | SLA-A P99（ns） | SLA-B P99（ns） | SLA-B max（ns） |
| --- | ---: | ---: | ---: |
| topic/idle | 24896 | 10331104 | 11370176 |
| topic/inflight | 24864 | 7055872 | 13453376 |
| external/idle | 21024 | 11111744 | 52502592 |
| external/inflight | 22176 | 6797440 | 7441344 |

第三组max为52.502592ms，原样保留；原门为P99，不能把通过解释为每个样本均≤20ms。
独立EStop/Disarm/GetState3及非dry-run DisableAck通过，final_trace在双child stop后valid。
工具/driver均exit0并真实reap；实际UID进程清场、六服务停止、串口无占用、双marker Dirty，
源/产物/正式安装/配置/设备前后身份已核。收尾后电源UNKNOWN，本轮值守已解除。
三次旧真实H2 FAIL和一次前置拒绝保留；旧`hardware.left.H2`完整移入`H2_previous_runs`，不称历次全过。

本轮入口`target/post_e_h2_rt/s48/h2_retry/`的result/root_raw_checks/review/final_review与root_acceptance；
36远端原件逐SHA回传，61本地owner文件纳入审计。本轮已归包并提交10cff7a，65097成员/65096索引，新增376件且旧64720条完整保全；
本轮post_archive_review与commit回执为外置后生件，下包承接。
右工具`0047920`的64项与独审、W1测试`2e5b515`的默认并行10项与独审均已完成，
仍仅隔离分支，未合入/部署，不是本轮runtime/build。W1探针证明fork至exec fd继承窗口，
原偶发失败具体根因仍OPEN_UNCONFIRMED；10项测试后仅fmt及等价lint调整，原失败记录保留。
安装准备仅只读66原件，未安装。右H1/H2/H3仍未测，D15 OPEN、deployable=false。
按25%额度线持续推进、每阶段独审/root验收后commit，不push；新的物理动作须新现场事实。

### 已验收延时软件阶段（收口c5daa67，以下保留当时结论）

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
原连接竞争与双环境验证已在C/A/B依次闭环。P-E、P.7及前置/延时修复已验收；本次左H2通过，旧三次FAIL保持。左H2已收口10cff7a，右/W1合并5952498及Orin新来源验证/独审通过，本次右H1/H3也已验收归档，后续右H2/部署门仍未闭合；用户已撤销15%停线，每包检查与独审/root验收后commit。
RR r3析构与公共CAL两项方案已ACK，直接替换现有工厂，保topic/schema/FF；
所有peer与自动清理者同批同源，模型/迁移实现按分包开发和评审。
旧跨原子可见性与映射删除问题已在R1–R3实现，R4双环境软件验证已通过；不外推额外内存序证明或完整候选闭环。
W1测试隔离已本地集成并对最终SHA另跑10项通过；旧锁偶发失败仍列OPEN_UNCONFIRMED，不由机制探针或10项通过倒推具体根因。
P-E保原20/s、50ms节拍、360–600秒、成功唯一新写>8192、真实wire4与所有结果final_stop。

## 现场边界

左H1/H2/H3 PASS；旧三次已采样H2 FAIL与一次driver前置拒绝原样保留。
当天已确认左臂断电重上电与USB重插、双臂支撑；收尾后电源未新确认。S-46本地8192长测通过，
Orin真实882唯一写时wire4满FAIL；右臂未测，六服务最后停止，D15 OPEN、deployable=false。
R4/P-E软件运行没有更新物理状态；9/22事实仅绑定已结束attempt，不再要求本次继续值守。
新使能前须取得新的电源/USB重置与持续在场事实，
不复用旧H2驱动或已消费授权。Orin为`loosqk@10.0.1.40`，调试/RT必要时直接sudo；
标定沿Orin已核Python值，串口按原D2 by-id身份。候选后续顺序保持D15→Rerun桥→F7。
