# 交接 · post-E 通信故障硬化

## 当前步骤

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
