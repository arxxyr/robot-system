# Spec · v0.5 候选 E — 替换 dimos-deploy 机器人控制执行面（round 12）

> CodeMap：[`team-swe/codemap/v0_5_dimos_replace.md`](../codemap/v0_5_dimos_replace.md)
> （round 4 **Approved**，amend 记录 round 1-3 = B1-B10 全闭）。Spec round 1
> 按 reviewer 指示**先拍 Q10/Q11/Q12 三项 P0**（D1/D2/D3），再 D4-D12 覆盖
> 其余 Q + AC 草案 + hard boundaries；round 1 复核 = **方向拍定（D1 (a)+(i) /
> D2 (i) / D3 (c)，条件生效）+ 4 P0 阻断 + 3 执行语义阻断**，round 2 逐条
> 回修；round 2 复核再出 **7 执行级阻断（T1-T7，P0 ×4 + P1 ×3）**，round 3
> 逐条回修；round 3 复核（T4/T6 闭环）再出 **6 项（U1-U6，P0 ×3 + P1 ×3）**，
> round 4 逐条回修；round 4 复核（U3/U5/U6 闭环）再出 **4 项（V1 P0 +
> V2-V4 P1）**，round 5 逐条回修；round 5 复核（V3/V4 闭环）再出 **4 项
> （W1 P0 + W2-W4 P1）**，round 6 逐条回修；round 6 复核（W1/W2/W4 闭环）
> 再出 **2 项（X1/X2 P1）+ 非阻断 1**，round 7 逐条回修；round 7 复核
> （X2 闭环）再出 **2 项（Y1 P0 + Y2 P1）**——含 reviewer 对自身 r7
> infinity 推荐的更正——round 8 逐条回修；round 8 复核（Y1/Y2 方向确认）
> 再出 **2 项（Z1/Z2 P1）**，round 9 逐条回修；round 9 复核（Z2 闭环）再出
> **3 项（AA1 P0 + AA2/AA3 P1）**，round 10 逐条回修；round 10 复核再出
> **1 项（AB1 P1）**，round 11 回修；round 11 复核再出 **1 项（AB2 P1
> 措辞冲突）**，round 12 回修后 **Approved**（均见"Spec round 记录"）。
> **状态：round 12 Approved + D1-D17 终拍 ✅ + D15-R 拍 (c1+) ✅
> （2026-07-15，round 7）**。**round 7 范围收缩（reviewer）：候选 E 目标
> = 受监督功能 demo，生产级安全闭环后置**——段(5b)/(9b) 移出（D14 四检测
> 器归档 post-E "生产安全硬化候选"），**D15 保持 OPEN**，AC-E25 不入
> demo gate，D-BI3 正式退役推迟；执行序收敛
> **(1)→(2)→(3)→(4)→(5)→(6)→(8)→(7)→(9)→(10-demo)**。**round 8 闭环
> 回修：§6.7 拆分口径（#2-#5 全保 + #1 S0-dead 半句 demo 例外）/
> per-arm guard 本 bus 全量 ensure-safe / 旧退役指令清零 / demo 数值
> 判据 + baseline 宣称收窄**（round 9 对账全过）。**round 9 回修：
> 部署 preflight API 改 per-bus 形态（本地 `DeployArm`、
> `ensure_safe_arm`/`ensure_safe_handoff` 双 transport 聚合、
> 双 config CLI 写死、反向测试两条）**。**round 10 回修：config 路径
> 与 D2 权威名统一（`/etc/robot/xlerobot_{left,right}_arm.toml` 三处
> 一致 + 安装映射 + unit 字面锁）/ 右 bus 写数改准 12 条（合计 20）+
> 有序序列断言 / `PreflightFailure::{Connect,Operation}` 类型契约
> （FailedTransport 方案撤回）**。
> **当前阶段：✅ Plan Approved — supervised-demo scope（reviewer，
> 2026-07-15，round 10 复核零新增阻断）**。批准边界：① D15 硬件
> fail-safe 继续 OPEN，AC-E25 不属于 demo gate；② 不执行旧栈正式退役 /
> disable / 删除，不宣称生产级或无人值守完成；③ 执行严格串行
> (1)→(2)→(3)→(4)→(5)→(6)→(8)→(7)→(9)→(10-demo)；④ **实施期任何语义
> 偏离先 Reverse Sync，不直接改 Spec 或继续执行**。
> 前置链：D-BI0~3 拍板（2026-07-13）+ CHANGELOG 2026-07-13 premise 登记。

## Spec round 记录

### round 1 → round 2（reviewer 复核：4 P0 + 3 执行语义阻断 + 非阻断 ×2）

| # | 级别 | 阻断 | 修订 |
|---|---|---|---|
| S1 | P0 | **D3 最终 target 可能从未发布**：dwell_ms=0 末 waypoint 填 tick_buf 后直接 advance → `Completed`（`scripted.rs:331-341` + `:435-445`），coordinator 对 Completed 帧不 publish（`coordinator.rs:1712-1730`）——"终态后 hold = 最后 target"不成立；且 r1 把 Idle/Degraded 一起动态化、"每次 publish 更新"连带改 IK 完成后行为（`ik.rs:315`） | D3 r2：**发布保证**（末 waypoint 先发 Continue、下一 tick 才 Completed）+ 更新规则**收窄为仅 Success-Scripted 且末 target 已 publish** + **行为矩阵**（IK / HoldPose / Degraded / cancel / failure 全维持现状） |
| S2 | P0 | **D1 编码与配置输入不构成权威**：NUL 分隔无长度/joint_count/domain/version、u64 端序未锁；S0 `motor_ids` 走 extras `filter_map` 静默丢弃非法值（`s0_daemon.rs:1163`）；6 个 config + e2e 动态模板（`case_ctx.rs:306`）无 motor_ids——"既有 harness 天然一致"不成立 | D1 r2：**长度前缀规范编码**（domain/version + count + ordered(name,id)，`u64::from_le_bytes` 显式）+ motor_ids **typed 化全套 fail-fast 校验** + mock **显式 ID 策略** + `[robot].id == --robot-id` 身份锁 |
| S3 | P0 | **D2 把运行身份与 S0 部署留给 Plan**：身份进 topic / layout hash / gateway config / latch 路径；现有两 S0 unit 仍启动 full config（`xlerobot-s0-bus1.service:55` / `bus2:115`），D12 却只加 S1/gateway unit | D2 r2：**身份全表锁定**（robot/hardware id、config 文件、串口、latch、gateway 端口、unit 名）+ **S0 arm-only unit ×2** 进 D12 + 旧 full unit 保留回滚 |
| S4 | P0 安全 | **排除 head/base ≠ 风险"整体消除"**：arm-only S0 不驱动也不能 EStop 这些电机；旧栈退出只 best-effort 断扭且吞失败（`_manipulator_base.py:184-192`）、底盘还需零速+断扭（`bus2_base.py:166+`） | 新增 **D13 fail-closed preflight**（读/写/readback 任一失败禁止切换 + 旧栈回滚保留）；D2 "整体消除"表述**撤回** |
| S5 | 执行 | **D4-D6 缺双臂成对与部分失败语义**：冻结实现逐臂 submit 立即返回 = 实际并行（`executor.py:95-107`）；gripper 从每 ~100ms 阶跃写变 Scripted 线性插值 = 有意语义变化不能宣称等价 | D5 r2 **成对协议**（并发提交 / 最大启动偏差 / 双终态 / 部分失败 cancel / 不收敛双 EStop）；D6 r2 **语义变化登记** + AC 锁 |
| S6 | 执行 | **D11 输入契约不完整**：冻结 12 维 = 臂"度" + gripper [0,100]（`executor.py:55-68`），r1 只定义 [0,1]→rad；NaN/Inf、越界、range 反转失败语义与 raw↔rad 权威未锁 | D11 r2：helper **rad-native** + **基线导入 shim**（原始 12 维：度 + [0,100]）+ 失败语义全套 + raw↔rad **唯一权威 = feetech crate**（robot-rpc 不复制公式） |
| S7 | 执行 | **D12 Type=notify 不可留 Plan**：design 冻结明确 S1/gateway sd_notify + watchdog（`00_design.md` §9 表），S1 现状显式 Type=simple（`s1_daemon.rs:54` B1 决策 #7） | D12 r2 **直接拍：纳入** S1/gateway liveness-aware sd_notify + watchdog（`Type=notify` + `WatchdogSec=5s`）；B1 决策 #7 显式解锁登记 |

非阻断 ×2（已落字）：D9 共享 case 库住 **robot-rpc client helper**、robot-rpc/tests 与 robot-tests 同向消费（**禁** robot-rpc 反向 dep robot-tests）；D3 新常驻线程正常 **stop+join+Drop**（iceoryx2 0.9.3，**禁** mem::forget 回潮）。

### round 2 → round 3（reviewer 复核：7 执行级阻断〔P0 ×4 + P1 ×3〕）

| # | 级别 | 阻断 | 修订 |
|---|---|---|---|
| T1 | P0 | **新 robot_id 的 EStop latch 迁移未定义**：D2 引入全新 latch 路径，照搬既有 runbook `0\n` provision（`scripts/systemd/README.md:37` 区块）= 借更换 robot_id 丢掉旧 EStop 记忆 | D2 增 **latch provision 锁**：首次 `1\n`（latched）primary+bak 双写 / 已存在绝不覆盖 / 首启 latched Safe 仅 maintainer+ ClearEStop 清 + AC-E21（迁移/幂等/损坏文件） |
| T2 | P0 | **D13 只是 runbook 约束可被绕过**：六 unit 可独立启动且带 Conflicts=，`systemctl start xlerobot-s0-left-arm` 停旧栈并绕过 preflight | D13 增 **systemd 强制**：fresh one-shot preflight unit + S0 `Requires=`/`After=` 绑定，失败 → 新栈全 inactive；"systemctl start 即回"改**保留人工回滚能力**（硬件 I/O 已失败不承诺旧栈必然安全恢复）+ AC-E22 |
| T3 | P0 | **D3 "非成功不更新"与矩阵"回启动 anchor"矛盾**：一次 Success 把 target 改成末点后两条不能同时成立 | 拍定：非 Success 终态**显式 reset idle_hold_target = 启动 anchor**（严格维持旧观测语义；"保留 last-success"备选未采）；AC-E18 必测"先 Success 再分别 cancel/fail/IK"序列 |
| T4 | P0 | **D12 watchdog 误判合法空闲**：LivenessObserver 要求计数每周期增长（`systemd_notify.rs:139` 语义），RR metrics 只计真实请求（`rr_io_thread.rs:124`），tonic serve 无进度计数 | D12 锁：request-driven worker 用**独立 loop_tick_count**（空闲 poll 也推进）/ tonic 用 **loopback health probe** / `READY=1` 仅全 observer ready 后 / shutdown 先 `STOPPING=1` 再停 watchdog + AC-E19 双向（零请求不重启 + 冻结每类 worker 必重启） |
| T5 | P1 | **双臂协议缺执行期不对称终态**："Completed 家族"不精确（实际终态 = Completed/Failed/Cancelled，`action_status.fbs:57`）；启动偏差无观测点（proto TaskHandle 无 accepted_at_ns，`task_handlers.rs:760` 仅 audit） | D5 重写：pair success = 双侧 `Completed(Success)` / 执行期一侧 Failed/Cancelled/Deadline → cancel 另侧 / 一侧 Success 后另侧失败 = partial completion 显式报告 / stream 断开 → `GetTaskStatus` 收敛 / **双拒绝不自动重试**（chunk FAIL 确定性终止）/ 偏差 = 两侧 `Started.at_ns` 差 |
| T6 | P1 | **D11 依赖事实不成立**：robot-tests 无 feetech 依赖（`robot-tests/Cargo.toml` [dependencies]）；`raw_to_rad`/`rad_to_raw` 是私有 fn（`adapter.rs:92`） | D11 更正：feetech **暴露纯公共换算 API**（adapter 自身复用）；robot-tests **optional 依赖挂 real-hw**；依赖变化进 Plan `cargo --timings` + deny gate |
| T7 | P1 | **D1 统一 exit 2 冲突 B1 冻结口径**：S1 配置加载/解析失败 = exit 3（`s1_daemon.rs:39` 文档 + `:294` 实现） | D1 exit 口径拆分：**CLI 身份/参数冲突 = exit 2**；**config 内容错误沿各 daemon 既有码**（S1 = exit 3）——避免第三处 B1 解锁 |

### round 3 → round 4（reviewer 复核：T4/T6 闭环；6 项〔U1-U3 P0 + U4-U6 P1〕）

| # | 级别 | 阻断 | 修订 |
|---|---|---|---|
| U1 | P0 | **单一 preflight oneshot 无法保证启动拓扑**：Conflicts= 不保证旧栈先停；错峰启动时共用 preflight 会再开已被第一条 S0 占用的串口；latch provision 不在 systemd 事务内 | D13 改**每臂 `ExecStartPre` 两步**（本 bus 范围 preflight + latch provision，同 unit 事务内、watchdog 重启也重跑）+ 旧栈停止顺序锁（`Conflicts=`+`After=` 旧 unit）+ S1/gateway 依赖图锁定；AC-E22 扩四场景（同时/错峰/直启/watchdog 重启） |
| U2 | P0 | **latch provision 与现有恢复语义冲突**："ClearEStop 后 Ready"错误（Safe+ClearEStop→Safe，`state_machine.rs:578`；再 SetEnabled(true)→Ready→heartbeat→Enabled）；"损坏全 strict fail-closed"不符 loader（primary 缺/损 + backup 合法会恢复，`estop_latch.rs:95`） | D2 修正状态链 + **完整 truth table**（双缺失/单份合法/单份损坏/双份损坏/双份值冲突）+ provision 规则改"仅双缺失时写入"；AC-E21 对齐 |
| U3 | P0 | **preflight "先读非零才写"不安全**：首读失败即不尝试断扭、其余电机可能不再处理 | D13 ensure-safe 改**无条件 best-effort 安全写全部 omitted motors**（bus2 顺序 velocity=0 → torque=0）→ 逐项 readback；单项失败继续其余电机、末尾聚合失败禁止启动；预读仅作审计不作门 |
| U4 | P1 | **AC-E14 启动偏差可被降级判据绕过**：Started 晚订阅可错过是设计内（keep-last 单 slot + GetTaskStatus catch-up，`task_rpc_e2e.rs:124` 注释） | 正式**降级**：偏差 gate = **InvokeTask Ack 偏差**（client 并发 join 提交，可控可测）；Started.at_ns 双侧**若收到仅作诊断记录**，缺失不判失败、不得充当启动偏差 gate |
| U5 | P1 | **D3 更新规则非穷尽式**："任何非 Success"+"IK 任意结局"+矩阵 HoldPose 行三处并列易漏 | 锁**单一穷尽规则**：仅 `Scripted × Completed(Success)` 提交末点；**其余所有 backend × 终态组合一律 reset 启动 anchor**；AC-E18 序列加 HoldPose cancel |
| U6 | P1 | **S0 退出码未锁死**：Args parse/resolve 失败 = exit 2（`s0_daemon.rs:396-405`），adapter 构建失败 = exit 1 | D1 3' 补 S0 明码：config/CLI 错误 = **exit 2**（parse/resolve 路径），adapter 构建失败 = **exit 1**（区分）；进 AC-E3 |

### round 4 → round 5（reviewer 复核：U3/U5/U6 闭环、U2 主链对齐；4 项〔V1 P0 + V2-V4 P1〕）

| # | 级别 | 阻断 | 修订 |
|---|---|---|---|
| V1 | P0 | **单臂直启留下另一 bus 安全空窗**：旧 coordinator 同占两 bus，启动任一新 S0 即 Conflicts 停整个旧栈，但每臂 ExecStartPre 只查本 bus——仅启动左臂时 bus2 wheels 长期遗留旧栈状态 | 新增**全局 handoff unit**（oneshot + `RemainAfterExit=yes`，每 boot 首次激活跑一次**双 bus 全量 ensure-safe**，并作旧栈停止顺序第一责任人）；两 S0 unit `Requires=`+`After=` 之；每臂 ExecStartPre 降为 **restart 路径守门**（只触本 bus）；AC-E22 增"旧栈 active → 仅启动一臂"场景 |
| V2 | P1 | **latch 双写 partial failure 无闭环**：既有双写允许"primary 成功、backup 失败返 Err"（`estop_latch.rs:183` 区块）；r4"任一份存在即跳过"→ 重启后单副本起跑，与"双份"宣称不符 | provision 规则改**三分支**：双缺失写 `1\n` 双份 / **单份缺失以既存值补写缺失份**（只复制不改值）/ 双份存在不动；AC-E21 增"backup 写失败 → 重启补写成双份"路径 |
| V3 | P1 | **Ack 降级残留三处旧口径**：D5 item 1 仍"最大启动偏差"、item 8 仍 Started.at_ns 作偏差；且错过 Started 的真实机制 = gateway `latest()` 把缓冲历史塌缩到最后一帧（`events_subscribe.rs:281` + `subscriber.rs:9`），非 QoS history_size=1 | 三处口径统一为 **Ack 偏差**；机制表述更正（latest() 塌缩）；增**边界声明**：Ack gate 只约束 RPC 接受完成偏差，**不证明实际动作启动偏差** |
| V4 | P1 | **gateway 对 S1 只 After= 不拉起**：直启 gateway 会 READY 但 task RPC 无后端 | 依赖图修正：gateway = `Requires=`+`After=` 同臂 S0 + **`Wants=`+`After=` 同臂 S1**（自动拉起 S1、S1 重启不停 gateway） |

### round 5 → round 6（reviewer 复核：V3/V4 闭环；4 项〔W1 P0 + W2-W4 P1〕）

| # | 级别 | 阻断 | 修订 |
|---|---|---|---|
| W1 | P0 | **handoff 仍未闭合单臂启动空窗**：r5 "双 bus 全量"实际只覆盖 omitted motors（bus1 7-8 + bus2 7-9）；旧 coordinator 管**两臂全部电机**且退出断扭吞错——仅启动左臂时**右臂 ID 1-6 可能仍带扭且无人接管**（反向同理）；"另一 bus 归另一臂 S0 管辖"在单臂场景不成立 | **拍死**：handoff 首次切换安全写并 readback **全部 17 电机**（bus1 ID 1-8 断扭；bus2 ID 1-6 断扭；bus2 ID 7-9 先零速后断扭）；"禁单臂强制成对"备选未采；AC-E22 场景五删"或启动被拒"歧义、断言精确 ID 集 |
| W2 | P1 | **provision "单份缺失"未限定现存副本合法**：primary 损坏 + backup 缺失同属"单份缺失"但无可复制合法值 | 分支收紧：唯一现存副本**严格解析为 0/1** 才原子写 + fsync 缺失副本；现存损坏/不可读/值非法 → provision **失败且不得创建另一份**；只写缺失文件、**不经 `save_estop_latch` 重写现存副本**；AC-E21 增两个"损坏 + 对侧缺失"方向 |
| W3 | P1 | **"失败保持 inactive"与 systemd 语义冲突**：`Restart=` 同样作用于 ExecStartPre 失败 → 自动重试而非稳定 inactive | 沿 S0 既有策略拍定：`RestartSec=2s` + `StartLimitIntervalSec=60s` + `StartLimitBurst=5`；失败期间**绝不执行 ExecStart**、有限重试后进入 **failed**；AC-E21/E22 覆盖瞬态恢复 + 持续失败限流 |
| W4 | P1 | **部署清单仍写 unit ×6**：新增 handoff 后三处清单 + 拍板表未更新；runbook 仍写独立"stop 旧栈 → preflight" | 统一为**运行时 unit ×6 + `xlerobot-handoff.service` ×1（共 7）**；runbook 以 **handoff transaction 为旧栈停止与首次全总线安全交接的权威入口** |

### round 6 → round 7（reviewer 复核：W1/W2/W4 闭环；X1/X2 P1 + 非阻断 1）

| # | 级别 | 阻断 | 修订 |
|---|---|---|---|
| X1 | P1 | **`StartLimitIntervalSec=60s` 保证不了"五次后稳定停止"**：Feetech 单包默认超时 1s（`registers.rs:43` + `bus.rs:199` 逐电机），右臂 preflight 最坏 ~12 次串口操作 ≈ 单次尝试 ~12s，加 `RestartSec=2s` 后第 6 次启动可能滚出 60s 窗口 → 永远达不到 start-limit、无限重试 | 拍定推荐案：**`StartLimitIntervalSec=infinity` + `StartLimitBurst=5`**——第 5 次失败后下一次启动被拒，人工修复后 `systemctl reset-failed`；**与既有 S0 unit 60s 值偏离 → CHANGELOG 登记**；备选"给 preflight 可证明总时限"未采（`TimeoutStartSec` 中途杀 helper 破坏聚合语义） |
| X2 | P1 | **两处生效中的旧 AC 钩子**（非历史记录）：D2 钩子仍"unit inactive → 重启后补写"（缺自动重试/start-limit/损坏单副本路径）；D13 钩子仍 r4 四场景（缺 17 电机场景与重试终态） | 两处节内钩子改为**引用 AC 草案总表对应行为唯一判据**（不再复写，防双源漂移；总表已含全部 r5-r7 修订） |

非阻断（已落字）：W2 "原子写 + fsync" 细则明确 = **tmp 写入 → file fsync → rename → parent-dir fsync**（与既有 latch 持久化契约一致，新建缺失副本后掉电不丢目录项）。

### round 7 → round 8（reviewer 复核：X2 闭环；Y1 P0 + Y2 P1〔含 reviewer 自纠 r7 infinity 推荐〕）

| # | 级别 | 阻断 | 修订 |
|---|---|---|---|
| Y1 | P0 | **`StartLimitIntervalSec=infinity` 累计所有启动而非只累计失败**（systemd 定义 = 限制启动尝试**总数**；reviewer 本机实测：初次成功 + 4 次成功 restart 后第 6 次启动即 start-limit-hit）——若干次正常 watchdog 恢复后 S0 **永久拒绝下一次恢复，违反 §6.7 #2**。r7 推荐系 reviewer 审查错误，本轮更正 | **预算拆分**：① 运行期崩溃/watchdog 风暴 = unit 级 `60s/5`（**维持既有 S0 值，r7 偏离登记撤销**）；② preflight/provision 连败预算 = **ExecStart guard wrapper 自管独立连续失败计数**（成功清零 + exec s0_daemon；第 5 次失败退**专用退出码** + `RestartPreventExitStatus=` 阻止自动重启——该指令只作用于主进程，故 preflight/provision 从 ExecStartPre **迁入 ExecStart guard**）；AC 四锁 |
| Y2 | P1 | **parent-dir fsync 宣称强于现有实现**：`estop_latch.rs:333-335` 目录 open + `sync_all()` 均 best-effort 忽略错误——r7 "掉电不丢目录项"保证无依据 | 拍定 **provision 专用严格语义**：目录 open / fsync 任一步失败 → 返回错误、禁止启动 S0（强于运行期路径并明示对比；运行期 latch 写 **0 改**） |

### round 8 → round 9（reviewer 复核：Y1/Y2 方向确认；Z1/Z2 P1，无新 P0）

| # | 级别 | 阻断 | 修订 |
|---|---|---|---|
| Z1 | P1 | **guard 计数状态未达可执行级**：缺实例隔离路径、owner/mode、跨 restart 保留方式、I/O 异常 fail-closed、原子更新与单实例锁、专用退出码具体值（不得与 s0_daemon 0/1/2 重叠，`s0_daemon.rs:36-41`）；AC "6 次成功"未注明与独立 60s/5 的关系 | 计数硬契约六条落字（路径 `/run/robot-system/preflight/<robot_id>.count` / guard 自建目录不用 `RuntimeDirectory=` / 0750/0640 owner=unit User / 读损坏、更新失败、清零失败全部退停止码 / tmp+rename 原子 + flock 单实例 / **专用码拍 86**）；AC-E22 六次成功注明只断言 guard 计数、须避开独立 60s/5 正常限流 |
| Z2 | P1 | **严格 dir-fsync 可被重试绕过**：write+rename 成功 → dir-fsync 失败 → 重试命中"双份存在不动"→ 未确认的目录持久化被绕过 | **统一最终步骤**：严格 parent-dir open+fsync 移到三分支收敛后必经（含"双份均存在"路径）；AC-E21 增"首次 dir-fsync 注入失败 → 第二次重试仍必须执行并成功 dir-fsync 后才 exec"序列测试 |

### round 9 → round 10（reviewer 复核：Z2 闭环；AA1 P0 + AA2/AA3 P1）

| # | 级别 | 阻断 | 修订 |
|---|---|---|---|
| AA1 | P0 | **guard 无权创建 /run 一级目录**：S0 unit 以普通 User 运行（`xlerobot-s0-bus1.service:50` / `bus2:110`），/run 为 root:root 0755——`mkdir -p` 首启即退 86 | r9 "guard 自建、不用 RuntimeDirectory="**撤回**；改 systemd 原生 **`RuntimeDirectory=robot-system/preflight` + `RuntimeDirectoryMode=0750` + `RuntimeDirectoryPreserve=yes`**（restart 与 stop/start 均保留、reboot tmpfs 清零）；AC 覆盖首次创建/restart 保留/stop-start 保留/reboot 清零四态 |
| AA2 | P1 | **flock(count)+rename(count) 不成锁**：flock 绑旧 inode，rename 后竞争进程对新 inode 拿锁同时成功（reviewer 本机最小实测复现） | 改**稳定锁文件** `<robot_id>.lock`（永不 rename/删除）：flock(lock) → 持锁读/跑/原子更新仅 count → **exec 前显式关锁 fd 或 CLOEXEC**；只 rename count 绝不替换 lock inode |
| AA3 | P1 | **计数初态与 86 永久保留缺失**：/run 开机必然无文件；`RestartPreventExitStatus=86` 在 exec 后同样作用于 s0_daemon | **NotFound = 0**（正常初态）；canonical 范围 **0..=5**，其余读取错误/非法格式/越界 → 退 86；读到 count==5 → 直接停止态；**86 声明为 s0_daemon 退出码契约永久保留码** + AC-E3 回归锁（防未来 daemon 误用 86 失去 watchdog 重启） |

### round 10 → round 11（reviewer 复核：AA1-AA3 + Z2 全闭环，无新 P0；AB1 P1）

| # | 级别 | 阻断 | 修订 |
|---|---|---|---|
| AB1 | P1 | **失败后才递增可被崩溃绕过**：guard 在慢 preflight 中 panic / SIGKILL / 异常退出时计数不变——~12s/次 + `RestartSec=2s` 仍滚出 60s/5 窗口无限重试，**X1 问题重新暴露** | 改**"未成功 guard 尝试计数"（pre-increment）六步**：持锁读 n〔n==5 即退 86〕→ **执行任何 preflight/provision 前原子写 n+1** → 两步成功后清零再 exec → 正常失败且值==5 退 86、不足 5 普通非零退出 → **中途崩溃保留已递增值下次继续累计**；AC-E22 增"递增后、完成前连续崩溃"注入 |

### round 11 → round 12（reviewer 复核：AB1 六步/分支/注入闭合；1 项 P1 措辞冲突）

| # | 级别 | 阻断 | 修订 |
|---|---|---|---|
| AB2 | P1 | **掉电语义与 /run 契约冲突**：AB1 步骤 ⑤ 写"掉电内保留已递增值"，与 AA1 "reboot 因 /run tmpfs 清零"矛盾——计数不具备掉电持久性 | 步骤 ⑤ 更正为 reviewer 句式：**同一 boot 内进程终止保留已递增值；掉电/重启按 RuntimeDirectory 契约清零，并重新执行 handoff 与 guard** |

## What

**E = 替换 dimos-deploy 的机器人控制执行面**：demo workflow 控制执行链在
robot-system 侧原生复现，达到冻结基线（`executor.py:72` 一圈 = 12 维 action →
读关节位形 → 双臂 trajectory〔各 5 臂关节〕+ 双夹爪下发）。验收 = 双 gate
（D-BI2：Rust cross-process e2e 自动 gate + Orin 真机"读状态→双臂→双夹爪"
完结 gate）。policy 推理 / 相机 / Web UI **不在 E**（D-BI0）。

**总形态预告（D1-D3 方向已拍 + D6 按推荐）**：E **零 wire 改动**——0 新
proto method、0 新 fbs schema、0 改 frozen 字段（r2 各阻断回修后结论不变）；
全部工作落在 robot-msg lib 函数 + config typed 校验 + S0 command 防错位 +
S1 语义补全（G1 + sd_notify）+ gateway sd_notify + configs（新双 config +
既有 config/模板补 motor_ids）+ client 侧 helpers/shim + e2e + 部署件
（运行时 unit ×6 + handoff oneshot ×1 + preflight + runbook）。

## 影响范围

- **In**：robot-msg（layout_id 计算 pub fn，规范编码）；config loader
  （motor_ids typed 化 + 全套 fail-fast 校验 + 身份锁）；robot-s0（真
  layout_id + command 侧 layout gate）；robot-s1（layout 派生/fail-fast +
  `scripted_start_pose` + `idle_hold_target` + 常驻 state cache + Scripted
  末点发布保证 + **sd_notify/watchdog**）；robot-rpc gateway
  （**sd_notify/watchdog**）；configs（新双 arm-only config + 既有 6 config
  与 e2e 模板补显式 motor_ids）；robot-rpc client 侧 helpers（轨迹转换 +
  layout 校验 + 基线导入 shim + 共享 case 库）；robot-rpc/tests workflow
  e2e；robot-tests 真机验收 bin（含 **D13 preflight phase**）；
  scripts/systemd **运行时 unit ×6 + `xlerobot-handoff.service` ×1** +
  新 runbook。
- **Out（hard boundaries 镜像，详 §"严禁改动"）**：proto/fbs wire 0 改；
  design.md 0 改；dimos-deploy 0 改；robotctl 0 改；HoldPose 行为 0 改；
  IkBackend / kinematics 两 crate 0 改；候选 A-D 已 land 行为 0 退化。

---

## 关键设计决策（D1-D3 = P0 优先拍；D4-D12 随后）

### D1（Q10，P0）⚠ **方向已拍 (a)+(i)**（2026-07-13，条件生效）：joint 名序权威 = TOML 派生链 + S0 command 防错位硬约束

> reviewer 拍板条件：完成**规范编码、typed ID、身份一致性**后生效；且只宣称
> §3.5.1 **部分落地**。本 r2 落字（S2 阻断回修）：

1. **计算权威（robot-msg 新 pub fn，r2 规范编码）**：
   `compute_layout_id(robot_id, hardware_id, joints: &[(name, motor_id)]) -> u64`
   = `u64::from_le_bytes(blake3(enc).as_bytes()[0..8])`（**端序显式 LE**）。
   `enc` = **长度前缀规范编码**（撤 r1 NUL 分隔——无长度/count/domain 存在跨
   字段歧义）：
   `lp("robot-layout-v1")`（domain/version 常量）‖ `lp(robot_id)` ‖
   `lp(hardware_id)` ‖ `u32_le(joint_count)` ‖ ∀i∈config 顺序:
   `lp(name_i)` ‖ `u32_le(motor_id_i)`，其中 `lp(x) = u32_le(len(x)) ‖ x`。
   编码**单射**（长度前缀）+ 域隔离（domain 常量）+ 可演进（v1→v2 走
   CHANGELOG）。覆盖 joint_names + joint_ids + order 三成分（design §3.5.1
   字面"sorted_…+order"的可执行化解读，解读差异走 CHANGELOG，design.md 0 改）。
   `schema_lint.rs` sorted-names helper（`:190`）改调新函数。
2. **config typed 化 + 全套 fail-fast 校验（r2，撤静默丢弃）**：`motor_ids`
   从 extras 提升为 **typed 字段**；校验（违者 fail-fast，exit 码见第 3'
   条 r3 口径）：
   `len(motor_ids) == len(joints)`、两者非空、joints 名唯一、motor_ids 唯一
   且 ∈ u8 域、**任何非法值拒绝**（撤 `filter_map` 静默丢弃，
   `s0_daemon.rs:1163`）、**缺 motor_ids = fail-fast**（无默认推导）。
   **mock/e2e 显式 ID 策略**：e2e 动态模板（`case_ctx.rs:306`）与 6 个无
   motor_ids 的 config **全部补显式 `motor_ids = [1..=n]`**——r1 "既有
   harness 天然一致"宣称**撤回**（现状无 ID 可哈希），补齐进 In 范围。
3. **身份一致性锁（r2）**：S0 与 S1 均锁 `[robot].id == --robot-id`（两者同
   给且不等 → fail-fast exit 2，沿 gateway 先例）——**禁止 CLI 与 TOML 身份
   分叉后各算不同 layout**。hardware_id 沿既有 resolve（`--hardware-id` 是
   section 选择器，选中段 id 即 hash 入参）。
   **3'. exit 码口径（r3 T7 + r4 U6 锁死）**：**CLI 身份/参数冲突**
   （`[robot].id ≠ --robot-id`、显式 `--layout-id` ≠ 派生值）= **exit 2**
   （参数族）；**config 内容错误**（缺/非法 motor_ids、长度不等、重名等）
   沿**各 daemon 既有码明列**——**S1 = exit 3**（B1 冻结口径
   `s1_daemon.rs:39` + `:294`）；**S0 = exit 2**（Args parse/resolve 路径
   既有码，`s0_daemon.rs:396-405`，typed 校验挂同路径），与 **adapter
   构建失败 = exit 1** 区分。全部进 AC-E3。
4. **S0**：启动期由选中 `[[hardware]]` 算 authoritative layout_id，撤
   `0xCAFE_BABE_DEAD_BEEF` 占位（`s0_daemon.rs:858`）→ `JointState.layout_id`
   变真值（wire 字段既有，**值语义变更**登记 CHANGELOG）。**command 侧硬约束
   （CodeMap B9）**：ipc_thread 在 `parse_joint_command` **之前**比对
   `fb.layout_id()` ≟ authoritative——不一致 → **丢弃、不写
   `LatestCommandSlot`** + `unknown_layout_count` + 限频 warn（闭 §3.5.1
   command 面违反，`ipc_thread.rs:1177-1192`）。
5. **S1**：layout_id 由 config 派生（同函数）；`--layout-id` 显式且 ≠ 派生 →
   fail-fast exit 2；缺省用派生（撤 `s1_daemon.rs:133` 占位默认）。一切
   command producer（含 mock_s1）layout_id = 派生值。
6. **client（e2e / 验收 bin / 转换 helper）**：同 config 读有序 joints 作
   name→index 权威 + 同函数校验 `GetState.layout_id`；不匹配硬失败，**禁裸
   固定索引**。

**子拍板已拍 (i)**：计数 + 限频日志，不动 S0 状态机。**部分落地边界（登记
CHANGELOG）**：§3.5.1 的 JointLayout 1 Hz 广播 / RR 请求 / 阈值→状态升级
均不在 E（复审触发：S0 观测面候选）。

**AC 钩子**：AC-E1（compute 确定性 + 顺序/id 敏感 + **编码单射**〔构造对撞
样例拒绝〕+ lint 同源）/ AC-E2（S0 mismatch → 不写 slot + 计数）/ AC-E3
（S1 CLI 冲突 exit 2 + S0/S1 robot-id 分叉 exit 2 + config 校验 fail-fast
全路径）/ AC-E4（client 校验 e2e）。

### D2（Q11，P0）⚠ **方向已拍 (i)**（2026-07-13，条件生效）：arm-only 6-motor ×2 + 运行身份/部署件全表锁定

> reviewer 拍板条件：先锁**身份、S0 units、omitted-motor fail-closed 交接
> （= D13）**。身份进 topic / layout hash / gateway config / latch 路径，非
> 文件命名细节——本 r2 全表锁定（S3 阻断回修）：

| 项 | 左臂 | 右臂 |
|---|---|---|
| robot_id | `xlerobot_left` | `xlerobot_right` |
| hardware_id | `left_arm` | `right_arm` |
| config | **新** `configs/xlerobot_left_arm.toml` | **新** `configs/xlerobot_right_arm.toml`（对称新文件；旧 `xlerobot_bus2_right_arm_full.toml` 保留不动作历史件） |
| 串口（by-id） | `…5A7C121427-if00`（bus1） | `…5A7C122340-if00`（bus2） |
| joints（ACTION_DIM 序） | `left_arm_shoulder_pan, left_arm_shoulder_lift, left_arm_elbow_flex, left_arm_wrist_flex, left_arm_wrist_roll, left_arm_gripper` | `right_arm_*` 同构 6 项 |
| motor_ids | `[1,2,3,4,5,6]` | `[1,2,3,4,5,6]` |
| calibration_paths | `left_arm.json`（arm-only 不含 head joint，无需 head.json 冗余；拣取语义沿 factory 先例） | `right_arm.json`（同理不给 base.json） |
| latch 路径 | `/var/lib/robot/xlerobot_left/estop_latch` | `/var/lib/robot/xlerobot_right/estop_latch` |
| gateway listen | `127.0.0.1:50051` | `127.0.0.1:50052` |
| S0 unit（**新增，计入 D12**） | `xlerobot-s0-left-arm.service` | `xlerobot-s0-right-arm.service` |

- **旧 unit 保留回滚（r2）**：`xlerobot-s0-bus{1,2}.service`（full config，
  `bus1:55` / `bus2:115` ExecStart 不动）**不删不改**；新 arm-only unit
  `Conflicts=` 旧 full S0 unit + dimos 两 unit（同串口互斥）。
- 底盘 `Operating_Mode=1` 残留风险经 (i) **移出 S0 写路径**（S0 永不写轮子；
  Q5 收窄、Q9 无 mode 硬化链）；但**排除 ≠ 风险整体消除**（r1 表述**撤回**，
  S4 阻断）——head/wheels 切换窗口遗留状态由 **D13 fail-closed preflight**
  兜底。
- **底盘/头驱动 = 非 E 目标**（12 维无 head/base）；未来走独立候选 + full
  config + mode 硬化链（CodeMap §5.1 (ii) 封存备查）。
- `xlerobot.toml`（陈旧）与两 full config **保留不删**；E 权威 = 本表两
  config。
- **latch 首次 provision（r3 T1 + r4 U2 语义对齐）**：两个新 latch 路径是
  **全新身份**，照搬既有 runbook 的 `0\n` provision
  （`scripts/systemd/README.md:37` 区块）= 借更换 robot_id **丢掉旧 EStop
  记忆**。锁定：
  1. **provision 规则（r4 收窄 + r5 V2 partial-failure 闭环）三分支**：
     - primary 与 backup **双缺失** → 写入 **`1\n`（latched）双份**；
     - **单份缺失（r6 W2 收紧：以合法性为前提，非仅存在性）**：唯一现存
       副本**严格解析为 0/1** → **只写缺失文件**（值 = 现存副本值，**不经
       `save_estop_latch` 重写现存副本**；原子写细则〔r7 落字 + r8 Y2
       收紧〕= **tmp 写入 → file fsync → rename → parent-dir open +
       fsync，provision 专用严格语义：任一步失败〔含目录 open/fsync〕→
       返回错误、禁止启动 S0**。注意这**强于**既有运行期 latch 写——
       `estop_latch.rs:333-335` 的目录 sync 是 best-effort 忽略错误
       （"某些 FS 不支持 dir sync"），**运行期路径 0 改**；严格版仅住
       provision 入口，掉电保证仅对 provision 宣称）；现存副本
       **损坏 / 不可读 / 值非法 → provision 直接失败且不得创建另一份**
       （交给限流 → failed，人工介入）。（依据：既有双写允许"primary
       成功、backup 失败并返 Err"〔`estop_latch.rs:183` 区块文档〕，r4
       "任一份存在即跳过"会单副本起跑；而 r5 按存在性补写在"primary 损坏
       + backup 缺失"时无合法值可复制——本分支两面都闭）；
     - 双份均存在 → **文件内容不动**，交给 loader 既有语义（下表）。
     **统一最终步骤（r9 Z2——所有分支收敛后必经，含"双份均存在"路径）**：
     **parent-dir open + fsync（严格：任一步失败 → provision 返回错误、
     禁止 exec）**。闭掉绕过序列："file fsync + rename 成功 → dir-fsync
     失败 → 重试命中双份存在 → 上一次未确认的目录持久化被跳过"；目录
     fsync 幂等、primary/backup 同 parent 一次即可。
  2. **loader truth table（r4，对齐 `estop_latch.rs:95` 现行为，0 改）**：

     | primary | backup | 结果 |
     |---|---|---|
     | 合法 v | 任意（合法/损坏/缺失/值不同） | 用 primary v（**backup 不读**——"双份值冲突"由 primary 权威消解） |
     | 缺失 | 合法 v | 恢复 v + 自愈回写 primary（best-effort） |
     | 损坏 | 合法 v | 恢复 v（沿 loader 现行为） |
     | 缺失/损坏 | 缺失/损坏 | **拒启动**（NotFound / InvalidData，strict fail-closed） |

     r3 "损坏文件全部 strict fail-closed"表述**更正**：strict 仅指**双份
     均不可用**；单份合法即恢复是既有语义，E 0 改。
  3. **新身份首启状态链（r4 更正）**：provision `1\n` → S0 启动进入
     **latched Safe** → maintainer+ `ClearEStop` 清 latch 后**仍 Safe**
     （`state_machine.rs:578` SAFE→SAFE）→ `SetEnabled(true)` → **Ready**
     → S1 heartbeat → **Enabled.Active**——r3 "ClearEStop 后 Ready"表述
     **更正**（漏 SetEnabled 一步）。
  4. provision 步序：本臂 **ExecStart guard 第二步**（preflight 之后、
     `exec s0_daemon` 之前，同进程事务内——r4 U1 + r8 Y1 形态，详 D13）。

**AC 钩子**：AC-E5（双新 config 载入 + typed 校验 + 派生 layout_id + mock
双栈 e2e 全通）/ AC-E21——**判据以 §AC 草案总表 AC-E21 行为唯一权威**
（r7 X2：本节不再复写细则，防双源漂移；总表行已含 provision 三分支
〔含 r6 损坏单副本失败方向 ×2〕+ r7 start-limit 重试终态 + truth table）。

### D3（Q12，P0）⚠ **方向已拍 (c)**（2026-07-13，条件生效）：G1 = `scripted_start_pose` + `idle_hold_target`，HoldPose 0 改

> reviewer 拍板条件：补**最终命令发布保证** + **success/cancel/failure/IK/
> Degraded 行为矩阵**。本 r2 落字（S1 阻断回修）：

1. **`scripted_start_pose`（per-action，修"起点半"，r1 形态不变）**：Scripted
   `start()` 的 segment0 from = **提交时当前位**，不再用启动期 `anchor_bias`
   （`scripted.rs:186` 现状）。
   - **来源**：S1 新增**常驻 state cache**——一条 state subscriber 线程持续
     更新 latest `JointState`（复用启动锚定校验语义：joint_count 匹配 +
     `max_stale_ms` 新鲜度 + **layout_id == 派生值**〔D1 联动〕）。**线程
     卫生（r2 非阻断落字）**：stop + join + 正常 Drop（iceoryx2 0.9.3），
     **禁** mem::forget 回潮。
   - **失败语义**：cache 无帧 / stale / layout 不匹配 → Scripted start
     **拒绝**（`ActionStartError::Rejected` → tonic `FailedPrecondition`），
     **绝不 fallback 到启动 anchor**（静默跳变危险）。
2. **最终命令发布保证（r2 新增，S1 阻断根修）**：现状 `dwell_ms == 0` 时末
   waypoint 填 `tick_buf` 后直接 advance → `Completed(Success)`，**该 tick
   不产命令帧**（`scripted.rs:331-341` + `:435-445`），coordinator 对
   Completed 帧**不 publish**（`coordinator.rs:1712-1730`）→ 末 target 可能
   从未上线。**修法锁定**：末 waypoint（无论 dwell_ms）完成时**先发一帧
   Continue（target = 末 waypoint）**，**下一 tick 才返 Completed**——保证
   "末 target 已成功 publish"先于任何 idle_hold_target 提交。dwell_ms > 0
   路径（Dwell 期已反复发末 target）统一走同一保证，行为不变。
3. **`idle_hold_target` 更新规则（r2 收窄 + r3 T3 矛盾修正 + r4 U5 穷尽式
   单一规则）**：

   > **唯一规则**：action 终态时——`(backend == Scripted) ∧ (终态 ==
   > Completed(Success)) ∧ (末 waypoint target 已按第 2 条发布)` →
   > `idle_hold_target = 末 waypoint target`；**其余所有 backend × 终态
   > 组合**（Scripted 非 Success、IK 全部、HoldPose 全部、DwellMs 全部、
   > 未来新 backend 缺省）→ **显式 reset `idle_hold_target = 启动
   > anchor_bias`**。

   r3 拍定沿用：reset 语义 = 严格维持旧观测行为（非成功后 hold = 启动
   anchor 在**任意历史序列**下成立）；"保留 last-success target"备选
   **未采**。**撤回 r1 "每次 publish 更新"**（连带改 IK 完成后行为，
   `ik.rs:315`）；**撤回 r1 "Idle / Degraded 一起改"**（只解锁 Idle）。
   初值 = 启动 anchor_bias → boot 后首个 Success-Scripted 之前 Idle hold
   与今日 **bit 等价**。
   - **B1 等价语义解锁（收窄版）**：Idle hold ≡ B1（`coordinator.rs:865-871`，
     v0.3 锁定）按本条**显式解锁**——形态（build_hold_command 路径）不变，
     **target 源仅在 Success-Scripted 后变更**；登记 CHANGELOG + AC 锁。
4. **行为矩阵（r2 新增，全行进 AC-E18）**：

   | 路径 | 终态后 hold target |
   |---|---|
   | Scripted `Completed(Success)` | **末 waypoint target（新，经第 2/3 条）** |
   | Scripted cancel（Cancelled / PartialSuccess） | 启动 anchor（现状不变） |
   | Scripted failure / deadline | 启动 anchor（现状不变） |
   | IK（一切结局） | 启动 anchor（现状不变，`ik.rs:315` 0 改） |
   | HoldPose（一切结局） | anchor_bias（0 改，CodeMap B10 边界） |
   | **Degraded 态 hold** | 启动 anchor_bias（**不解锁**——Idle 与 Degraded 自此分源） |

   cancel / failure 维持"回拉启动 anchor"= 既有行为原样（经穷尽式 reset
   规则在任意序列下成立；矩阵各行 = 该规则的实例化）；改为"hold 当前位/
   last-success"属独立语义变更，**不在 E**（登记不在范围）。**AC-E18 必测
   序列（r3 + r4）**：先 Success 一次（idle_hold_target 已 = 末点）→ 再
   分别 cancel / failure / IK / **HoldPose cancel**，逐一断言 hold 回到
   启动 anchor——**不得只测 boot 初态**。
5. **HoldPose 0 改（B10 边界，r1 不变）**：继续每 tick 读 `anchor_bias`
   （启动值，构造期固化）；`hold_pose.rs` 不动；两个新概念与共享 anchor
   **物理隔离**（字段/传参形态 Plan 定；语义边界本条锁死）。

**G1 闭合声明**：起点半（1）+ 回拉半（2+3）= CodeMap Q12 (c)；Q2 的"G1 两半
全部闭合"前置由本条满足（连续 Success-chunk 场景无回拉、无跳变；非 Success
路径回拉 = 显式登记的既有行为）。

**D3 子拍板**：stale 阈值缺省沿既有 anchor `max_stale_ms` config（不新增
配置面）；拒绝码固定 `FailedPrecondition`。

**AC 钩子**：AC-E6（start_pose = 提交时当前位；stale/layout 失败 →
FailedPrecondition 不 fallback）/ AC-E7（连续两 Success-chunk 无回拉无跳变）
/ AC-E8（HoldPose 回归 bit 等价）/ AC-E17（末 waypoint 先 Continue 后
Completed，dwell=0 路径发布保证）/ AC-E18（行为矩阵全行）/ AC-E20（cache
线程 stop+join 正常 Drop）。

---

### D4（Q1）拍板：双臂拓扑 = (a) 双 robot_id 双栈 + 双 gateway

0 架构改动（CodeMap §1.2-3 + task29 先例 + Orin 双 S0 unit 现状）。验收
client 管两条连接（两端口 loopback）；e2e harness 起 2×(s0+s1) + 2 gateway。
gateway/s1 多 hardware 化（CodeMap Q1(b)）= 非 E 目标登记。

### D5（Q2）拍板：10 Hz 映射 = (a) per-chunk 折叠 + chunk 间等终态

一次推理 = 一条 10-waypoint Scripted（每段 `segment_duration_ms=100`，4 KiB
gate 内）。**冻结基线实际行为就是顺序 chunk**（`policy_task.py:80-84`：执行
完 steps_per_chunk 步才回外层重推理）→ 等终态即对齐基线，D3 后 chunk 间隙
hold 在末位无回拉。mid-chunk 抢占（Cancel+Invoke 对）= **非 E 目标登记**
（post-E 若真需要再议）。

**双臂成对提交协议（r2 新增，S5 阻断）**：冻结实现逐臂 submit 但调用立即
返回 = 两臂**实际并行**（`executor.py:95-107`）。E 锁定：

1. 每 chunk：双臂各读 state → 两 payload 就绪后**并发提交**（两 gateway
   同时 `InvokeTask`，并发 join 形态）；**Ack 偏差**上限进 AC-E14
   （实测锁数值；口径统一见第 7 条，r5 V3）。
2. **等待双终态（r3 精确化）**：终态精确集 = `Completed / Failed /
   Cancelled`（`action_status.fbs:57` TaskStateFb；r2 "Completed 家族"表述
   撤回）；**pair success 判定 = 两侧均 `Completed(Success)`**，其余任意
   组合 = pair 非成功。
3. **提交期部分失败**（一收一拒）：立即 `CancelTask` 已接收侧 → 等其终态；
   cancel 不收敛（超时）→ **双侧 `EStop`**（operator+ RPC 既有，安全方向，
   弱 Ack 语义照旧）。
4. **执行期不对称终态（r3，T5）**：两侧均 accepted 后，一侧先到
   `Failed / Cancelled / deadline` 而另一侧仍 Executing → **立即
   `CancelTask` 仍运行侧** → 等其终态（不收敛 → 双侧 EStop）；一侧已
   `Completed(Success)`、另一侧随后失败 → pair 结果 = **partial
   completion**（验收报告显式记录成功侧终态 + 失败侧终态，pair 判 FAIL）。
5. **stream 断开 / 事件超时收敛（r3）**：`StreamTaskEvents` 断开或超时 →
   轮询 `GetTaskStatus` 收敛终态（read RPC 无 auth 现状照旧），收敛超时
   上限进 AC-E14。
6. **双拒绝（r3 拍定）**：**不自动重试**——chunk 判 FAIL、验收确定性终止
   （非零退出）；自动重试策略若未来需要走独立拍板，**不留验收脚本临场
   决定**。
7. **偏差观测点（r3 + r4 U4 降级 + r5 V3 机制更正与边界声明）**：Started
   事件可能被错过的**真实机制** = gateway events 桥以 `latest()` 拉流，把
   subscriber 缓冲内历史**塌缩到最后一帧**（`events_subscribe.rs:281` 循环
   + `subscriber.rs:9` "拿到 buffer 里所有 sample 后保留最后一条，其他
   drop"）——**并非 QoS history_size=1**（r4 表述更正）；叠加 client 订阅
   晚于 invoke 返回（`task_rpc_e2e.rs:124` 注释"不强制收到 Started，用
   GetTaskStatus catch up"）。强制收到 Started 会与该桥设计冲突。
   **拍定**：偏差 **gate = InvokeTask Ack 偏差**（client 并发 join 提交，
   两侧 Ack 返回时刻之差，client 侧可控可测，上限进 AC-E14）；两侧
   `Started.at_ns` **若收到仅作诊断记录**（缺失不判失败、不得充当 gate）。
   **边界声明（r5）**：Ack gate 只约束 **RPC 接受完成**的偏差，**不证明
   实际动作启动偏差**（动作起动时刻在 S1 tick 侧，现无可靠双侧观测面；
   如需真启动偏差属后续观测面候选，不在 E）。proto `TaskHandle` 无
   `accepted_at_ns`（`task_handlers.rs:760` 仅 audit）维持不暴露。
8. 全部路径进 AC-E14（**Ack 偏差 gate**〔Started.at_ns 仅诊断〕/ 双终态
   精确集 / 提交期与执行期部分失败 cancel / partial completion 报告 /
   stream 断开收敛 / 双拒绝确定性 FAIL / 不收敛双 EStop）。

### D6（Q3）拍板：gripper = (a) 复用 Scripted 全向量同车

D2(i) 下 gripper 是 6 维向量第 6 位——**M4 缺口的闭合形态 =
`set_gripper_position` ↦ gripper 目标位入 waypoint 向量**（与臂同车）。
0 新 method / 0 新 schema。CodeMap Q3(b)（新 Gripper RPC，b1 强 RR / b2 弱
Ack 拆分）**封存备查**：post-E 若需绕-action-槽直写再起独立候选。

**gripper 语义变化登记（r2，S5 阻断后半）**：冻结基线 gripper = 每 ~100ms
一次**阶跃**寄存器直写（绕插值/仲裁）；E 下 gripper 随 Scripted **分段线性
插值**。**有意语义变化，非等价**——r1 隐含的"同车即等价"宣称**撤回**；
AC-E15 锁验收判据 = **终点位置一致 + 轨迹单调**（不逐点比对阶跃波形），
CHANGELOG 随实现登记该变化。

### D7（Q4）拍板：adapter = (iii) robot-rpc client 侧 helper 模块

`JointTrajectory 形状数据 → ScriptedPayload` 转换 + name→index（D1 链）+
GetState 读向量封装，住 robot-rpc client 侧（feature/module 形态 Plan 定），
e2e 与验收 bin 共用。**不进 robotctl**（辅助定位不变）、**不等 s2-bridge**。
转换规则按 CodeMap §3.1 表（时长差分 / velocities 丢弃 / 首点语义由 D3
承接——`scripted_start_pose` 使"显式首点"不再需要，转换器**丢弃首点并断言
其 ≈ 提交时当前位**的宽容校验进 AC）。

### D8（Q5）拍板：非目标关节填充 = GetState 当前位（仅臂向量内）

D2(i) 下向量仅 6 关节：单独动夹爪 = 5 臂关节填 GetState 当前位；反之同理。
读写间隙漂移容差 + "同 chunk 一次读"策略进 AC-E9 定数值。底盘/头问题随
D2(i) 消失。

### D9（Q6）拍板：e2e 放置 = (c) 双轨

workflow e2e 进 `robot-rpc/tests/`（CI 自动跑，沿 4 e2e 先例；**"真 s0 +
真 s1 + 真 gateway"三真同堂首例**，readiness 链沿 marker→TCP→RPC probe）+
真机验收 bin 进 robot-tests（required-features），**两者复用同一 case 库**。
**case 库落位（r2 非阻断落字）**：共享语义住 **robot-rpc client helper**
（D7 模块的 test-support 面），robot-rpc/tests 与 robot-tests **同向消费**；
**禁止** robot-rpc 反向 dep robot-tests（robot-tests 已 dep robot-rpc）。

### D10（Q7）拍板：真机完结 gate 驱动 = (c) 专用验收 binary 为主

robot-tests 新验收 bin（required-features 守门；phase 化：读状态 → 双臂
chunk → 双夹爪 → 汇总），机器可判 + 人眼可看；robotctl 作旁路证据面
（现有 `status`/`watch`；**不新增子命令**——补齐 get-state 等登记为
post-E robotctl 候选）。

### D11（Q8）拍板（r2 重写，S6 阻断）：单位契约分层——helper rad-native + 基线导入 shim；换算权威唯一

- **E 运行时 helper（robot-rpc client 侧，D7）**：**rad-native**——只收 rad
  轨迹；**拒 NaN/Inf、拒长度不匹配**；**不 clamp**（硬件保护 clamp 的唯一
  权威 = S0 calibration clamp 既有路径，不在 client 复制第二道）。
- **基线导入 shim（测试侧，r2 新增）**：输入 = **冻结 12 维原始形态 = 臂
  "度" + gripper [0,100]**（`executor.py:55-68` `action_to_positions` 入参
  语义），镜像其换算：臂 `× π/180`；gripper `/100 → [0,1] → rad`（
  `rad = rad_lo + v × (rad_hi − rad_lo)`），**rad_lo/rad_hi 由调用方显式
  传入**。越界镜像 dimos clamp（[0,100] clamp + 计数 warn）。
- **换算权威唯一（r2 + r3 T6 事实更正）**：`raw↔rad` 现为 feetech adapter
  **私有 fn**（`adapter.rs:92-103`），且 **robot-tests 现无 feetech 依赖**
  （`robot-tests/Cargo.toml` [dependencies] 无该 crate）——r2 "已 dep"表述
  **更正**。锁定：feetech crate **暴露纯公共换算 API**（pub 纯函数；
  adapter 自身改为复用同一 API，公式单源）；robot-tests 以 **optional
  依赖挂 `real-hw` feature**（仅 preflight / 验收 bin 消费）；该依赖变化
  进 Plan 的 `cargo build --timings` 对照 + cargo deny gate。
  **robot-rpc 仍不得复制公式**——验收 bin 用公共 API 从标定 JSON raw
  `range_min/max` 算 rad_lo/rad_hi，再作参数传给 shim。
- **失败语义（r2）**：NaN/Inf → reject（helper 与 shim 均）；长度不匹配 →
  reject；标定 `range_min >= range_max` → 派生期 **fail-fast**；rad-native
  路径无 clamp（垃圾值由 S0 calibration clamp 兜底 + AC 锁 reject 面）。
- r1 结论保持：**"[0,1]"（连同"度"/[0,100]）不进 E 运行时**，仅存在于 shim
  输入层（CodeMap §5.2 之问的完整答案）。

**AC 钩子**：AC-E10（shim 度/[0,100] 镜像换算 + clamp 计数 + rad_lo/hi 参数
化；helper reject 全路径；range 反转 fail-fast）。

### D12（Q9）⚠ 拍板（r2 重写，S7 阻断：不留 Plan）：S1/gateway 纳入 sd_notify + watchdog；运行时 unit ×6 + handoff ×1 + 新 runbook；deploy.sh 不进 E

- **Type=notify 直接拍（推荐 = 纳入）**：design 冻结明确 S1 / gateway 走
  sd_notify + watchdog（`00_design.md` §9 心跳表：S1 `sd_notify` 2 Hz /
  timeout 5 s、gateway 同款；unit `Type=notify`）。S1 现状 = B1 决策 #7
  显式 Type=simple 非目标（`s1_daemon.rs:54`）——**E 落长期运行形态、
  按设计补齐**（round 7 注：E 收口 = 受监督 demo，正式退役推迟 post-E，
  但 daemon liveness 属 demo 保留项）：S1 与 gateway 各接
  **liveness-aware sd_notify**（沿 S0
  `systemd_notify.rs` LivenessObserver 模式：任一被观测 worker 停滞即跳
  ping → systemd watchdog 重启）。被观测 worker 集（数目 Plan 细化，语义
  Spec 锁）：S1 = tick loop / heartbeat pub / rr servers / state cache
  （D3）；gateway = rr_io_threads / estop_pub / state pool / tonic serve。
  unit `Type=notify` + `WatchdogSec=5s`。**B1 决策 #7（"不带 sd_notify"）
  按本条显式解锁**，登记 CHANGELOG。
- **liveness 语义防误判（r3，T4）**：既有 LivenessObserver = 计数不增长即
  跳 ping（`systemd_notify.rs:139` 区块语义）；RR metrics 只计**真实请求**
  （`rr_io_thread.rs:124` `requests_sent` 族），tonic serve 无进度计数——
  直接复用会把**合法空闲**判成停滞。锁定四条：
  1. request-driven worker（rr_io_threads / estop_pub / rr servers 等）
     观测**独立 `loop_tick_count`**（空闲 poll 轮询也推进），**不用**请求
     计数作 liveness 源；
  2. tonic serve 用 **loopback health probe**（或等价响应性指标）作
     observer；
  3. `READY=1` 仅在**全部 observer ready** 后发送；
  4. shutdown 先发 `STOPPING=1`、再停 watchdog ping。
- **unit 清单（r6 W4 统一）= 运行时 unit ×6 + `xlerobot-handoff.service`
  ×1（共 7）**：S0 arm-only ×2（D2 表）+ S1 ×2 + gateway ×2 + handoff
  oneshot ×1（D13）；旧栈停止顺序第一责任人 = handoff unit（Conflicts+
  After），运行时 unit 保留 Conflicts 兜底。旧 unit 全部保留（回滚）。
- 新真机 runbook（替代两份 pre-strict 档案；r6 W4：**以 handoff
  transaction 为旧栈停止与首次全总线安全交接的权威入口**——步序 =
  `systemctl start` 目标臂 unit → systemd 依赖链自动执行〔handoff：停
  旧栈 + 全 17 电机 ensure-safe〕→〔本臂 ExecStart guard：preflight +
  provision → exec s0_daemon〕→ demo 验收；**round 8 同步（P1-1）：
  demo 后旧栈仍保留——只允许为释放硬件而 `stop`，正式退役 / disable /
  删除全部等 post-E 生产安全候选闭环〔D-BI3 r3〕**；
  手工 `systemctl stop` 旧栈仍可作前置动作但**不再是安全交接的依据**）。
- **deploy.sh 不进 E**（登记独立 follow-up；Orin 本地 build 惯例沿用）。
- D2(i) 下**无底盘 mode 硬化链**（但有 D13 preflight）。

**AC 钩子**：AC-E19（S1/gateway watchdog：worker 停滞 → 跳 ping → systemd
重启，沿 freeze_systemd_smoke 先例形态）。

### D13（r2 新增，P0 安全，S4 阻断）⚠ 拍板：真机交接 fail-closed preflight（head/base 遗留状态）

**"排除 = 风险整体消除"表述撤回**：arm-only S0 不驱动也**不能 EStop**
head/base 电机；旧栈退出只 best-effort 断扭且吞失败
（`_manipulator_base.py:184-192` disconnect 逐电机 `Torque_Enable=0` 包在
`except: pass`；`bus2_base.py:166+` 轮子 `Goal_Velocity=0` 同款）——
head/wheels 在切换窗口的扭矩/速度状态**未知**。

**fail-closed preflight（锁定；r4 U3 ensure-safe 语义重写；round 8 P0-2
扩程 = 本 bus 全量）**：真机启动任一 arm-only S0 **之前**（含 systemd
watchdog / crash 自动 restart 的每一次 guard 重跑），对该臂所属 bus 的
**全部电机**——左 = bus1 **ID 1-8**（6 臂 + 2 head）；右 = bus2
**ID 1-9**（6 臂 + 3 wheels）。**扩程根因（P0-2）**：`auto_enable=false`
只防新 S0 自动上扭，**不消除崩溃残留扭矩**——Feetech `connect()` 只建
连接/翻译 id/缓存，**不写 `Torque_Enable=0`**（`adapter.rs:475-514`
实证）；S0 Enabled 崩溃 → 电机持续带扭 → 若 guard 只处理 omitted
motors，则本臂 ID 1-6 在新 S0 起后仍保持上次带扭状态。**仍只碰本臂
bus**（错峰启动不触另一臂已占串口）：

1. （审计步，可选）预读当前值——**仅记录，不决定是否执行安全写**；
2. **无条件 best-effort 安全写本 bus 全部电机**：bus1 ID 1-8 逐电机
   `Torque_Enable = 0`；bus2 ID 1-6 逐电机 `Torque_Enable = 0`、
   ID 7-9 **先 `Goal_Velocity = 0` 后 `Torque_Enable = 0`**（顺序
   锁定：先零速再断扭）；**单电机/单寄存器写失败不中断**，继续处理
   其余电机；
3. **逐项 readback**：每电机每安全寄存器读回并断言安全值；单项失败同样
   继续其余项；
4. **末尾聚合**：任一写 / readback 失败或值不安全 → preflight 整体
   **FAIL → 禁止切换**（该臂新栈不起），**保留人工回滚能力**（旧 unit 未
   disable、未删；是否回启由人工判断，见下"回滚表述修正"）。
   r3 "先读非零才写"**撤回**（首读失败会跳过断扭尝试且可能中断其余电机——
   安全方向必须先写后验）。

**启动拓扑 systemd 强制（r3 T2 + r4 U1 + r5 V1 重写）**：

- **全局 handoff unit（r5 V1——单臂直启不得留下另一 bus 安全空窗）**：旧
  coordinator **同时占两条 bus**（CodeMap §0.3 blueprint），启动任一新 S0
  都会经 Conflicts 停掉**整个旧栈**——per-bus preflight 只保本 bus，仅启动
  左臂时 bus2 wheels 会长期遗留旧栈状态。新增
  `xlerobot-handoff.service`（`Type=oneshot` + **`RemainAfterExit=yes`**）：
  ① 对 dimos 两 unit + 旧 full S0 unit 声明 `Conflicts=` + `After=`
  （**旧栈停止顺序的第一责任人**：conflict 目标的 stop job 按 After 排序
  在本 unit start 前完成）；② `ExecStart` = **全部 17 电机安全写并
  readback（r6 W1 拍死——旧 coordinator 管两臂全部电机且退出断扭吞错，
  仅启动一臂时另一臂 ID 1-6 可能仍带扭且无人接管）**：
  **bus1 ID 1-8 `Torque_Enable=0`；bus2 ID 1-6 `Torque_Enable=0`；
  bus2 ID 7-9 先 `Goal_Velocity=0` 再 `Torque_Enable=0`**——写/readback
  聚合语义同上 fail-closed 段（单项失败继续、末尾聚合 FAIL）；
  **R9-1 双 bus 形态**：`preflight-handoff --left-config …
  --right-config …`（双份 config 显式传入、各构造独立 transport——
  两 bus motor ID 重叠均有 1..6，失败按 `PreflightFailure` 的 `arm`
  归属〔R10-3 类型形态〕）；
  **一侧失败（含串口构造失败）不阻断另一侧完整执行，末尾双侧聚合**；
  "禁止单臂、强制成对启动"备选**未采**。两个 arm-only S0 unit **`Requires=` +
  `After=`** handoff unit → **仅启动一臂也先保全部 17 电机安全**（随后
  该臂 S0 的 enable 序列只重新上扭自己的 6 电机）；同 boot 内第二臂启动
  时 handoff 已 active（RemainAfterExit）**不再重跑**（不碰第一臂已占
  串口）；reboot 后自动重新验证（断电后扭矩本就复位，重验无害且
  fail-closed）。
- **每臂 ExecStart guard 两步 pre-exec（r5 定位收窄 + r8 Y1 形态改造）**：
  `ExecStart=` **guard wrapper**：`① 本臂 bus preflight`（**只触本臂
  bus**——左碰 bus1、右碰 bus2，任何时刻不碰另一臂串口）→ `② 本臂 latch
  provision`（D2 三分支规则）→ 两步全部成功 → **清零连败计数 + `exec
  s0_daemon`**（exec 保 MainPID，`Type=notify` / watchdog 语义照旧）。
  同进程事务内顺序执行，pre 步失败**绝不 exec s0_daemon**。
  **失败/重试预算（r8 Y1 拆分——r7 `StartLimitIntervalSec=infinity`
  撤回**：systemd 定义 infinity 为限制启动尝试**总数**，成功启动 / 手工
  启动 / watchdog 重启同样消耗终身额度〔reviewer 实测：初次成功 + 4 次
  成功 restart 后第 6 次启动即 start-limit-hit〕→ 正常恢复若干次后 S0
  **永久拒绝下一次 watchdog 恢复，违反 §6.7 #2**）：
  1. **运行期崩溃 / watchdog 风暴预算** = unit 级
     `StartLimitIntervalSec=60s` + `StartLimitBurst=5`（**维持既有 S0
     值，r7 偏离登记撤销**）+ `Restart=on-failure` + `RestartSec=2s`。
  2. **preflight/provision 连败预算** = guard 自管**独立连续失败计数**
     （与墙钟无关——X1 定量依据仍成立：单次 pre 尝试最坏 ~12s〔Feetech
     单包 1s 超时 ×~12 次串口操作，`registers.rs:43` + `bus.rs:199`〕
     滚出 60s 窗口，unit 级限流抓不住慢失败循环）。**计数状态硬契约
     （r9 Z1 六条）**：
     - **路径按实例隔离**：`/run/robot-system/preflight/<robot_id>.count`
       + 同目录**稳定锁文件** `<robot_id>.lock`（双臂各自预算，互不
       消耗）；
     - **目录归属（r10 AA1 修正——r9 "guard 自建 + 不用 RuntimeDirectory"
       撤回）**：S0 unit 以普通 `User` 运行（`xlerobot-s0-bus1.service:50`
       / `bus2:110` 先例），/run 为 root:root 0755，**普通用户无权自建
       一级目录、首启即退 86**。改 systemd 原生三件套：
       **`RuntimeDirectory=robot-system/preflight` +
       `RuntimeDirectoryMode=0750` + `RuntimeDirectoryPreserve=yes`**
       （两臂 unit 同款声明；`yes` 精确给出"自动 restart 与手工
       stop/start 均保留、reboot 因 /run tmpfs 清零"）；count/lock 文件
       `0640` owner = unit `User`（guard 创建文件时置）；
     - **计数初态与 canonical 范围（r10 AA3）**：count 文件 **NotFound
       = 0**（/run 每次开机必然无文件，属正常初态、非异常）；可解析值
       canonical 范围 **0..=5**；**其他读取错误 / 非法格式 / 越界值 →
       退 86**；guard 启动读到 **count == 5 → 直接进入停止态退 86**
       （人工恢复 = 修复 + 清 count 文件 + `systemctl start`）；
     - **I/O 异常全部 fail-closed**：更新失败 / 成功清零失败 → 一律
       **退 86**（**禁止默认归零、禁止继续 exec**）；
     - **原子更新 + 单实例锁（r10 AA2 修正——r9 "flock(count)"撤回：
       flock 绑定 inode，rename 换新 inode 后竞争进程可同时拿锁，
       reviewer 本机最小实测复现）**：`flock(<robot_id>.lock)`——锁文件
       **永不 rename / 删除**；持锁期间：读 count → 跑
       preflight/provision → tmp+rename 原子更新/清零**仅** count；
       **exec 前显式关闭锁 fd（或置 CLOEXEC）**；flock 获取失败 = 同
       实例并发 → 退 86；
     - **专用退出码拍死 = 86 + 永久保留（r10 AA3）**：不与 s0_daemon
       既有 `0/1/2` 重叠（`s0_daemon.rs:36-41`），避开 S1 侧 `3/4/5`；
       unit 配 `RestartPreventExitStatus=86`——**exec 后同样作用于
       s0_daemon**，故 **86 进入 s0_daemon 退出码契约作永久保留码**
       （daemon 任何路径不得以 86 退出；回归锁进 AC-E3，防未来误用后
       失去 watchdog 重启）；该指令只作用于主进程——guard 即主进程
       （从 ExecStartPre 迁入 ExecStart 的原因）；
     - **语义流（r11 AB1 改造 = "未成功 guard 尝试计数"，pre-increment
       六步——r9/r10 "失败后才 +1"撤回：guard 在慢 preflight 中 panic /
       SIGKILL / 异常退出时计数不变，~12s/次 + 2s 重试仍滚出 60s/5 窗口
       无限重试，X1 重新暴露）**：
       ① 持稳定 `.lock`，读 n；**n == 5 → 立即退 86**；
       ② **执行任何 preflight / provision 之前**，原子写入 **n+1**
       （写失败 → 退 86）；
       ③ 两步全部成功 → **清零计数**（清零失败退 86）→ `exec
       s0_daemon`；
       ④ 正常失败：当前值 == 5 → 退 86；不足 5 → 普通非零码退出（走
       预算 1 的 `RestartSec=2s` 重试）；
       ⑤ **同一 boot 内的进程终止（panic / SIGKILL / 异常退出）保留已
       递增值**，下一次启动从 ① 继续累计——与正常失败同额计入预算；
       **掉电/重启按 RuntimeDirectory 契约清零（AA1，/run tmpfs），并
       重新执行 handoff 与 guard**——计数**不**具备掉电持久性（r12 更正
       r11 "掉电内保留"与 AA1 的冲突表述）；
       ⑥ 计数语义 = **未成功尝试数**（非"已完成失败数"）。
  3. **人工恢复路径**：修复硬件 → 清计数文件 → `systemctl start`
     （runbook 硬步序；`reset-failed` 仅在同时触发预算 1 时另需）。
  备选"给 preflight 定义可证明总时限"仍**未采**（`TimeoutStartSec` 中途
  杀 helper 破坏"全部电机继续处理、末尾聚合"语义）。watchdog 重启同样
  重跑 guard 两步（handoff unit 因 RemainAfterExit 不重跑；restart 窗口
  本臂只守本 bus——另一 bus 的安全由 handoff 首次切换建立〔全 17 电机〕，
  之后由其自身 S0 生命周期接管，**单臂运行期间另一臂电机保持 handoff
  断扭态**）。
- **旧栈 Conflicts 兜底**：arm-only S0 unit 保留对 dimos / 旧 full unit
  的 `Conflicts=` + `After=` 声明（防绕过 handoff 的边缘路径，与 handoff
  unit 双保险）。
- **S1/gateway 依赖图锁定（r5 V4 修正）**：`xlerobot-s1-<arm>.service` =
  `Requires=` + `After=` 同臂 S0 unit（S1 启动锚定硬依赖 S0 state）；
  gateway unit = `Requires=` + `After=` 同臂 S0（state 池 + estop 链）+
  **`Wants=` + `After=` 同臂 S1**（r4 只 `After=` 不会拉起 S1——直启
  gateway 会 READY 但 task RPC 无后端；`Wants=` 自动拉起 S1，且 S1 重启
  **不**连带停 gateway）。
- preflight / provision / handoff 可执行入口：独立 bin/子命令（验收 bin
  复用**同一实现**，形态 Plan 定）。

**回滚表述修正（r3）**：r2 "`systemctl start` 即回"**撤回**，改为**保留
人工回滚能力**（旧 unit 未 disable、未删）——硬件 I/O 已失败的场景**不承诺**
旧栈必然安全恢复；是否回启旧栈与残余风险由**人工判断**。

**载体**：preflight 独立入口（feature `real-hw`，直连 feetech transport 读
写寄存器——guard pre-exec 窗口本臂串口无人占用：旧栈已停〔Conflicts+After〕、
s0_daemon 尚未 exec）+ **每臂 ExecStart guard 强制** + runbook 硬步序
（三层同向）。

**AC 钩子**：AC-E16 与 AC-E22——**判据以 §AC 草案总表对应行为唯一权威**
（r7 X2：本节不再复写细则，防双源漂移；总表行已含 r6 handoff 全 17 电机
精确 ID 集场景与 r7 start-limit 重试终态）。

---

## AC 草案总表（round 2+ 细化编号与判据）

| AC | 锁什么 | 层 |
|---|---|---|
| AC-E1 | `compute_layout_id` 确定性 / 顺序敏感 / id 敏感 / lint 同源 | unit |
| AC-E2 | S0 layout mismatch command → 丢弃不写 slot + 计数 | unit + e2e |
| AC-E3 | exit 码全表（r4 + r10）：CLI 身份/`--layout-id` 冲突 → S0/S1 均 exit 2；config 内容错误 → S1 exit 3 / S0 exit 2；S0 adapter 构建失败 exit 1 区分；**86 = guard 永久保留码回归锁（s0_daemon 任何路径不得以 86 退出，防 `RestartPreventExitStatus=86` 误吞 watchdog 重启）** | unit |
| AC-E4 | client layout 校验不匹配硬失败 | e2e |
| AC-E5 | 左臂新 config 载入 + 双栈（2×s0+s1+gateway）mock e2e 全通 | e2e |
| AC-E6 | scripted_start_pose = 提交时当前位；stale/layout 失败 → FailedPrecondition 不 fallback | e2e |
| AC-E7 | 连续两 chunk 无回拉、无跳变（G1 闭合） | e2e |
| AC-E8 | HoldPose 回归 bit 等价（target == anchor_bias） | unit + e2e |
| AC-E9 | 非目标关节填当前位 + 漂移容差数值 | e2e |
| AC-E10 | 转换 helper：差分时长 / 首点宽容校验 / gripper [0,1]→rad 导入 | unit |
| AC-E11 | 对照移植清单三调用面 e2e 全绿（M1/M2/M3+M4 经 D6 同车） | e2e |
| AC-E12 | Orin 真机三步（读状态→双臂→双夹爪）受监督 demo runbook 化——**D15-R (c1+) 显式豁免下执行，安全边界五条全满足**：① 机械隔离/工作区无人/无负载/操作者在场可立即断总电源 ② auto_enable=false、每次上扭显式 SetEnabled(true) ③ 先单臂**低速/小幅**（|target−anchor| ≤ 0.087 rad、斜率 ≤ 0.14 rad/s；撤"低扭矩"宣称）再双臂、单侧异常立即 cancel 双侧+双 EStop ④ 旧栈保留可恢复启动路径 ⑤ kill -9 带扭仅残余风险证据不得称通过；**应急协议：Enabled 期间任一 S0 死亡/重启 → 立即人工切总电源并中止本轮（不得只依赖 EStop 弱 Ack）** | 真机 demo gate |
| AC-E13 | 既有 1185 基线 + 6 smoke 0 退化 | workspace |
| AC-E14 | 双臂协议全路径（r3+r4）：**Ack 偏差上限 gate**（Started.at_ns 若收到仅诊断记录，缺失不判失败）/ 终态精确集与 pair success 定义 / 提交期与执行期部分失败 cancel / partial completion 报告 / stream 断开 GetTaskStatus 收敛（含超时上限）/ 双拒绝确定性 FAIL / 不收敛双 EStop | e2e |
| AC-E15 | gripper 阶跃→插值语义变化：终点一致 + 轨迹单调（非等价宣称） | e2e |
| AC-E16 | D13 ensure-safe（r4+r6）：无条件安全写 + 单项失败继续 + 末尾聚合；写失败/readback 失败/值不安全 → 均不起新栈；**范围两档各一 case**——handoff 全 17 电机（bus1 1-8 断扭 / bus2 1-6 断扭 / bus2 7-9 先零速后断扭）+ **per-arm 本 bus 全量（round 8 P0-2 扩程：左 bus1 ID 1-8 断扭；右 bus2 ID 1-6 断扭、7-9 先零速后断扭；全部 readback——`auto_enable=false` 不消除崩溃残留扭矩，Feetech `connect()` 不写 Torque_Enable〔`adapter.rs:475-514`〕）**；**反向两条（R9-1+R10 修订）**：双 bus 同号 motor 失败按 `PreflightFailure.arm` 区分 / bus1=Err(构造) → bus2 **12 条有序写全执行**（精确序列 `[T1..T6,V7,T7,V8,T8,V9,T9]`，每轮 V 先于 T——断言序列非只断数量；handoff 合计左 8+右 12=20 条写）→ 聚合含 `Connect{Left}` | unit + 真机 gate |
| AC-E17 | 末 waypoint 先 Continue（target=末位）后 Completed（dwell=0 路径发布保证） | unit + e2e |
| AC-E18 | D3 穷尽式规则 + **序列测试**（RB5：**各分支独立 case 上下文**——先 Success 再分别 cancel/failure/IK/HoldPose-cancel，逐一断言 reset 回启动 anchor；failure 分支追加 **Degraded hold == anchor + ClearError 回 Idle**；不得只测 boot 初态、不得同上下文串测） | e2e |
| AC-E19 | S1/gateway sd_notify watchdog 双向（RB3 时序锁死）：**长时间零请求不重启** + **freeze topic 运行时注入每类 worker**——PID0 READY → 注入一次 → withholding → watchdog 重启 → **PID1 无再注入且恢复 READY**；**反向步（R5-5）**：PID1 READY 后 driver 显式再发一条 `target_pid` = PID0（已死 pid）命令 → 断言 PID1 打 "stale freeze ignored" 日志且无二次 withholding；**多 hardware case（R6-5）**：gateway 双 hardware 冻 `StatePool(1)` 单线程 → withholding（健康线程不掩蔽）（bin `sd_watchdog_smoke` / feature `sd-watchdog-smoke`） | smoke |
| AC-E20 | state cache 线程 stop+join 正常 Drop（无 mem::forget） | unit |
| AC-E21 | latch provision（r5 三分支 + r6 合法性收紧 + r8/r9）：状态链 latched Safe → ClearEStop → 仍 Safe → SetEnabled(true) → Ready；分支 case = 双缺失写入（tmp→fsync→rename）/ 单份缺失且现存合法补写 / **primary 损坏+backup 缺失 → provision 失败不创建**（+ 反方向）/ 双份存在内容不动；**dir-fsync 统一最终步骤序列测试（r9 Z2）：首次注入 dir-fsync 失败 → 第二次重试命中"双份存在"仍必须执行并成功 dir-fsync 后才 exec**（严格失败路径含目录 open/fsync → 拒启）；backup 写失败 → 自动重试补写成双份；连败预算联动见 AC-E22 | unit + e2e |
| AC-E22 | 启动拓扑强制（r4 四场景 + r5-r8 修订）：同时启动双臂 / 错峰启动第二臂（不碰已占串口）/ 直接 `systemctl start`（guard pre 失败 → 重试期绝不 exec s0_daemon）/ watchdog 自动重启（guard 重跑仍 fail-closed **且每次重跑断言本 bus 全量 ensure-safe——覆盖 Enabled 崩溃残留扭矩场景，round 8 P0-2**）/ **旧栈 active 时仅启动一臂 → 唯一结果 = handoff 对全部 17 电机 ensure-safe 后该臂启动**（断言精确 ID 集：bus1 1-8 / bus2 1-6 / bus2 7-9）/ **guard 预算四锁（r8 Y1 + r9 Z1 判据收窄）**：①连续 5 次 pre 失败 → 退 86 + `RestartPreventExitStatus=86` 停止自动重启 ②一次成功清零连败计数 ③连续 6 次成功启动/restart 不触发该预算——**只断言 guard 计数保持清零**，执行上相邻启动须留足间隔/分组避开独立 unit 级 60s/5（快速六连被 60s/5 拦是预算 ① 正确行为非缺陷，**不得写成同一条无间隔 systemd e2e**）④运行期快速崩溃仍由 unit 级 60s/5 限流；计数 I/O 异常（非法格式/越界/更新失败/清零失败）与 flock 冲突 → 均退 86 不 exec（**NotFound = 0 正常初态**；count==5 → 停止态）；**RuntimeDirectory 四态（r10 AA1）**：首次启动自动创建 / restart 保留 / 手工 stop-start 保留 / reboot 清零；**锁语义（r10 AA2）**：rename count 期间竞争进程对 `<robot_id>.lock` 不得同时拿锁成功（稳定 inode 反向复现 case）；**pre-increment 崩溃注入（r11 AB1）**：连续"递增后、完成前崩溃"（kill guard 于 preflight 中）→ 第 5 次后退 86 停止且 **s0_daemon 从未执行**；成功路径仍清零 | 真机 gate（+unit 级依赖声明检查） |
| AC-E23 | **随 D14 移出候选 E（round 7）**——归档 post-E 生产安全硬化候选（deadline 过期 → Fault，与 heartbeat 路径双 case 区分） | 移出（post-E） |
| AC-E24 | **随 D14 移出候选 E（round 7）**——归档 post-E 生产安全硬化候选（双路 overrun / stall / on_timeout 三值输出；R6 方案为历史记录，开工前须先解两 P0 前置阻断） | 移出（post-E） |
| AC-E25 | D15：真机 Enabled 态 `kill -9 s0_daemon` → **<500ms** 进入 fail-safe——**保持 OPEN 未完成，不计入候选 E demo 完结 gate**（D15-R (c1+)）；kill -9 后持续带扭 ≥10s 台架观察**仅作残余风险证据，不得描述成通过**；判据待 post-E 硬件 CodeMap 拍 (b)/(c2) 后定 | OPEN（post-E 正式退役硬门） |

## 严禁改动（hard boundaries）

1. **wire 0 改**：0 新 proto method / 0 新 fbs schema / frozen 字段 0 改
   （`JointState.layout_id` / `JointCommand.layout_id` 字段既有，仅**值语义**
   变更走 CHANGELOG）。
2. **§6.7 #2-#5 全保；#1 拆分口径（round 8 P0-1）**：#1 的 **S0-alive
   软件保证不变**（D1 的 S0 command gate = 防错位**强化**，不触
   latch/EStop 链）；#1 的 **S0-dead "靠硬件/驱动 fail-safe" 半句按
   D15-R (c1+) 作显式受监督 demo 例外并保持 OPEN**（CHANGELOG
   2026-07-15 条偏离对象含 §6.7 #1）。后续 summary/CLAUDE **不得再写
   "§6.7 全保"**，只能用本拆分口径。
3. design.md 0 改（§3.5.1 落地差异 / 解读全走 CHANGELOG）。
4. dimos-deploy 0 改（冻结 `9ece6bd`，跨仓 0 PR 永久）。
5. robotctl 0 改（辅助定位；子命令补齐 = post-E 登记）。
6. HoldPose 行为 0 改（D3 边界；AC-E8 回归锁）。
7. robot-kinematics / robot-kinematics-xlerobot 0 改；IkBackend 0 改。
8. 候选 A-D 已 land 行为 0 退化（AC-E13）；B1/B3/B4 既有契约除**两处显式
   解锁**外全保：① D3 的 Idle hold target 源（仅 Success-Scripted 提交，
   Degraded 不解锁）② D12 的 B1 决策 #7（S1 Type=simple → notify）；
   source_task_id 单调 / RR wire / events drain 全不动。

## 不在范围（登记）

底盘/头驱动与其 EStop 覆盖（D2/D13——preflight 只做交接窗口 ensure-safe，
非运行期覆盖）、mid-chunk 抢占（D5）、Gripper 独立 RPC（D6 封存 b1/b2）、
cancel/failure/IK/Degraded 终态 hold 语义改动（D3 矩阵维持现状）、robotctl
子命令补齐（D10）、deploy.sh（D12）、Python client（D-BI1(i)）、相机 /
policy 推理 / Web UI（D-BI0）、S0 unknown_layout 阈值→状态升级 +
JointLayout 广播/RR（D1 部分落地边界登记）。
**post-E "生产安全硬化候选"（round 7 归档，正式退役 D-BI3 的硬前置）**：
D14 四检测器（deadline / on_timeout / stall / overrun——R6-1/2/3 方案为
历史起点）+ 两 P0 前置阻断（CtrlLostStop 恢复不重 Enable / "新鲜未过期
命令才 rearm"）+ D15 硬件 fail-safe（(b) 断电 watchdog / (c2) 监督进程
经硬件 CodeMap 再拍）+ AC-E23/E24/E25 + 生产线程 tick 用时直方图 +
预留接口实体化（文档级契约 → 真实现）+ 撤销监督 demo freeze-diff 例外。

## 拍板记录

| # | 决策 | 拍板 | 日期 | 备注 |
|---|---|---|---|---|
| D1 | Q10 layout 链 (a)+硬约束 + 子拍板 (i) | **确认 (a)+(i)** | 2026-07-14 | P0；§3.5.1 部分落地登记；方向初拍 2026-07-13 |
| D2 | Q11 config (i) arm-only ×2 + 身份全表 + latch provision（r3-r12 全链） | **确认 (i)** | 2026-07-14 | P0；units/交接经 D12/D13；方向初拍 2026-07-13 |
| D3 | Q12 G1 (c) + 发布保证 + 行为矩阵 + 穷尽式单一规则 | **确认 (c)** | 2026-07-14 | P0；方向初拍 2026-07-13 |
| D4 | Q1 双栈双 gateway (a) | **确认 (a)** | 2026-07-14 | |
| D5 | Q2 per-chunk + 等终态 (a) + 双臂成对协议（r2-r5 全链） | **确认 (a)** | 2026-07-14 | 偏差 gate = Ack、Started 仅诊断 |
| D6 | Q3 gripper 同车 (a) + 插值语义变化登记 | **确认 (a)** | 2026-07-14 | (b) 封存备查 |
| D7 | Q4 adapter = robot-rpc client helper (iii) | **确认 (iii)** | 2026-07-14 | |
| D8 | Q5 填当前位（仅臂向量） | **确认** | 2026-07-14 | 仅 position-mode 臂向量 |
| D9 | Q6 e2e 双轨 (c) + case 库落位 robot-rpc | **确认 (c)** | 2026-07-14 | 禁反向 dep |
| D10 | Q7 验收 bin 为主 (c) + preflight/guard 入口宿主 | **确认 (c)** | 2026-07-14 | robotctl 仅旁证 |
| D11 | Q8 单位分层：rad-native helper + 原始 12 维 shim | **确认** | 2026-07-14 | raw↔rad 唯一权威 = feetech crate |
| D12 | Q9 S1/gateway sd_notify+watchdog 纳入 + 运行时 unit ×6 + handoff ×1 + runbook〔handoff transaction 权威入口〕；deploy.sh 不纳入 | **确认** | 2026-07-14 | B1 决策 #7 解锁 |
| D13 | 真机交接 fail-closed preflight（r2 新增；r3 systemd 强制；r4 无条件安全写；r5 全局 handoff + gateway Wants=S1；r6 handoff 全 17 电机拍死；r7 X1 定量依据；r8 预算拆分〔infinity 撤回〕；r9 计数硬契约 + dir-fsync 统一最终步骤；r10 RuntimeDirectory 三件套 + 稳定锁文件 + NotFound=0/0..=5/86 永久保留；r11 pre-increment 未成功尝试计数；**r12 掉电边界**〔同 boot 保留、掉电/重启清零重跑 handoff+guard〕） | **确认（含 r2-r12 全部 fail-closed、计数及重启契约）** | 2026-07-14 | **P0 安全** |
| D14 | SAFE-B1 执行链检测器——**round 7 改拍：四检测器（deadline/on_timeout/stall/overrun）移出候选 E，归档 post-E "生产安全硬化候选"**；两 P0 前置阻断登记（CtrlLostStop 恢复 Pure 不重 Enable〔`state_machine.rs:621-636`〕/ cmd_deadline_valid 借道 + 信号抑制 = Active 持续 timeout 输出〔`:202` + `ipc_tick.rs:456`〕→ 须锁"新鲜未过期命令才 rearm"）；R6-1/2/3 方案保留为历史记录非 Execute 权威 | **↻ round 7 移出 E**（原 (i+) 拍定 2026-07-14 归档） | 2026-07-15 | **P0**（post-E）；未闭合不得**正式**退役 |
| D15 | SAFE-B2 硬件 fail-safe——kill -9 <500ms 真机证明为**退役硬门**；**round 5 顺序硬门：能力核实 → 配置 + readback（条件段 9b）→ 机械隔离 AC-E25 → 才 AC-E12**；STS3215 不支持 → **任何完整真机 Enabled 动作前回 Spec 在 (b)/(c) 重拍，不得静默降级**（**① 已出"不支持"结论 → 走下行 D15-R；round 7 后顺序硬门保留为生产替换/正式退役前的门，demo 按 (c1+) 显式豁免**） | **确认；状态 OPEN 不得写成闭环** | 2026-07-14 | **P0**；design:1111-1112 规则 3/4 依据 |
| **D15-R** | 无电机侧 watchdog 下的 fail-safe 处置——**(c1+) 受监督 demo 豁免 + 接口预留**：不采购继电器/不实现监督进程/不预选 (b)(c2)；D15 保持 OPEN；AC-E25 不入 demo gate；D-BI3 正式退役推迟（旧 unit 可 stop 不得 disable/删除/破坏回滚）；收口宣称 = "受监督功能 demo 跑通"；design:1107 显式 demo 例外走 CHANGELOG freeze-diff；预留接口 = 文档级契约（arm/kick/request_safe/status+readback；handoff/guard 与 SetEnabled(true) 之间；fail-closed；不写 no-op trait） | **确认 (c1+)** | 2026-07-15 | **P0**；能力核实收窄口径 = "无已文档化、可配置的 command watchdog" |
| D16 | SAFE-B3 + 收口性质——**(ii) 受监督 demo 修正版（round 7 同步）**：真机 Enabled 动作按 D15-R (c1+) 显式豁免执行（round 5 "先过 D15 全链"收紧由豁免取代）；**正式退役推迟 post-E**（demo 期间旧 unit 可 stop 不得 disable/删除/破坏回滚）；收口宣称唯一口径 = "机器人控制执行面受监督功能 demo 跑通" | **确认 (ii) 修正版** | 2026-07-14/15 | **P0**；CHANGELOG premise 补登记 |
| D17 | SAFE-B4 跨臂联锁——**(ii) 明确延期**：登记"不可无人值守双臂运行"；client cancel + 双 EStop = 受监督边界 | **确认 (ii)** | 2026-07-14 | P1 |

---

# 安全架构横切审查回写（2026-07-14，reviewer SAFE-B1..B4 + 资源裁决）

> 触发：reviewer 横切审查否定"驱动/心跳看门狗已齐"与"资源门闩纯可用性"两个
> 本人先前表述，并把三项 P0 升为 Spec 级拍板位。**D14-D17 已全部拍定
> （2026-07-14，round 4 落拍 + round 5 修正）；round 7 范围收缩：D14
> 检测器移出候选 E、D15 保持 OPEN、D15-R 拍 (c1+) 受监督 demo 豁免**；
> 正式退役（D-BI3）推迟 post-E 生产安全硬化候选。

### D14（SAFE-B1，P0）↻ round 7 改拍：检测器移出候选 E——归档 post-E 生产安全硬化候选

**实证现状**（"看门狗已齐"表述**撤回**）：`JointCommand.deadline_ns` 只进
buffer、执行路径不检查；`on_timeout` 未进 `JointCommandBuf`、运行时固定
`CtrlLostHold`（`ipc_tick.rs:461` 仅 hint）；`CmdDeadlineExpired /
RtOverrun / LimitViolation / WatchdogTimeout` 只有状态机转移
（`state_machine.rs:645`）无生产事件触发器——RtOverrun 现仅
`metrics.overruns.fetch_add(1)`（`rt_thread.rs:292`）。**两套语义严格分离
不混写**：deadline 过期 → `Fault(CmdDeadlineExpired)`（命令失效）；
heartbeat 超时 → `CONTROL_LOST_*` 子态（链路失效）。

- **round 7 改拍（reviewer，2026-07-15）**：段(5b) 的 deadline /
  `on_timeout` / stall / overrun 重写**移出候选 E**，归档为 post-E
  **"生产安全硬化候选"**——原因不是不重要，而是不属于受监督功能 demo
  的最短闭环，且方案仍有两个 P0 语义冲突（下）。R6-1 原子协议 / R6-2
  输出方案 / R6-3 阈值研究**保留在 round 5/6 amend 历史记录，不作为本
  候选 Execute 权威**；P.2.5b 原 "≤ D15 硬件 timeout 典型 200ms" 论证
  随 D15 能力核实结论失效**已删**。
- **post-E 前置阻断登记（两 P0，硬化候选开工前必须先解）**：
  1. **CtrlLostStop 恢复语义冲突**：`CtrlLostStop` 已执行 torque-off
     后，heartbeat 恢复是 **Pure** 转移直接回 `Enabled.Active`
     （`state_machine.rs:621-636`；classify 仅 Enabled+Disable 为
     Effectful）——**不会重新 `Enable(true)`**，出现"状态报告 Active、
     硬件仍断扭"。
  2. **rearm 借道冲突**：deadline Fault 被消费后，`ClearError` 可借
     现有 `cmd_deadline_valid` guard 再进 Active
     （`state_machine.rs:202` ready_to_enabled 合取项，
     `ipc_tick.rs:456` 现硬编码 `true`）；而 `last_signaled_seq` 又
     禁止旧命令重新报 Fault——形成 **Active 状态下持续 timeout 输出**。
     后续方案必须锁成**"新鲜、未过期命令才重新武装并允许
     Ready→Active"**。
- **〔历史：round 4-6 拍定链，round 7 已改拍移出，非 Execute 权威〕
  (i+)（round 4；(i) 不批准——两处纠正）**：① 本人"WatchdogTimeout
  进程级已由 systemd 覆盖"表述**撤回**——design §6.2 心跳表（`00_design.md:
  1023` 区块）明确区分 **S0 RT→IPC 内部 watchdog（1 kHz / 10 ms → FAULT +
  safe hold）** 与 **systemd watchdog（2 s → 重启进程）**，两者不可互代；
  ② 现 RtOverrun 检测只在 HardRT（`rt_thread.rs:290`），**XLeRobot 走
  SerialRT，`SerialIoThreadMetrics` 无 overrun 指标**（`serial_io_thread.
  rs:128-136` 实证）——原 (i) 保护不到 E 真机路径。
  **(i+) 范围 = E 段 5b（插段 5 与 6 之间）四件全闭环**：deadline /
  `on_timeout` 运行时生效 / **内部 executor stall（WatchdogTimeout）** /
  **HardRT + SerialRT 双路 overrun 事件化**。**round 5/6 修正（R5-2 +
  R6-3）**：round 4 "HardRT 10ms 设计字面"直搬是误迁移（实际 HardRT
  默认 50 Hz/20 ms，`s0_daemon.rs:153`，systemd unit 不传参——10 ms 在
  正常 tick 间隔内即误报）；R6-3 终版 = stall 阈值公式
  **`max(10ms, 10×周期)`**（1 kHz 下逐字还原冻结值 10 ms），**作为
  design-freeze diff 走 CHANGELOG 登记 + 安全论证**（冻结字面
  `00_design.md:1025` = "1 kHz / 10 ms" 无缩放公式）；SerialRT 实际
  固定 5 ms/200 Hz（`s0_daemon.rs:809` 用
  `SerialIoThreadConfig::default()`，`serial_io_thread.rs:124`）且
  SLA 8+4 ms 是未采集目标非基线——SerialRT overrun **两相制**（shadow
  → Orin 基线回填 → enforce）。
  全部具体值（周期来源 / lateness 与 work-duration 双指标分离 / 阈值 /
  信号通路 / rearm / Fault 输出策略）**P.2.5b 落死**（不留 Execute）。
  **Fault 输出策略拍定（R5-3）：沿既有 `OutputPolicy::Disabled`
  （`state_machine.rs:392` 字面，状态机 0 改）**——round 4 本人"safe
  hold 输出"宣称**撤回**（stall 场景下 hold 依赖的正是已停摆的执行链；
  Disabled 与 §6.5.1 硬件 fail-safe 同向）；design §6.2 行"输出 safe
  hold"措辞与实现的偏离**走 CHANGELOG 段 5b 登记**（不改 00_design.md）。
  `LimitViolation`（需 per-joint limits 配置面）延后独立候选。
- **共同硬约束（round 7 仍有效）**：本项未闭合前**不得正式退役旧栈**
  （D-BI3 正式退役推迟至生产安全候选闭环；demo 期间旧 unit 可 `stop`，
  **不得 disable/删除/破坏回滚**）。
- AC：AC-E23 / AC-E24 **随本项移出候选 E**（表行保留标注归档）。

### D15（SAFE-B2，P0）✅ 已拍（顺序硬门）· ⚠ 状态 OPEN 不得写成闭环：硬件 fail-safe blocking gap（正式退役硬前置）

**实证**：`docs/10_verification.md:103` 原文 "kill -9 s0 → torque
disable/zero/brake <500ms = **⏳ blocking gap**"。Feetech 1s 超时 = **主机
串口读超时**（`registers.rs:43`）**非电机侧 command watchdog**；adapter
未配置电机侧 watchdog；"失联 hold-last-position 且持续带扭" ≠ 冻结要求的
torque disable/zero/brake——本人先前"登记为语义澄清"表述**撤回**。

- **已拍（round 4）：确认为退役硬门**；**round 5 追加（R5-1）：真机
  动作顺序硬门**。执行序严格 = ① **STS3215 能力核实**（寄存器表 /
  datasheet + 台架单电机机械隔离实测：电机侧 command watchdog 能力；
  可与软件段并行做）→ ② **受控电机全集配置 watchdog + readback 断言**
  （adapter connect 期，条件段 9b；**① 出"不支持"后本步由 D15-R 所拍
  方案的落地核验步替代**）→ ③ **机械隔离条件下完成 AC-E25**
  （真机 Enabled 态 `kill -9 s0_daemon` → 实测 <500ms 进入 torque
  disable / zero / brake）→ ④ **才允许 AC-E12 双臂动作**。**若 ① 不
  支持 → 必须在任何完整真机 Enabled 动作之前回 Spec 在 (b) 继电器・
  独立硬件 watchdog / (c) 等价方案中重新拍，不得静默降级**。
- 根本依据补录（design 冻结字面，`00_design.md:1111` 规则 3 + `:1112`
  规则 4）：**不支持硬件 timeout 的电机不得标 HardRT、不得进入
  `ENABLED.ACTIVE` 自动控制**；systemd Restart 是恢复机制非安全机制。
  round 4 版把受监督真机验证放在 kill -9 证明之前，违反该字面——
  **顺序已改**（AC-E25 先于 AC-E12，步序见 P.2.10）。
- **D-BI3 退役前必须** AC-E25 通过（journal + 测量方法进证据 docs）。
- **① 能力核实结论（2026-07-15，桌面证据链三锚点；round 7 收窄口径）：
  STS3215 无已文档化、可配置的 command watchdog，且当前真机失联持续
  带扭——"不支持"分支触发**：
  1. dimos-deploy `feetech_io/registers.py`（表头自证"完整表见 lerobot
     源码 feetech/tables.py" + 官方手册链接）：16 项无任何通信看门狗/
     命令超时/失联断扭寄存器；
  2. lerobot 完整 `STS_SMS_SERIES_CONTROL_TABLE`（huggingface/lerobot
     main `src/lerobot/motors/feetech/tables.py`，2026-07-15 取原文
     核对）：**58 项逐一核过，无该类寄存器**——`Protection_Time(35)` /
     `Over_Current_Protection_Time(38)` 是过载/过流保护时长，非通信
     看门狗；公开渠道反向搜索同样无该能力记载（仅 UART 缓冲复位机制，
     与断扭安全无关）；
  3. 真机行为证据：失联 = hold-last-position **持续带扭**（本节实证段 +
     `10_verification.md:103` blocking gap ⏳ 行）——若存在默认生效的
     watchdog 不会呈现此行为。
  台架观察步（非寄存器写）：Enabled → `kill -9` → 观察扭矩持续
  ≥10 s，留 journal——**保留为行为证据 / 残余风险证据**（round 7 口径：
  不得描述成任何"通过"）。
  **依 D15 已拍分支回 Spec 重拍 → D15-R 已拍 (c1+)（2026-07-15，见
  下节）：D15 本身保持 OPEN 不得写成闭环；AC-E25 <500ms 保持未完成、
  不计入候选 E demo 完结 gate；原段 9b（写 watchdog 寄存器）不可执行
  且 (c1+) 下无替代落地段——已删。**
- **round 7 边界（D15-R (c1+) 拍定后）**：上述四步顺序硬门保留为
  **生产替换 / 正式退役前**的门；候选 E 受监督 demo 按 D15-R (c1+)
  **显式豁免**执行 AC-E12（安全边界五条全满足，见 D15-R / P.2.10），
  该豁免 = design `00_design.md:1107`（§6.5.1）冻结规则的显式 demo
  例外，**走 CHANGELOG freeze-diff 登记（2026-07-15 条），不改
  00_design.md**。

### D15-R（P0）✅ 已拍 (c1+)（2026-07-15，round 7）：受监督 demo 豁免 + fail-safe 接口预留

**拍定 (c1+)——受监督 demo 豁免 + 后续 fail-safe 接口预留**（reviewer
round 7 原文条款照录；按最新目标收缩：先跑通受监督功能 demo，生产级
安全闭环后置）：

- **本候选不采购继电器、不实现独立监督进程、不在 (b)/(c2) 间预选**——
  实际 API / 供电拓扑 / (b)(c2) 选择等**随 post-E 硬件 CodeMap 再定，
  禁止现在凭空抽象**。
- **能力核实结论收窄口径**："STS3215 **无已文档化、可配置的** command
  watchdog，且当前真机失联持续带扭"；≥10 s 台架观察保留为**行为证据**。
- **D15 保持 OPEN，不得写成闭环**；**AC-E25 <500ms 保持未完成，不计入
  候选 E demo 完结 gate**。
- **D-BI3 正式退役推迟**：旧 unit demo 期间可 `stop`，**不得 disable、
  删除或破坏回滚能力**（决策单同步修订，见 CHANGELOG 2026-07-15 条）。
- **收口宣称唯一口径**："机器人控制执行面**受监督功能 demo 跑通**"——
  **不得宣称生产替换、无人值守或硬件 fail-safe 完成**。
- **freeze-diff**：本项 = 对 design `00_design.md:1107`（§6.5.1）冻结
  规则的**显式 demo 例外**，走 CHANGELOG 登记（2026-07-15 条），不改
  00_design.md；**无人值守生产化前必须补硬件 fail-safe 并撤销本例外**。
- **demo 安全边界（AC-E12 执行前置，五条全满足）**：
  1. 机械隔离、工作区无人、无负载，操作者全程在场且可立即切断总电源；
  2. 新 unit 不设开机自动控制——`auto_enable = false`，每次上扭均显式
     `SetEnabled(true)`；
  3. 先单臂**低速/小幅**验证（round 8 P1-2 可判定数值：单臂探针
     |target−anchor| ≤ **0.087 rad**、轨迹最大斜率 ≤ **0.14 rad/s**——
     对应既有 ±5°、0.25 Hz 真机验证量级；**撤"低扭矩"宣称**：Feetech
     路径无可 readback 的 torque/current 限制面），再双臂；任何单侧
     异常立即 cancel 双侧并双发 EStop；
  4. 旧栈保留可恢复启动路径；
  5. kill -9 后持续带扭**不得描述成通过**——只能作为残余风险证据。
- **预留接口 = 仅文档级集成契约（不写假 no-op watchdog trait）**：
  1. 未来接口需覆盖 **arm / heartbeat(kick) / request_safe(disarm) /
     status+readback** 四类操作；
  2. 集成点位于 **handoff/guard 与允许 `SetEnabled(true)` 之间**；
  3. **缺失、失联或 readback 失败必须 fail-closed**（不得进入
     Enabled）。

三选项事实单（**历史拍板依据**；reviewer 按最新目标拍 (c1+)——(b)/(c2)
的前置事实核实项〔供电拓扑、候选硬件件〕随 post-E 硬件 CodeMap）：

| 项 | (b) 硬件断电 watchdog | (c2) 独立监督进程断扭 | (c1) 受监督豁免登记 |
|---|---|---|---|
| 机制 | 电机供电轨串外置硬件看门狗继电器；S0 周期喂（GPIO/USB）；喂停 → 断电 → 断扭 | 常驻最小监督进程监视 S0（pidfd/heartbeat，50-100ms 周期）；S0 死 → 串口 fd 已释放 → open + broadcast `Torque_Enable=0` | 不新增自动 fail-safe；显式登记 design-freeze diff：受监督 demo（人值守 + 断电手段在手 + 机械隔离步先行）豁免 rule 3 |
| 冻结字面合规 | **满足** rule 2/3（断电属硬件/驱动层） | **不满足** rule 2 "驱动层/硬件"字面（用户态软件）——需 freeze-diff 登记 + 论证（软件单点 S0 → "S0+监督进程"二取一活） | **不满足**——freeze-diff 登记 + 论证（残余风险 = kill 后至人工断电窗口内电机 hold 静止带扭，非失控运动） |
| <500ms 可达性 | 是（watchdog 超时 50-200ms 可配） | 是（检测 ≤200ms + open/write ≤100ms） | **否**——AC-E25 无法自动达成，`10_verification.md:103` ⏳ 保留不销 |
| AC-E25 判据 | kill -9 → 供电轨断电观测（电压跌落/电机 bus 不可达）时差 <500ms | kill -9 → 监督进程 torque-off write ack journal 时差 <500ms + 手拨验证无阻力 | 改测"失联行为 = hold 静止 + 软件 shutdown 路径断扭"——**非** <500ms 自动 fail-safe |
| E scope 影响 | +硬件采购/接线/喂狗接口（D-BI0 冻结基线外，scope 变化需拍）；落地前核实双臂供电拓扑（单轨一继电器覆盖双臂——单臂 S0 死双臂断电，保守侧） | +1 个小型常驻进程 + unit + 串口接管协议（S0 活着时绝不碰串口；与 guard restart 链 preflight ensure-safe 的先后协议） | 0 新增工程；**但 D15 永不闭合 → E 无退役收口**（D-BI3 推迟到"无人值守生产化"候选补硬件 watchdog 后）——买入决策单预期变更需拍 |
| 退役链 | 保住（AC-E25 可过） | 保住（AC-E25 可过；安全等级=软件双活非硬件级，登记） | **断**（E 收口 = 受监督 demo，无退役） |

（(b)/(c2) 前置事实核实项——双臂供电拓扑、候选硬件件、串口接管协议——
全部随 post-E 硬件 CodeMap，本候选不做。）

### D16（SAFE-B3 + 收口性质，P0）✅ 已拍 (ii) 修正版（round 7 demo 口径同步）：物理急停 与 E 终点宣称

**实证**：`10_verification.md:100` physical estop button → latched
**<5ms = ⏳** 未实现；双 robot_id 后一颗物理按钮需同时覆盖两臂并持久化
**两侧** latch。

- (i) E 实现物理急停（<5ms 硬件级路径 + 双 topic/双 latch 扇出）——硬件
  工程项，E scope 大增。**未采**。
- **已拍 (ii) 修正版（round 4——"未闭合仍 disable"删除）**：E 收口定性 =
  **"受监督 demo 替换"**，但退役与 disable 服从硬门：
  1. **软件段（1-9）与真机只读/非 Enabled 验证**随时可做；**真机
     Enabled 态动作（含 AC-E12 双臂/夹爪）按 D15-R (c1+) 显式豁免
     执行**（demo 安全边界五条全满足；design `00_design.md:1111`
     规则 3 的 demo 例外走 CHANGELOG freeze-diff 登记——round 5 的
     "先过 D15 全链才可 Enabled"收紧由该显式豁免**取代**）；
     **不得执行 D-BI3 正式退役**——demo 期间旧 unit 可 `stop`，
     **不得 `disable`、删除或破坏回滚能力**（决策单 :139 已定义
     disable = 正式退役动作）；
  2. **正式退役推迟至 post-E**：D14 两 P0 前置阻断 + D15 硬件
     fail-safe（生产安全硬化候选）闭环后才允许 D-BI3 退役
     （disable 旧栈）；
  3. 物理急停（<5ms）未闭合 → **不得宣称生产可用或无人值守可用**——
     收口宣称唯一口径（round 7）= **"机器人控制执行面受监督功能 demo
     跑通"**，不得宣称生产替换 / 无人值守 / 硬件 fail-safe 完成；
     监督条件明文（真机运行必须有人值守 + 硬件断电手段在手）；
  4. 本人原"dimos 旧栈同样没有——退役不降低安全水位"豁免论证**撤回**
     （design `00_design.md:1110` 规则 3/4 是**冻结不变量**，不因对比
     豁免）。
  SAFE-B2 / SAFE-B3（+ D17(ii) 跨臂联锁）构成后续**"无人值守生产化"独立
  候选硬前置**；`10_verification` 两行 ⏳ 登记**原样保留不销**；触 D-BI0
  宣称面 → **CHANGELOG premise 补充登记**。

### D17（SAFE-B4，P1）✅ 已拍 (ii)：跨臂联动精确化

已有 = client 侧 pair 编排（单侧失败 → cancel 另侧 → 不收敛双 EStop）。
**真缺口** = client 死亡或一侧 S0 自主 Fault/EStop 时，无
daemon-independent 跨臂联锁。

- (i) 纳入 E：双侧 state 监测（互订对侧 estop/state 或独立监督进程），
  一侧 Fault/EStop → 自动双 EStop + 故障注入 e2e。**未采**。
- **已拍 (ii)（round 4，与 D16(ii) 联动）**：明确延期——登记**"不可用于
  无人值守双臂运行"**；**client 侧 cancel + 双 EStop 保留为当前受监督
  运行边界**（受监督 demo 定性的组成部分）。

### 资源门闩裁决（reviewer 落定，本节照录执行）

- robot-health 现状 = 只采样/日志、无阈值执法（实证 lib 零 threshold/
  enforce）；**暂不给 S0 加 `CPUQuota`/`MemoryMax` 合理**（错误限额可能
  主动杀死安全进程）。
- **E 内做两件**：① 段(10-demo) 真机工作流**记录 S0/S1/gateway 的
  CPU / RSS + synthetic iceoryx2 transport latency baseline**
  （robot-health 采样 + 既有 bin + runbook 采集步 + 收口 docs 附；
  round 9 非阻断改准——首句直接用收窄口径防摘要/grep 误取；生产线程
  真实调度抖动留 post-E，权威细则 P.2.10）；
  ② 段(9) 新 unit 注释**明文记录"资源策略为何暂不设限"**（CPUQuota vs
  SCHED_FIFO throttling / MemoryMax vs mlockall OOM-kill）。
- **阈值执法另开 Spec**（S0 与 S1/gateway 分开评估）。本人先前两处表述
  撤回：不得笼统称"纯可用性优化"；不得以尚未闭合的硬件 fail-safe 作延期
  依据。

> Spec round 12 Approved + D1-D13 终拍（2026-07-14，`3f49230`）后授权。
> **outline 级**：定段序 / 每段 scope 与 gate / 拍板项；P.2 细化随 Plan
> round 展开。**0 代码改动直至 Plan ACK**。

## Plan round 1 amend 记录（reviewer 复核后）

4 项（PC1 P0 + PC2-PC4 P1）+ P.3 五项拍板，已回修/落值：

| # | 级别 | 阻断 | 修订 |
|---|---|---|---|
| PC1 | P0 | **段 (2) 不能独立落地**：`motor_ids` 一旦成 serde 已知字段就不再进 extras（`config_loader.rs:68-78` HardwareSection typed 结构），但 `build_adapter` 仍读 `extras["motor_ids"]`（`s0_daemon.rs:1163`）——只改 loader 段 (2) 即破 S0 | 段 (2) **原子包含消费端迁移** + **S0/S1 两套 `HardwareSection`** + 全部 struct fixture 与 TOML producer；**全仓 `[[hardware]]` producer census 入 P.1**（实测名单：`case_ctx.rs` / `set_enabled_e2e.rs:264` / `estop_e2e.rs:369` / `state_rpc_e2e.rs` / `gateway.rs` 测试 helper / `crash_restart_smoke.rs` / `freeze_systemd_smoke.rs` / `scripted_backend_smoke.rs` / `pubsub_cleanup_smoke.rs` / `s1_daemon.rs`+`coordinator.rs`+`state_pool.rs`+`robot_rpc_gateway.rs` 各自 HardwareSection 消费/定义面 / configs 12 个 TOML） |
| PC2 | P1 | **release-first feature 集漏 `clear-estop-e2e`**（`robot-tests/Cargo.toml:52` required-features 守门）——stale/missing binary 风险 | P.0 命令补 `clear-estop-e2e`；**后续新增验收 feature 与 `xlerobot-real` guard bin 时必须同步加入显式 release build 清单**（P.0 落为活清单条款） |
| PC3 | P1 | **PD-E3 只有题目无方案**；段 (7) "双栈三真 daemon" 易误读 | PD-E3 按 reviewer 方案落值（见 P.3）；段 (7) 锁 **2×(s0+s1+gateway) = 6 个子进程** |
| PC4 | P1 | **PD-E5 (b) 与已拍 D1 冲突**：一切 command producer 从 config 派生 layout；mock_s1 现无 `--config` 入口（实测 0 命中） | PD-E5 拍 (a)；段 (4) 明确 **mock_s1 补 `--config` 入口 + 既有 crash-smoke 等调用方迁移**；显式 `--layout-id` 仅一致性断言、冲突 fail-fast |

## Plan round 2 amend 记录（reviewer 复核后）

6 项（QA1-QA3 P0 + QA4-QA6 P1）+ 非阻断同步 ×3，已回修：

| # | 级别 | 阻断 | 修订 |
|---|---|---|---|
| QA1 | P0 | **段(9) 重新引入 ExecStartPre，违反 Y1 终拍**：退出码 86 只有 guard 作主进程才被 `RestartPreventExitStatus=86` 捕获 | P.2.9 改准：arm S0 unit **无 ExecStartPre**；`ExecStart=xlerobot_deploy guard --arm … --config … --s0-bin <绝对路径> -- <s0 args>`；handoff unit `ExecStart=xlerobot_deploy preflight --scope handoff`；**unit ×7**（撤"六件"笔误） |
| QA2 | P0 | **D3 终态数据通路未定义**：`ActionTickOutput` 只携 `ActionOutcome`（`action.rs:397` 区块），`CurrentAction` 不存 backend kind / 末点 / final_published（`coordinator.rs:1284`）——idle_hold_target 无法实现 | 拍死机制（reviewer 案）：**`ActionInstance` trait += `fn successful_hold_target(&self) -> Option<Vec<f64>>`（默认 None）**；仅 Scripted 在末点已发布后返 Some(末点)；coordinator 于 `Completed(Success)` 时取用，None/其余终态天然 reset anchor——kind 无需另存 |
| QA3 | P0 | **PD-E6 推荐前提错误**：`systemd_notify.rs` 用 tracing 非纯 std，上移 robot-core 破坏零依赖边界；**`robot-health` 已存在**且定位"非 RT、robot-rpc 等 daemon 可复用"（`robot-health/Cargo.toml` 注释原文），S0 已依赖、无循环 | **PD-E6 拍 (c)：`robot-health::systemd` 单一实现**（S0 实现迁入，S0 改调；S1/robot-rpc 各加一条 workspace 依赖边）；P.0 改口"**0 新外部 crate**" + 依赖边变更段随跑 `cargo --timings` + `cargo tree` + deny |
| QA4 | P1 | **gateway probe 把 S1 健康绑进 gateway**：`GetTaskStatus(unknown)` 真 forward S1 RR 并等 ≤5s（`task_handlers.rs:1045` step 9）——S1 重启 → gateway 停喂 watchdog，违反 Wants= 终拍语义 | probe 改 **本地 `GetStateRequest { hardware_id: "" }` → `InvalidArgument`**（经 tonic transport/handler，不依赖 S0/S1/cache 首帧） |
| QA5 | P1 | **client helper 签名不足**：`load_hardware_layout` 无 Result、`HardwareLayout` 未定义；`raw12_to_rad -> [f64;12]` 表达不了 NaN/range 反转/clamp 计数，且**左右夹爪标定范围不同**；`dual_arm_workflow` "段 6 骨架段 7 填"违反跨段禁半成品 | 签名 typed 化（见 P.2.4/P.2.6）；shim 携**双 gripper range** + `clamp_count` 返 `Result`；**dual_arm_workflow 完整模块原子放段(7)**，段(6) 不留 stub |
| QA6 | P1 | **三个 e2e case 不可执行**：六进程拓扑是真 S0 无 FakeState 可停；S1 错 layout 会启动期 fail-fast；Scripted 运行期只 Continue/Completed、IK 错误在 start 阶段——failure / 执行期 partial-failure 无生产触发器 | stale case = **SIGSTOP 对应 S0 + 等 freshness 超时**；E2 跨进程 = **独立 raw `JointCommand` 注入器**（test 工具直发错 layout 帧到 S0 command topic）；failure 触发 = **test-only fault backend**（robot-s1 feature 守门，**入 P.0 活清单**；"Reverse Sync 降 AC 验证层级"备选未采） |

非阻断同步 ×3（已落字）：S1 `HardwareSection` 实际在 `coordinator.rs:301`（P.2.2 更正）；`unknown_layout_count` 明确 `AtomicU64::fetch_add(1, Relaxed)`（P.2.3）；schema_lint 用**硬编码 fixture 直接补 (name,id)**，撤"读取段 2 config census"描述（P.2.1）。

## Plan round 3 amend 记录（reviewer 复核后；SAFE-B1..B4 回写 Spec 主体 D14-D17）

5 项 Plan 阻断（R3-B1..B3 P0 + R3-B4/B5 P1）+ 非阻断 ×2，已回修：

| # | 级别 | 阻断 | 修订 |
|---|---|---|---|
| R3-B1 | P0 | **FaultInjectBackend 无可执行分发键/构建链**：backend 按 (kind, schema_id) 分发（`mod.rs:162` enabled 列表机制），"N tick 后失败"缺 supported_kinds/schemas、profile、冲突消除、失败变体、release 命令 | P.2.8 锁死：`ActionKind::DwellMs + NO_PAYLOAD_SCHEMA_ID`（既有 wire 键 0 新增）/ test profile 启用 `fault_inject` **显式移除 `mock_dwell`** / `fail_after_ticks = 3` 固定 / 失败 = `Err(ActionFailure::InternalError("fault_inject"))`（实证枚举域）/ feature `test-fault-backend` **永不进 default** / e2e 前显式 `cargo build --release -p robot-s1 --bin s1_daemon --features test-fault-backend`（入 P.0 活清单） |
| R3-B2 | P0 | **sd_notify 段未达函数级**：robot-health::systemd API 未定（线程名硬编码 `s0_systemd_wd`）；gateway ready 顺序错位（现 ready_tx 在 bind 后 serve 前）；freeze hook/WorkerKind/feature 未定义；release 命令缺 | P.2.7 补齐：API 签名（thread_name 参数化）/ 顺序锁死 **worker ready → bind → tonic serve → probe 首成 → READY=1 → watchdog enforce**、shutdown **STOPPING=1 → 停 probe/watchdog → 原 graceful**、**既有 bind-ready `ready_tx` 原样保留另建 systemd-ready 通路** / `WorkerKind` + `ROBOT_TEST_FREEZE` env + feature `test-freeze-hooks`（default-off，入活清单）/ release 命令两条入 P.0 |
| R3-B3 | P0 | **state cache spawn 契约缺失败路径**：factory 无类型、创建可失败却不返 Result | P.2.5 锁死：`spawn_state_cache_thread(cfg, clock) -> Result<StateCacheHandle, StateCacheSpawnError>`（typed 四变体）/ node+subscriber 线程内建 / sync ready 握手（超时 → 回滚 stop+join）/ **cache ready 前 daemon 不得打 ready marker** / Handle stop/join/Drop 职责精确（模板 `StatePool::spawn`，**禁 mem::forget**） |
| R3-B4 | P1 | **wrong-layout e2e 矛盾与竞态**：P.2.3 残留"S1 侧注入"与 P.2.8 注入器冲突；六进程下正常 S1 持续发合法命令 + latest() 塌缩 → 假 PASS；"计数可观测时断言"非确定性 | P.2.3 残句删；case 锁**独立 mini 场景**：不起对应 S1（s0 + FakeHeartbeat 维持 Enabled + raw 注入器）→ baseline target → 注入 ≥3 帧错 layout 明显不同 target → 断言 state/output 不变；`unknown_layout_count +1` **仅 unit 锁**；模糊口径删除 |
| R3-B5 | P1 | **部署段缺函数签名**：safety-critical 启动链不能留 Execute 自行设计 | P.2.9 补齐 `GuardConfig`/`run_guard`/count-lock API/`PreflightScope`/`DeployTransport` trait/`ensure_safe`/`provision_latch`/s0_bin 绝对路径校验/`exec` 失败路径/聚合报告结构（全签名见 P.2.9） |

非阻断 ×2（已落字）：P.1 段(6) "case 库骨架"措辞改具体 helper 名（dual_arm_workflow 全在段 7）；AC-E1 `domain_version_isolated`（"改常量"不可执行）→ **golden vector**（固定输入 → 固定 u64 快照）。

## Plan round 4 amend 记录（reviewer 复核后；D14-D17 + 资源全部落拍）

6 项（RB1-RB4 P0 + RB5/RB6 P1），已回修：

| # | 级别 | 阻断 | 修订 |
|---|---|---|---|
| RB1 | P0 | **D14 漏内部 WatchdogTimeout 且不覆盖 SerialRT**：design §6.2 双看门狗（内部 1kHz/10ms→FAULT vs systemd 2s 重启）不可互代；RtOverrun 只在 HardRT（`rt_thread.rs:290`），XLeRobot 走 SerialRT 无 overrun 指标（`serial_io_thread.rs:128-136`） | D14 改拍 **(i+)** 四件全闭环（deadline / on_timeout / 内部 stall〔双 RT 类〕/ 双路 overrun）；**P.2.5b 落具体值**（X%/N / SerialRT stall 阈值 / 重复抑制） |
| RB2 | P0 | **D16 "未闭合仍 disable"与三处权威口径冲突**：D14/D15/段(10) 均"未闭合不得退役"，决策单 :139 定义 disable = 正式退役；"dimos 同样没有"不能豁免冻结不变量（design:1110 规则 3/4） | D16 修正版落拍：未闭合可软件段 + 受监督验证，**不得退役不得 disable**；豁免论证撤回 |
| RB3 | P0 | **env freeze 证明不了 watchdog 重启**：`ROBOT_TEST_FREEZE` 启动时读取——TonicProbe 会在首次 probe 前冻结（测到启动失败非 watchdog 超时）；env 跨 systemd 重启存续 → PID1 再冻结 | 撤 env 方案；沿 **S0 `freeze_control.rs` 先例 = test-only iceoryx2 topic 运行时注入**（feature 名对齐 S0 现名 **`test-freeze-hook`** 单数）；smoke 时序锁死 **PID0 READY → 注入一次 → withholding → PID1 切换 → PID1 无再注入 + 恢复 READY**；smoke **bin/feature 现在锁名**：`sd_watchdog_smoke` / `sd-watchdog-smoke` |
| RB4 | P0 | **loopback probe 不可编译 + 缺并发拓扑**：`RobotControlClient` 仅 `client` feature 生成（`build.rs:26`）；`serve_with_incoming_shutdown` await 到停机（`gateway.rs:1781`）不能线性 serve→probe | 锁 **低层 unary probe**（`tonic::client::Grpc<Channel>` + 既有 prost message + 显式 PathAndQuery，**不开 client feature 不改生产边界**）+ **并发拓扑/所有权/早退/shutdown 顺序全锁**（P.2.7） |
| RB5 | P1 | **两 e2e 判据可假通过**：`InternalError` → coordinator **Degraded**（`coordinator.rs:1746-1754` 实证）——矩阵后续分支会被拒；wrong-layout case 缺正向对照 | 矩阵**各分支独立 case 上下文** + failure 分支追加 Degraded hold 断言 + maintainer `ClearError` 回 Idle；wrong-layout 先 Enabled.Active 确认 → **matching-layout 命令观察输出确实变化** → 再错帧证明保持前值 |
| RB6 | P1 | **"调度抖动"无数据源**：robot-health 只采 CPU/RSS（`lib.rs:56-62`） | 数据源改 = **段 5b 新增 tick 用时直方图指标**（HardRT/SerialRT 各自测量点；RT 侧仅 fetch_add 固定桶）；窗口/percentile/归档格式 P.2.10 落死 |

## Plan round 5 amend 记录（reviewer 复核后；D15 顺序 + D14 全套可执行化）

5 P0 + 2 P1，已回修（RB2/RB5/D17 延期边界/资源方向 reviewer 确认保留不重开）：

| # | 级别 | 阻断 | 修订 |
|---|---|---|---|
| R5-1 | P0 | **D15 真机顺序违反冻结设计**：round 4 版允许 D15 未闭合时受监督真机验证、段(10) 先双臂后 kill -9——design `00_design.md:1111` 规则 3 冻结字面 = 未证明硬件 timeout 的电机不得进入 ENABLED.ACTIVE | 改序硬门：**能力核实 → 配置 + readback（新条件段 9b）→ 机械隔离 AC-E25 → 才 AC-E12**；不支持 → 任何完整真机 Enabled 动作前回 Spec 重拍 (b)/(c)；D16 §1 收紧为"真机只读/非 Enabled 验证"（D15/D16/拍板表/AC-E12/P.1/P.2.10 全同步） |
| R5-2 | P0 | **D14 周期与阈值在正常运行下误报**：HardRT 实际默认 50 Hz/20 ms（`s0_daemon.rs:153`，systemd 不传参）——10 ms 无进展在正常 tick 间隔内即触发；SerialRT 忽略该参数固定 5 ms/200 Hz（`s0_daemon.rs:809` `default()` + `serial_io_thread.rs:124`），150% = 7.5 ms 而 SLA 允许单 tick read+write ~12 ms（`10_verification.md:120-121`）→ 连续误触发；既有 `overruns` 是 schedule lateness 非 work duration（`rt_thread.rs:291-292`）；>500% 开口桶无数值 max；整进程 SIGSTOP 只证 systemd 层 | 周期来源拍定 = **HardRT 读 resolved `args.tick_period_ns`、SerialRT 保留固定 5 ms 阈值锚 I/O 预算**（方案 b，0 行为改，不混写）；**lateness / work-duration 双指标分离**（Fault 只接 work-duration）；stall = **10×各自周期**（§6.2 "10ms@1kHz" 按比例迁移）；补 **`work_max_ns` fetch_max**；跨进程 stall 证明改 **freeze 单 worker**（S0 freeze topic `WorkerKind{Rt=1,SerialIo=2}` 既有〔`freeze_control.rs:74`〕、`serial_io_thread.rs:317` hook 已在）——P.2.5b 全落死 |
| R5-3 | P0 | **D14 RT→IPC 通路与状态语义不可执行**：`JointCommandBuf`（robot-core `adapter.rs:105`）不能持 robot-msg 生成 `StaleAction`（`joint.fbs:22`）；`EventQueue` 是 ipc 线程私有 FIFO（`state_machine.rs:431`）RT/SerialRT 不能直接入队；新命令 rearm / 错 layout / ClearError / Ready-Safe / Fault 输出策略全未定（转移仅受理 Enabled `:640-645`；Fault 输出实为 `Disabled` `:392` 非 round 4 宣称的 "safe hold"） | P.2.5b 锁死：**`StaleActionRepr`**（照 `ControlModeRepr` 先例 `adapter.rs:123`，ipc 解析处显式映射 + unknown→Hold 保守 + variant-count 锁测）；**`ExecutorSafetySignals` 固定原子量 + IPC monitor 读取-去抖-入队**；**安全事件锁存重试至 Fault 提交、不限频不丢弃（限频只作用于日志 ≤1/s）**；rearm 语义表（过 gate 命令写 slot 才 rearm / 错 layout 不触 slot / 仅 Enabled 受理 + seq 水位防旧命令重 Fault / Ready-Safe 不评估 / ClearError 不自动 rearm）；**Fault 输出拍沿 `Disabled`，"safe hold"撤回，§6.2 措辞偏离走 CHANGELOG** |
| R5-4 | P0 | **gateway 自探针路径错**：round 4 写 `/robot.RobotControl/GetState`，实际 package = `robot.control.v1`（`robot_control.proto:35`）→ 服务返 Unimplemented，探针等不到 InvalidArgument，10s 后启动失败 | P.2.7 锁死 `tonic::codegen::http::uri::PathAndQuery::from_static("/robot.control.v1.RobotControl/GetState")` |
| R5-5 | P0 | **freeze topic 缺 wire/生命周期契约**：S0 topic 固定 `/<robot>/test/freeze` 且 `max_subscribers(1)` + `history_size(1)`（`freeze_control.rs:201/226-227`）——同 robot_id 三进程共用则后两个 subscriber 启动失败；既有 POD `{kind, duration_ms}` 编码不了 `RrServer(u8)/RrIo(u8)`；history 会把 PID0 命令重放给 PID1 | P.2.7 锁死：**独立路径**（S1 `/<robot_id>/s1/test/freeze`、gateway `/<robot_id>/gateway/test/freeze`，S0 既有不动）；**精确 POD** `{target_pid: u32, kind: u8, index: u8, _pad: [u8; 2], duration_ms: u64}`（daemon crate cfg-gated pub 导出 + smoke import——`freeze_systemd_smoke.rs:81` 单源先例）；**非法 kind / index 越界（S1 RrServer ≥4、gateway RrIo ≥6）/ pid 不匹配 → warn + 忽略**；`target_pid` 防重放 + **AC-E19 反向步**（显式再发 PID0 定向命令断言 stale-ignored——不依赖 history 补发时机，A1 根因非确定） |
| R5-6 | P1 | **资源基线采不到三类进程**：仅 S0 起 robot-health（`s0_daemon.rs:740`）、现配置无 `[monitor]`、默认 `interested_processes` 为空（`config_loader.rs:56-59`）；且 `robot_rpc_gateway` bin 名 17 字符超 comm 15 上限，精确匹配（`robot-health lib.rs:210`）必落空 | 权威 arm config（段 2 落）追加 `[monitor] interval_secs = 1` + `interested_processes = ["s0_daemon", "s1_daemon", "robot_rpc_gatew"]`（**gateway 用截断名**）；(comm,pid) 归档 + **6 unit `systemctl show -p MainPID` join 映射左右臂**；**warmup = 丢每进程首帧（CPU 差分需两帧）、最小样本 ≥30 帧/进程**，不足延长尾窗（P.2.10 落死） |
| R5-7 | P1 | **生效正文旧状态**：D14-D17 仍标"待拍"、顶部只写 D1-D13、P.1 仍 "9 feat" 且执行序漏 (5b)、段(10) AC 列仍"视拍"（`:20/:781/:969` 锚点） | 全量同步：标题 ✅ 已拍 ×4 / preamble / 顶部状态行 / 拍板表三行 / P.1 头（10 feat + 条件段 9b + 序含 5b・9b）/ 段(10) 行 / AC-E12/E19/E24 行 |

## Plan round 6 amend 记录（reviewer 复核后；D15 能力核实完成 + D14 信号/输出语义重设计）

4 P0 + 2 P1，已回修（R5-1 四步序 / R5-4 / R5-5 / R5-6 reviewer 确认成立不重开）：

| # | 级别 | 阻断 | 修订 |
|---|---|---|---|
| R6-1 | P0 | **deadline seq/rearm 漏报**：round 5 用 0 作"无信号" sentinel，但命令 seq 合法从 0 起（`mock_s1.rs:416`）；`expired_seq > enable 水位` 判据在 "enable 时 slot=N、命令 N 随后过期" 时 N>N 恒假——每次 enable 后首条命令永不 Fault | 重设计：**独立 `deadline_expired_valid: AtomicBool`（禁 sentinel）**；受理 = valid ∧ `signal_seq == 当前 slot.seq` ∧ **该 slot 此刻仍过期（复核）** ∧ Enabled；**新有效命令写 slot 清信号**（等值判据兜 RT race）；ClearError 不清信号不 rearm（RT `last_signaled_seq` 线程局部防重复触发）；写序 seq→valid 均 Release、读序 valid→seq 均 Acquire；**四组 unit**（seq=0 / enable 后首条 / 旧信号新 slot race / ClearError 后旧命令）+ Disable-未 Fault-re-Enable 受理 case |
| R6-2 | P0 | **on_timeout 三值无可执行输出语义**：HardRT/SerialRT 对 HoldCurrent/ZeroCommand 只是重发 slot 原命令（`rt_thread.rs:396-410` / `serial_io_thread.rs:468`），StopTorque 只是停写；feetech 非 Position mode 含 Stop 静默 `Ok(())`（`adapter.rs:561`） | P.2.5b 锁死：**单一 resolve 函数、heartbeat CtrlLost 与 deadline 过渡窗口两处共用**；Hold = **进入时 latch 当前位一次**（不逐 tick 重读）以 Position 写出（现行重发 stale = 跟随过期目标，行为修正走 CHANGELOG）；Zero 逐模式（Velocity→0 速 / Torque→0 扭 / **Position→同 Hold 严禁归零位**）；Stop = **非 EStop plain 断扭**（feetech sync_write `Torque_Enable=0` 不置 latch 不触 EStop 事件；失败映射沿 AdapterErr→Fault 链）；**`OutputPolicy::Disabled` 边界登记 = 仅停写 ≠ torque-off** |
| R6-3 | P0 | **阈值不确认**：冻结字面 = "1 kHz / 10 ms"（`00_design.md:1025`）无缩放公式，50Hz/200ms 是 20 倍放宽；SerialRT 8+4 ms 是未采集目标非基线（`10_verification.md` ⏳），且同 tick 有 enable/disable/connect 控制动作（`serial_io_thread.rs:447`）——整 tick >36ms 可能是合法动作 | stall 公式改 **`max(10ms, 10×周期)`** 并**作为 design-freeze diff 走 CHANGELOG 登记 + 安全论证四点**（1 kHz 下逐字还原冻结值 / 阈值必须>周期 / 200ms ≪ systemd 2s 且 ≤ 硬件 timeout 上界 / 未来 1 kHz 部署自动回冻结字面）；SerialRT overrun 改**两相制**：shadow（只计数+直方图，effectful/emergency tick 分类排除）→ **Orin 机械隔离基线采集 ≥10 min → 阈值回填 commit（reviewer 追认）→ enforce**；HardRT overrun 即落即 enforce；stall 不分相 |
| R6-4 | P0 | **段 9b 不可执行**：寄存器表无 watchdog 项（`registers.rs:68` 区 16 项实证），地址/单位/易失性/解锁写入 readback 重锁/EEPROM 磨损全留实施期；权威 config 缺省 `auto_enable=true`（`s0_daemon.rs:955`）——只读阶段或 kill 后重启自动上扭破坏 D15 顺序门 | **① 能力核实完成（2026-07-15）：STS3215 不支持**（三锚点：dimos-deploy 表〔自证完整表在 lerobot〕/ lerobot 完整 58 项表逐一核对无该类寄存器 / 真机失联持续带扭）→ 依 D15 已拍分支**回 Spec 新拍板位 D15-R**（(b) 硬件断电 watchdog / (c2) 独立监督进程 / (c1) 受监督豁免——事实单含合规性・<500ms 可达性・AC-E25 判据・退役链影响；推荐 (b)>(c2)>(c1)）；段 9b 挂起改为 D15-R 落地段；**权威 arm config ×2 显式 `auto_enable=false`**（P.2.2）+ P.2.10 各步 operator 显式 `SetEnabled(true)` |
| R6-5 | P1 | **Cargo 传播与多 hardware 接线未闭**：S1/robot-rpc 均无 `test-freeze-hook` feature、robot-tests 无 robot-s1 依赖；gateway StatePool 是 per-hardware 一线程（`state_pool.rs:6`）——无索引 WorkerKind 冻不到指定线程，聚合 observer 会被健康线程掩蔽 | P.2.7 落三份 Cargo 精确（robot-s1/robot-rpc `[features] test-freeze-hook=[]`；robot-tests 新 optional `robot-s1`〔沿 `:148` robot-s0 先例 path+default-features=false〕+ feature 行 `sd-watchdog-smoke = ["dep:robot-s1","dep:robot-rpc","robot-rpc/test-freeze-hook"]`）；`WorkerKind::StatePool(index)` + 越界规则 + **逐 hardware 线程各一 observer（禁聚合）** + 多 hardware 单线程冻结 smoke case |
| R6-6 | P1 | **P.3 旧段序**：PD-E1 行缺 (5b)/(9b)，与 P.1 权威序冲突 | PD-E1 行同步（含 9b 挂起注） |

## Plan round 7 收缩记录（reviewer 复核后；范围收缩 = 受监督功能 demo，生产级安全闭环后置）

reviewer 裁决全文照录执行（auto_enable=false / R6-5 接线 / R6-6 序修订被接受沿用）：

| # | 项 | 裁决 | 落点 |
|---|---|---|---|
| R7-1 | **D15-R 拍 (c1+)**：受监督 demo 豁免 + fail-safe 接口预留——不采购继电器/不实现监督进程/不预选 (b)(c2)；能力核实收窄口径"无已文档化、可配置的 command watchdog"；≥10s 台架观察 = 行为证据；**D15 保持 OPEN**；AC-E25 不入 demo gate；**D-BI3 正式退役推迟**（旧 unit 可 stop 不得 disable/删除/破坏回滚）；收口宣称唯一口径"受监督功能 demo 跑通"；design:1107 显式 demo 例外走 CHANGELOG freeze-diff | ✅ 拍定 | D15/D15-R/D16 节 + 拍板表 + CHANGELOG 2026-07-15 条 + 决策单 D-BI3 修订 |
| R7-2 | **D14 同步收缩**：段(5b) 四检测器移出候选 E → post-E "生产安全硬化候选"；两 P0 前置阻断登记（CtrlLostStop 恢复 Pure 不重 Enable〔`state_machine.rs:621-636`〕/ `cmd_deadline_valid` 借道〔`:202` + `ipc_tick.rs:456` 硬编码 true〕+ 信号抑制 = Active 持续 timeout 输出→须锁"新鲜未过期命令才 rearm"）；R6-1/2/3 保留历史记录非 Execute 权威；"≤ D15 硬件 timeout 200ms"论证已删 | ✅ 执行 | D14 节 + P.2.5b 墓碑 + AC-E23/E24 归档 + 不在范围 post-E 条 |
| R7-3 | **E 保留项确认**：layout 链 / G1 / 双臂 workflow・gripper shim・e2e / handoff・preflight・latch・guard / S1・gateway sd_notify + freeze smoke / auto_enable=false / 资源只采基线 / D17 延期——执行序收敛 (1)→(2)→(3)→(4)→(5)→(6)→(8)→(7)→(9)→(10-demo) | ✅ 执行 | P.1 表（删 5b/9b 行）+ P.3 PD-E1 + P.0 活清单（撤 robot-s0 test-freeze-hook 条） |
| R7-4 | **demo 安全边界五条**（AC-E12 显式豁免执行前置）+ 抖动基线数据源改用既有 `iceoryx2_rt_baseline` bin（0 新生产代码；生产线程直方图归 post-E） | ✅ 执行 | D15-R 节 + AC-E12 行 + P.2.10 (10-demo) 重写 |
| R7-5 | **预留接口边界**：仅文档级集成契约（arm / heartbeat(kick) / request_safe(disarm) / status+readback；集成点 = handoff/guard 与 SetEnabled(true) 之间；缺失/失联/readback 失败 fail-closed）；**不写假 no-op trait，禁止凭空抽象** | ✅ 执行 | D15-R 节 + P.2.9b 墓碑 |
| R7-6 | **文档一次性 Reverse Sync**：Spec 顶部/D14-D17/D15-R/AC/P.1/P.2.5b/P.2.9b/P.2.10 + 决策单 D-BI3 + CHANGELOG freeze-diff + 后续 summary/CLAUDE 口径统一"受监督功能 demo" | ✅ 执行 | 本 commit 三文件 |

## Plan round 8 amend 记录（reviewer 复核后；收缩方向通过，修四项闭环点）

2 P0 + 2 P1，已回修（其余 round 7 收缩接受）：

| # | 级别 | 阻断 | 修订 |
|---|---|---|---|
| R8-1 | P0 | **"§6.7 五条全保"与 demo 例外直接冲突**：(c1+) 无法保证 #1 后半句"S0 不在时靠硬件 fail-safe" | 严禁改动 #2 改**拆分口径**：#2-#5 全保；#1 S0-alive 软件保证不变、S0-dead 硬件半句按 D15-R (c1+) 显式 demo 例外并保持 OPEN；CHANGELOG 偏离对象**补点名 §6.7 #1**；后续 summary/CLAUDE 禁写"§6.7 全保" |
| R8-2 | P0 | **auto_enable=false ≠ restart 后已断扭**：Feetech `connect()` 只建连/翻译 id/缓存不写 `Torque_Enable=0`（`adapter.rs:475-514` 实证）——S0 Enabled 崩溃 → 电机持续带扭 → guard 只处理 omitted → 新 S0 起后本臂 1-6 仍带扭 | per-arm guard **每次启动/restart 对本 bus 全量 ensure-safe**（左 bus1 ID 1-8 断扭；右 bus2 ID 1-6 断扭、7-9 先零速后断扭；全部 readback；仍只碰本臂 bus）——D13/AC-E16/AC-E22/P.2.9/mock unit 判据全同步；runbook 增**应急协议**（Enabled 期间任一 S0 死亡/重启 → 立即人工切总电源中止本轮；不得只依赖 EStop 弱 Ack——S0-dead 时 Ack 不证明事件已消费）；kill -9 带扭 ≥10s **移出必经步** → 机械隔离+关自动 restart 的一次性 characterization（不计 gate，已有证据可不重跑） |
| R8-3 | P1 | **生效中的旧退役指令**：D12 runbook "验收通过后才 disable 旧栈"（:546）/ P.2.9 runbook "D-BI3 步序" / 决策单 LCM 行"随仓退役" | 统一改：demo 后旧栈仍保留；只允许为释放硬件而 `stop`；正式退役 / disable / 删除 / LCM 随仓退出**全部等 post-E 生产安全候选闭环**（决策单 r4 同步） |
| R8-4 | P1 | **demo 数值与 baseline 宣称不可判定**："低速/低扭矩"无 torque-limit 配置面不可验收；`iceoryx2_rt_baseline` 测的是 synthetic 同进程 pub/sub 延迟非生产线程调度抖动 | 改"**低速/小幅**"：单臂探针 \|target−anchor\| ≤ **0.087 rad** + 轨迹斜率 ≤ **0.14 rad/s**（既有 ±5°、0.25 Hz 量级），撤"低扭矩"宣称；bin 改称 **synthetic iceoryx2 transport latency baseline**，归档 p50/p99/p999/max + drops/overruns + 完整参数 + SCHED_FIFO/mlockall/affinity 是否成功；只采样不执法、阈值失败登记不阻断 demo；生产线程 tick 用时与真实调度抖动留 post-E |

## Plan round 9 amend 记录（reviewer 复核后；round 8 四项对账全过）

1 P0 + 1 非阻断，已回修：

| # | 级别 | 阻断 | 修订 |
|---|---|---|---|
| R9-1 | P0 | **部署 preflight API 无法表达双 bus handoff**：签名块用未定义的 `Side`（全仓唯一定义在 `robot-kinematics-xlerobot/src/lib.rs:208`，robot-s0 不依赖该 crate 且不规划此依赖边）；`ensure_safe(scope, t)` 只收一个 `DeployTransport`，而 handoff 必须同时区分 bus1/bus2——**两 bus motor ID 重叠（均有 1..6），trait 只有 motor 参数无从判归属**；Execute 将被迫临时定类型/依赖/双 bus 路由，违反函数级 Plan 门禁 | **per-bus 形态重写（P.2.9）**：robot-s0 本地 `DeployArm { Left, Right }`（禁依赖 kinematics `Side`）；撤 `PreflightScope`，改 **`ensure_safe_arm(arm, bus)` + `ensure_safe_handoff(left_bus, right_bus)`**（一侧任何失败**不阻断**另一侧完整执行、末尾双侧聚合；`MotorFailure` += `arm` 归属字段 + `OpKind::Connect`；串口构造失败以 **FailedTransport** 替位进聚合，无特判分支）；**CLI 配置来源锁定**：arm 路径单份 config、handoff 显式 `--left-config` + `--right-config`（handoff unit `ExecStart` systemd 字面写死）；**反向测试两条**（双 bus 同号 motor 按 arm 区分 / bus1 全失败 → bus2 仍全部执行 → 聚合失败）——D13 ②/AC-E16/子命令表全同步 |
| R9-2 | 非阻断 | 资源门闩节 ① 首句仍"调度抖动基线"，后文才更正——摘要/grep 会误取旧口径 | 首句直接改 "CPU / RSS + synthetic iceoryx2 transport latency baseline" |

## Plan round 10 amend 记录（reviewer 复核后；R9-1 方向确认 + R9-2 闭合）

1 P0 + 2 P1，已回修：

| # | 级别 | 阻断 | 修订 |
|---|---|---|---|
| R10-1 | P0 | **systemd 配置路径与 D2 权威文件名不一致**：D2 锁 `configs/xlerobot_{left,right}_arm.toml`（身份表），handoff unit 却写 `/etc/robot/xlerobot_{left,right}.toml`（漏 `_arm`）；且无安装重命名映射 | 统一 **`/etc/robot/xlerobot_{left,right}_arm.toml`**（仓库 configs/ 同名安装，runbook 增 `install -D` 步）；**三处同一绝对路径** = handoff 双 `--config` / arm guard `--config` / guard **自动追加** `--config <GuardConfig.config_path>` 到 s0 args（`--` 后再带 --config = 配置错误退 86，单源结构性保证）；AC-E22 增 unit 字面三处一致锁 + 拒绝 case |
| R10-2 | P1 | **右 bus 写次数算错**："9 条写"实际 = ID 1-6 断扭 6 条 + ID 7-9 每轮 V+T 共 6 条 = **12 条**；handoff 合计左 8 + 右 12 = **20 条** | 计数全改准；测试**断言完整有序序列**（右 = `[T1..T6, V7,T7, V8,T8, V9,T9]`，**每轮零速先于同轮断扭**，非只断数量）——签名块注释 / AC-E16 表行 / P.2.9 unit 清单三处同步 |
| R10-3 | P1 | **FailedTransport → op=Connect 无类型契约**：`TransportErr` 无来源信息，正常实现按调用点分类 Write/Readback，"自然得到 Connect"不成立 | 采 reviewer 推荐形态：**`PreflightFailure::Connect { arm, err }` + `PreflightFailure::Operation { arm, motor, reg, op, err }`**（`OpKind` 撤 Connect 只留 Write/Readback）；`ensure_safe_handoff` 签名改收 `Result<&mut dyn DeployTransport, String>` ——构造失败**签名级显式**聚合 `Connect{arm}` 并跳过该侧写序，**Ok 侧照常完整执行 12 条写 + readback 后聚合失败**；FailedTransport 方案撤回 |

## Plan round 10 复核结论（reviewer，2026-07-15）：✅ Plan Approved — supervised-demo scope

- 未发现新增阻断；R10-1/2/3 闭环确认（路径与参数来源统一 + 重复
  `--config` fail-closed 退 86 / 左 8・右 12・合计 20 条写且右轮严格
  V→T / Connect 与 Operation 类型分离、一侧连接失败不阻断另一侧、
  FailedTransport 仅剩历史记录合法）。
- **批准边界**：① D15 硬件 fail-safe 继续 OPEN，AC-E25 不属于 demo
  gate；② 不执行旧栈正式退役 / disable / 删除，不宣称生产级或无人值守
  完成；③ 执行严格串行 (1)→(2)→(3)→(4)→(5)→(6)→(8)→(7)→(9)→(10-demo)；
  ④ 实施期任何语义偏离**先 Reverse Sync**，不直接改 Spec 或继续执行。
- 复核验证：`git diff --check` / `cargo fmt --check` / workspace
  clippy `-D warnings` 均通过；本轮全为文档改动，未重复运行 workspace
  tests。
- 下一步指令：登记本结论 → push 本地 docs commit 链 → **只进入
  Execute 段(1)**。

## P.0 工作基线

- Spec 定稿 = `3f49230`（round 12 Approved + 终拍）；CodeMap = `dbbba7b`
  （round 4 Approved）。
- toolchain：`channel = "nightly"` **浮动最新、不锁定**（2026-07-13 政策）；
  新 lint fix-forward。
- 每 commit 前 `cargo fmt --all` + `cargo clippy --workspace --all-targets
  -- -D warnings` 零警告；**Quality Gate 顺序硬性**（RS2 全链教训）=
  `cargo build --release --workspace` → **`cargo build --release -p
  robot-tests --features "pubsub-cleanup-smoke,scripted-backend-smoke,
  ik-backend-smoke,robotctl-smoke,clear-estop-e2e"`**（required-features
  bins；PC2 补 `clear-estop-e2e`）→ workspace test（1185 基线 0 退化 =
  AC-E13 每段随跑）→ 相关 smoke。**活清单条款（PC2）**：后续新增验收
  feature 与 `xlerobot-real` guard bin 落地时，**必须同步加入本显式
  release build 清单**（漏加 = RS2 家族回归）。**round 3 首批入清单**
  （各自段落地即生效）：
  `cargo build --release -p robot-s1 --bin s1_daemon --features
  test-fault-backend`（段 7 e2e 前）/ `cargo build --release -p robot-s1
  --features test-freeze-hook` + `cargo build --release -p robot-rpc
  --features test-freeze-hook`（段 8 smoke 前；feature 名对齐 S0 既有
  `test-freeze-hook` 单数，RB3）/ `cargo build --release -p robot-tests
  --features sd-watchdog-smoke`（段 8 新 smoke，bin =
  `sd_watchdog_smoke`——RB3 现在锁名，撤"落地时具名"）/ `cargo build
  --release -p robot-s0 --features xlerobot-real`（段 9/10-demo，含
  `xlerobot_deploy` bin）。〔round 7：撤 robot-s0 test-freeze-hook
  构建条——段(5b) stall case 随 D14 移出候选 E；robot-s1 / robot-rpc
  test-freeze-hook 与 sd-watchdog-smoke 两条**保留**（段 8 在 E 内）〕
- **0 新 wire**（D1-D13 终拍口径：0 新 method / 0 新 schema）；**0 新
  外部 crate**（QA3 改口：工作区**内部**依赖边允许按拍新增——已拍两条 =
  S1 / robot-rpc → robot-health〔PD-E6(c)〕；依赖边变更段随跑
  `cargo build --timings` 对照 + `cargo tree` + cargo deny；外部 crate
  确需新增 → 白名单登记 + reviewer 拍板）。
- cargo deny：未新增 violation 口径逐段核。

## P.1 commit 节奏（预估 9 feat + 1 真机 demo 段 + 逐段 docs；
**PD-E1 已拍 + round 7 收缩：全串行落 commit，顺序
(1)→(2)→(3)→(4)→(5)→(6)→(8)→(7)→(9)→(10-demo)**——先落最终 daemon
liveness〔段 8〕，再让旗舰 workflow e2e〔段 7〕覆盖它；段(5b)/(9b) 已
移出候选 E〔round 7，D14 归档 post-E 生产安全硬化候选 / (c1+) 无 9b〕）

| 段 | 内容 | 主要落点 | AC | 依赖 |
|---|---|---|---|---|
| (1) | **layout 计算权威**：`compute_layout_id` 规范编码 pub fn + schema_lint 迁移 | robot-msg | E1 | — |
| (2) | **config typed + 身份锁（PC1 原子扩）**：motor_ids typed 全套校验 + **消费端迁移同 commit**（`build_adapter` 从 typed 字段读，撤 `extras["motor_ids"]`，`s0_daemon.rs:1163`）+ **S0/S1 两套 `HardwareSection` 同步** + `[robot].id==--robot-id`（S0/S1）+ exit 码口径 + **全仓 `[[hardware]]` producer census 逐一补显式 motor_ids**（PC1 名单：case_ctx / set_enabled_e2e:264 / estop_e2e:369 / state_rpc_e2e / gateway.rs 测试 helper / 4 smoke bin 动态 builder + freeze_systemd_smoke / configs 12 TOML；struct fixture 全数） | config loader（S0+S1）+ s0/s1 args + 全仓 producer | E3 部分 | (1) |
| (3) | **S0 layout 落地**：撤 `0xCAFE…` 占位（config 派生）+ command 侧 layout gate（丢弃不写 slot + `unknown_layout_count` + 限频 warn） | robot-s0 | E2 | (1)(2) |
| (4) | **S1 layout 落地 + client 校验**：S1 config 派生 / `--layout-id` 仅一致性断言、冲突 fail-fast（PD-E5(a)）/ **mock_s1 补 `--config` 入口 + crash-smoke 等既有调用方迁移**（PC4）；robot-rpc client 模块骨架 + `GetState.layout_id` 校验 | robot-s1（含 mock_s1）+ robot-rpc client + 调用方 | E3/E4 | (3) |
| (5) | **G1 语义段**：S1 常驻 state cache（stop+join+Drop）+ `scripted_start_pose`（拒绝语义）+ 末点发布保证（先 Continue 后 Completed）+ `idle_hold_target` 穷尽规则与行为矩阵 + HoldPose 回归 | robot-s1 | E6/E7/E8/E17/E18/E20 | (4) |
| (6) | **转换 helper + shim**（round 3 非阻断改准：撤"case 库骨架"——`dual_arm_workflow` 全在段 7）：`client::trajectory` + `client::baseline_shim` 两 helper（name→index / 时长差分 / 首点宽容校验 / rad-native 校验 / 原始 12 维 shim）+ feetech 公共换算 API + robot-tests optional dep | robot-rpc client + robot-adapter-feetech（pub 纯函数）+ robot-tests | E10 | (1)(4) |
| (7) | **双臂协议 + workflow e2e**：并发提交 / 双终态 / partial / cancel / 双 EStop / GetTaskStatus 收敛编排 + **2×(s0+s1+gateway) = 6 子进程** e2e（PC3 口径；新 config 对 mock 路径全通） | robot-rpc tests | E5/E9/E11/E14/E15 | (2)-(6)(8) |
| (8) | **sd_notify 段**：S1 + gateway liveness-aware notify/watchdog（loop_tick_count / tonic loopback probe / READY / STOPPING）+ smoke | robot-s1 + robot-rpc + robot-tests | E19 | (6) 后、(7) 前（PD-E1：先落最终 daemon liveness，旗舰 e2e 覆盖它） |
| (9) | **部署件段**：guard bin（pre-increment 六步 / 86 / 稳定锁 / RuntimeDirectory 契约）+ preflight/handoff 入口（17 电机 ensure-safe，mock transport unit）+ latch provision + unit ×7 + 新 runbook | **robot-s0 `xlerobot-real` bin + robot-s0 lib 共享实现（PD-E2(a) 已拍）**+ scripts/systemd | E16/E21/E22（unit 侧） | (7) |
| (10-demo) | **真机受监督 demo 段**（非 repo commit 为主；**D15-R (c1+) 显式豁免 + 安全边界五条**）：Orin 本地 build → handoff transaction → 双栈起（auto_enable=false 不自动上扭；旧 unit 可 `stop` 不 disable 不删）→ 读状态（只读）→ 安全边界 checklist → 单臂低速/小幅（|target−anchor| ≤ 0.087 rad、斜率 ≤ 0.14 rad/s，显式 SetEnabled(true)）→ 双臂 chunk → 双夹爪（**AC-E12**）+ AC-E16/E21/E22 真机侧 → **资源基线采集**（CPU/RSS + synthetic transport latency baseline，细则 P.2.10）→ 证据 docs；**应急协议 = S0 死亡立即断电中止**；kill -9 characterization 移出必经步（可选一次性，不计 gate）。**无 D-BI3 退役步（正式退役推迟 post-E）；收口宣称 = "受监督功能 demo 跑通"** | Orin + docs | E12/E16/E21/E22（真机侧） | (1)-(9) |
| 逐段 docs | CHANGELOG 登记分布（PD-E4）+ 段间 summary 增量 + 收口 summary/CLAUDE/README | docs | E13 全程 | 随段 |

## P.2 工作流（函数级，round 1 后展开）

通则：每段 = 独立 feat commit（同段 ≤2）；段内 unit/e2e 同 commit；跨段禁
半成品；**CHANGELOG 随语义所在段登记（PD-E4(a)）**：段 3〔JointState.
layout_id 值语义 + §3.5.1 部分落地边界〕/ 段 5〔B1 等价解锁：Idle hold
target 源〕/ 段 6〔feetech 公共 API + robot-tests optional dep 白名
单〕/ 段 7〔gripper 阶跃→插值语义变化〕/ 段 8〔B1 决策 #7 解锁〕/
段 9〔86 永久保留 + guard 机制 + RuntimeDirectory 契约〕/ **段
(10-demo)〔demo 执行证据补录；监督 demo freeze-diff 本体已于
2026-07-15 随 round 7 docs 预登记〕**。〔round 7：原段 5b / 9b 两条
登记项随段移出候选 E——post-E 硬化候选落地时再立〕

### P.2.1 段(1) layout 计算权威（robot-msg）

- 新 `crates/robot-msg/src/joint_layout.rs`（lib pub）：
  `pub fn compute_layout_id(robot_id: &str, hardware_id: &str,
  joints: &[(&str, u32)]) -> u64`——编码严格按 D1.1（`lp("robot-layout-v1")
  ‖ lp(robot_id) ‖ lp(hardware_id) ‖ u32_le(count) ‖ ∀ lp(name_i) ‖
  u32_le(id_i)`，`u64::from_le_bytes(blake3[0..8])`）。
- blake3 依赖位核实：schema_lint（robot-msg bin）已用 blake3——若现为
  bin-only 依赖则提升为 lib 依赖（同 crate 内移动，白名单登记一句）。
- `schema_lint.rs:190` 私有 helper 删除、改调新 fn；**lint 调用面适配
  （round 2 非阻断更正）**：lint 用**硬编码 fixture**——直接给 fixture 补
  (name,id) 对即可，与段 2 config census 无关。
- unit（AC-E1，`joint_layout.rs` tests）：`deterministic` /
  `order_sensitive`（交换两 joint → 不同值）/ `id_sensitive` /
  `name_boundary_injective`（`("ab",1),("c",2)` vs `("a",1),("bc",2)`
  不同值——长度前缀单射）/ `golden_vector`（round 3 非阻断改准——"改常量"
  不可执行：**固定输入 → 固定 u64 快照**锁编码稳定性，域/版本变更必然
  打破快照）/ `lint_same_source`（lint 路径与直调同值）。

### P.2.2 段(2) config typed + 身份锁 + 全仓 census（PC1 原子）

- **S0** `config_loader.rs` `HardwareSection` += `pub motor_ids: Vec<u8>`
  （**必填**，无 `#[serde(default)]`——缺失 = parse 错误，走 S0 exit 2）；
  resolve 校验：`len(motor_ids)==len(joints)` / 两者非空 / joints 名唯一 /
  motor_ids 唯一（u8 域由类型承担）。**S1** 侧 `HardwareSection`（
  **`coordinator.rs:301` 本地定义**，round 2 非阻断更正）同步 += 同校验
  （违者 S1 exit 3——config 内容错误族）。
- **消费端同 commit 迁移**：`s0_daemon.rs:1163` `build_adapter` 撤
  `extras["motor_ids"]` `filter_map` → 读 typed `hw.motor_ids`。
- **身份锁**：S0 `Args::resolve` + S1 `Args::parse` 后：`--robot-id` 与
  TOML `[robot].id` 同给且不等 → exit 2（沿 gateway 先例文案）。
- **census 逐一补显式 `motor_ids = [1..=n]`**（PC1 名单）：
  `tests/common/case_ctx.rs:306` 模板 / `set_enabled_e2e.rs:264` /
  `estop_e2e.rs:369` / `state_rpc_e2e.rs` builder / `gateway.rs` 测试
  helper / `crash_restart_smoke.rs`・`freeze_systemd_smoke.rs`・
  `scripted_backend_smoke.rs`・`pubsub_cleanup_smoke.rs` 动态 builder /
  configs 12 TOML / S0・S1 config_loader 测试 fixture 全数；**消费定义面
  核对**（gateway `--config` minimal 解析、`state_pool.rs`、
  `coordinator.rs`、`factory.rs`——只读 id/joints 的面确认不受必填新字段
  影响：若共用完整 struct 反序列化则同步）。
- **权威 arm config ×2 显式 `auto_enable = false`（R6-4）**：缺省 =
  true（`s0_daemon.rs:955` 区实证，候选 C D6 保既有行为）——不显式则
  只读验收阶段或 kill -9 后 systemd 重启会**自动上扭**，直接破坏 D15
  顺序门；上扭动作全部改由 operator 显式 `SetEnabled(true)`（P.2.10
  步序注明时点）。
- unit：loader 校验 6 拒路径 + 身份锁 2 路径（S0/S1 各自 error 类型断言；
  exit 码 bin 层由既有 smoke/e2e 间接锁 + AC-E3 unit 断言 error→码映射
  函数）。

### P.2.3 段(3) S0 layout 落地 + command gate

- `s0_daemon.rs`：`let authoritative = compute_layout_id(&robot_id,
  &hw.id, zip(joints, motor_ids))`；`pub_cfg.joint_layout_id =
  authoritative`（撤 `:858` 占位）；authoritative 传入 ipc_thread cfg。
- `ipc_thread.rs`：`parse_joint_command` 调用点**之前**加
  `if fb.layout_id() != cfg.authoritative_layout {
  metrics.unknown_layout_count.fetch_add(1, Ordering::Relaxed)
  〔AtomicU64，round 2 非阻断明确〕; 限频 warn（沿仓内 every-N 模式）;
  continue（不写 LatestCommandSlot）}`。
- unit：`command_layout_mismatch_dropped_not_written`（构造 mismatch fb
  bytes → slot 不变 + **`unknown_layout_count` 恰 +1**——计数断言**仅在
  本 unit**，R3-B4）/ `command_layout_match_written`；cross-process
  AC-E2 = 段(7) 的**独立 mini 场景**（raw 注入器，见 P.2.8——R3-B4 删
  "S1 侧注入"残句）。

### P.2.4 段(4) S1 layout + mock_s1 `--config` + client 校验

- `s1_daemon.rs`：撤 `:133` 占位默认；layout_id = config 派生（同 fn）；
  显式 `--layout-id` ≠ 派生 → exit 2（参数冲突族）；help 文本同步。
- **mock_s1 补 `--config`（PC4）**：必填化（joints 数从 config 派生可与
  `--joints` 一致性断言）；调用方迁移 census：`crash_restart_smoke.rs` +
  runbook 档案不改（pre-strict banner）+ 其余 grep `mock_s1` 命中逐一。
- robot-rpc client：`src/client/layout.rs`（QA5 typed 化）：
  `pub struct HardwareLayout { pub joints: Vec<String>, pub motor_ids:
  Vec<u8>, pub layout_id: u64 }` +
  `pub fn load_hardware_layout(config_path: &Path, hardware_id: &str)
  -> Result<HardwareLayout, LayoutError>`（`LayoutError` typed：读失败 /
  parse / 无该 hardware / 校验失败）+
  `pub fn validate_state_layout(expected: &HardwareLayout,
  state: &RobotState) -> Result<(), LayoutMismatch>`；task_rpc_e2e /
  state_rpc_e2e harness stamping 改派生值。
- unit + e2e：S1 fail-fast case（bin 层走既有 e2e 启动失败断言模式）；
  AC-E4 = state_rpc_e2e 增 `layout_validate_mismatch_hard_fail` case。

### P.2.5 段(5) G1 语义段（robot-s1）

- 新 `state_cache.rs`（R3-B3 契约锁死，模板 = `StatePool::spawn` 的
  `io::Result` 先例，**禁 mem::forget**）：
  ```rust
  pub struct StateCacheConfig { pub topic: String, pub expected_joints:
      usize, pub expected_layout: u64, pub max_stale_ms: u64,
      pub poll_interval: Duration }
  pub enum StateCacheSpawnError { NodeCreate(TransportError),
      SubscriberOpen(TransportError), ThreadSpawn(std::io::Error),
      ReadyTimeout }
  pub fn spawn_state_cache_thread<C: RobotClock + 'static>(
      cfg: StateCacheConfig, clock: Arc<C>,
  ) -> Result<StateCacheHandle, StateCacheSpawnError>
  ```
  ——node + subscriber **线程内创建**（iceoryx2 `!Send`）；`sync_channel`
  **同步 ready 握手**（线程内建成后回 Ok；建失败回 Err；握手超时 →
  `ReadyTimeout`，两失败路径均 **stop + join 回滚**后返 Err）；
  `StateCacheHandle`：`latest_fresh(now) -> Result<Vec<f64>,
  StaleOrMissing>`（校验 = joint_count + layout〔D1 联动〕+ 新鲜度）/
  `stop()`（Release flag）/ `join()`（一次性，消费 handle）/ `Drop` =
  stop + join best-effort。**s1_daemon 在 cache spawn 成功之后才打
  ready marker**（失败 = 启动失败 fail-fast，不降级）。锚定校验逻辑与
  `read_anchor_from_s0_state` 共享私有 fn 抽取。〔AC-E20〕
- `coordinator.rs`：持 `Option<StateCacheHandle>`；`submit` Scripted 路径
  取 `scripted_start_pose = cache.latest_fresh()`，失败 →
  `ActionStartError::Rejected`（既有 → FailedPrecondition 映射）**不
  fallback**；`ActionContext` += `scripted_start_pose: Option<&[f64]>`
  （HoldPose 仍读 `anchor_bias`，`hold_pose.rs` 0 改）。
- `scripted.rs`：`:186` initial_pose 改取 `ctx.scripted_start_pose`
  （Scripted 必需，缺 → start 拒）；**末点发布保证**：
  `advance_waypoint_and_emit` 末段路径改两拍——先返
  `Continue{target=末 waypoint}`（`FinalEmitted` phase），下 tick 返
  `Completed(Success)`；dwell>0 路径在首个 dwell 帧置 `final_published`
  同一保证。
- **终态数据通路（QA2 拍死）**：`ActionInstance` trait +=
  `fn successful_hold_target(&self) -> Option<Vec<f64>> { None }`
  （默认 None，既有 backend 0 改）；**仅** `ScriptedInstance` 在
  `final_published == true` 时返 `Some(末 waypoint target)`。
  coordinator 终态处（`tick_action` Completed 分支）：
  `outcome == Success` 时取 `instance.successful_hold_target()`——
  `Some(t)` → `idle_hold_target = t`；`None` 或非 Success →
  `idle_hold_target = anchor_bias.to_vec()`（**穷尽式单一规则由类型
  承担**：其他 backend / 终态天然 reset，无需另存 kind）。
- `run_action_tick_loop`：`InnerState` += `idle_hold_target: Vec<f64>`
  （init = anchor_bias）；Idle 分支 `build_hold_command` bias 参数 →
  `idle_hold_target`；Degraded 分支保 anchor_bias 不动。
- unit：AC-E17（dwell=0 序列：Continue(末位) 后 Completed）/ AC-E8
  （HoldPose target == anchor_bias 回归）/ 矩阵单机维（每终态 →
  idle_hold_target 断言）；跨进程 AC-E6/E7/E18 落段(7)。

### P.2.5b 段(5b)〔round 7 移出候选 E——归档 post-E 生产安全硬化候选〕

- deadline / `on_timeout` / stall / overrun 四件检测器重写**移出候选 E**
  （reviewer round 7：不属于受监督功能 demo 最短闭环）。R6-1 原子协议 /
  R6-2 输出语义 / R6-3 阈值研究**保留在 round 5/6 amend 历史记录，非本
  候选 Execute 权威**；原 "≤ D15 硬件 timeout 典型 200ms" 论证随 D15
  能力核实结论失效**已删**。
- **post-E 前置阻断两 P0**（CtrlLostStop 恢复 Pure 不重 Enable /
  `cmd_deadline_valid` 借道 + 信号抑制 = Active 持续 timeout 输出——
  须锁"新鲜、未过期命令才重新武装并允许 Ready→Active"）**登记于
  D14 节**，硬化候选开工前必须先解。
- 生产线程 tick 用时直方图（RB6 数据源）随本段移出；demo 改用既有
  `iceoryx2_rt_baseline` bin 作 **synthetic iceoryx2 transport
  latency baseline**（宣称收窄——非生产线程调度抖动，见 P.2.10）。

### P.2.6 段(6) 转换 helper + shim（round 3 非阻断：撤"case 库骨架"）

- feetech：`raw_to_rad` / `rad_to_raw`（`adapter.rs:92-103`）提升
  `pub`（挪 `convert.rs` 模块，adapter 内部改调——公式单源，无 feature
  gate 纯函数）。
- robot-rpc `src/client/trajectory.rs`：
  `to_scripted_waypoints(traj: &TrajectoryInput, layout:
  &HardwareLayout, current: &[f64]) -> Result<Vec<Waypoint>, ConvertError>`
  （name→index / 时长差分→`segment_duration_ms` / 首点宽容校验〔≈current
  容差 AC 定值，超差 reject〕/ NaN・Inf・长度 reject / 缺位填 current
  〔D8〕）。
- `src/client/baseline_shim.rs`（QA5 typed 化——**左右夹爪标定范围不同，
  双 range 必填**）：
  `pub struct Shim12 { pub values: [f64; 12], pub clamp_count: u32 }` +
  `pub fn raw12_to_rad(raw: &[f64; 12], left_gripper_rad: (f64, f64),
  right_gripper_rad: (f64, f64)) -> Result<Shim12, ShimError>`
  （`ShimError` typed：NaN/Inf / range 反转〔lo >= hi〕；[0,100] clamp
  镜像 dimos 并计入 `clamp_count`；idx 5 用 left、idx 11 用 right）。
- **`dual_arm_workflow` 不在本段**（QA5：跨段禁半成品——完整模块原子放
  段(7)，本段 0 stub）。
- robot-tests `Cargo.toml`：`robot-adapter-feetech = { workspace = true,
  optional = true }` 挂 `real-hw`（timings + deny 逐段核）。
- unit：AC-E10 全路径（差分/首点/reject 族/shim 镜像换算 + clamp 计数 +
  range 反转 fail-fast——反转在标定派生处，段 9 真机侧再锁）。

### P.2.7 段(8) sd_notify 段（S1 + gateway）

- **PD-E6 已拍 (c)（round 2，reviewer 更正前提后拍板）**：
  **`robot-health::systemd` 单一实现**——S0 `systemd_notify.rs` 实现
  迁入 robot-health（其定位明文"非 RT、robot-rpc 等 daemon 可复用"，
  S0 已依赖、无循环；tracing 依赖在 robot-health 合法），S0 改调；
  S1 与 robot-rpc 各加一条 **workspace 内部依赖边**（P.0 口径 = 0 新
  外部 crate；本段随跑 `--timings` + `cargo tree` + deny）。(a) robot-
  core 上移**不选**（systemd_notify 用 tracing 非纯 std，破坏零依赖
  边界）；(b) 各自副本**不选**（违单源）。
- **robot-health::systemd 公共 API（R3-B2 签名锁死；线程名撤
  `"s0_systemd_wd"` 硬编码改参数）**：
  ```rust
  pub struct WatchdogConfig { pub thread_name: String,
      pub startup_grace_ping: bool /* 沿 S0 首周期无条件喂 */ }
  pub struct LivenessObserver { pub label: &'static str,
      pub tick: Box<dyn Fn() -> u64 + Send> }
  pub fn notify_ready();        // READY=1（NOTIFY_SOCKET 未设 = no-op）
  pub fn notify_stopping();     // STOPPING=1
  pub fn spawn_watchdog(observers: Vec<LivenessObserver>,
      cfg: WatchdogConfig) -> Option<WatchdogHandle>  // None=非 systemd
  impl WatchdogHandle { pub fn stop(self) /* 停喂+join */ }
  ```
  S0 迁移传 `thread_name = "s0_systemd_wd"` 原值——**行为 bit 等价**；
  S1 = `"s1_systemd_wd"`、gateway = `"gw_systemd_wd"`。
- **gateway ready 顺序 + 并发拓扑锁死（R3-B2 + RB4）**：
  `serve_with_incoming_shutdown(...).await` 会阻塞到停机
  （`gateway.rs:1781` 实证）——**server 必须 `tokio::spawn` 为独立
  task**，不能线性 "serve → probe"。锁定：
  1. workers ready → bind（**既有测试用 bind-ready `ready_tx` 原样
     保留**，bind 后即回，测试语义 0 改；systemd-ready 为独立通路）；
  2. `let server = tokio::spawn(serve_with_incoming_shutdown(incoming,
     shutdown_rx))`；
  3. **probe task**：低层 unary 循环（间隔 500ms，首成上限 10s——超时 =
     启动失败按既有启动错误路径退出）→ 首次成功 → `notify_ready()` →
     `spawn_watchdog`（enforce 开始）；
  4. **probe 实现（RB4——`RobotControlClient` 仅 `client` feature 生成，
     `build.rs:26`，生产不开）**：`tonic::client::Grpc<Channel>` +
     **既有 prost message 类型**（server-only build 亦生成）+ 显式
     `tonic::codegen::http::uri::PathAndQuery::from_static(
     "/robot.control.v1.RobotControl/GetState")`（**R5-4 修正：package
     = `robot.control.v1`，`robot_control.proto:35` 实证——round 4 漏
     package 前缀会得 Unimplemented、READY 永达不到、10s 后启动失败**）
     + tonic-prost codec——**不开 `client` feature、不改生产 client
     边界**；请求 = `GetStateRequest { hardware_id: "" }`，判成功 =
     收到 `InvalidArgument`；
  5. **早退错误**：server task 在 READY 前 Err → 触发 shutdown + 启动
     失败退出（沿既有码）；READY 后 Err → `notify_stopping()` → 停
     probe/watchdog → join 全部 → 按既有错误退出；
  6. **shutdown 正常序（所有权 = main async fn 持 server JoinHandle +
     probe/watchdog handle + shutdown_tx）**：`notify_stopping()` →
     停 probe/watchdog → `shutdown_tx` 触发 server graceful → join
     server task → 既有 rr_io / estop_pub / state_pool 停链原样。
- **test-only freeze hooks（R3-B2 + RB3 重写——env 方案撤回**：启动时
  读取会把 TonicProbe 冻在首次 probe 前〔测到启动失败非 watchdog 超
  时〕，且 env 跨 systemd 重启存续 → PID1 再冻结**）**：沿 **S0
  `freeze_control.rs` 先例 = test-only iceoryx2 freeze topic 运行时
  注入**（raw API、与生产 schema/拓扑隔离，cfg 守门同款）。**wire /
  生命周期契约（R5-5 锁死）**：
  - **独立 topic 路径**：S1 = `/<robot_id>/s1/test/freeze`、gateway =
    `/<robot_id>/gateway/test/freeze`（S0 既有 `/<robot_id>/test/
    freeze` 不动，`freeze_control.rs:201`）——S0 QoS
    `max_subscribers(1)`（`:226`）下同 robot_id 三进程共用会使后两个
    subscriber 启动失败，**必须各持独立 service**；QoS 沿 S0 字面
    （1 pub / 1 sub / history 1 / buffer 2 / safe_overflow）。
  - **精确 POD**（既有 `{kind, duration_ms}` 编码不了带槽位 worker）：
    `#[repr(C)] TestFreezeCommandV2 { target_pid: u32, kind: u8,
    index: u8, _pad: [u8; 2], duration_ms: u64 }`；S1 / gateway 各自
    daemon crate cfg-gated **pub 导出、smoke driver import**——单源沿
    `freeze_systemd_smoke.rs:81` import 先例，不重定义。
  - `pub enum WorkerKind`（S1：`Tick / HeartbeatPub / RrServer(index) /
    StateCache`；gateway：`RrIo(index) / EstopPub /
    **StatePool(index)** / TonicProbe`——**R6-5：StatePool 是
    per-hardware 一线程**〔`state_pool.rs:6` 实证〕，无索引单变体会
    冻不到指定线程）→ 路由到各 worker `freeze_check` 原子（loop 顶
    检查 park）。**非法值行为**：kind 未知 / index 越界（S1
    RrServer ≥4、gateway RrIo ≥6、**gateway StatePool ≥ 配置
    hardware 数**）/ `target_pid != std::process::id()` →
    **warn 日志 + 忽略，不 freeze 不 panic**。
  - **history 重放防护**：`history_size(1)` 会把 PID0 的命令重放给
    重启后 PID1 的 subscriber——`target_pid` 精确匹配使 PID1 忽略并
    打 "stale freeze ignored (pid mismatch)" 日志。
  **Cargo feature 传播三份精确（R6-5——现状：S1/robot-rpc 均无
  `test-freeze-hook` feature，robot-tests 无 robot-s1 依赖）**：
  1. `crates/robot-s1/Cargo.toml`：`[features]` +=
     `test-freeze-hook = []`（cfg 守门 freeze_control 模块 + POD/
     WorkerKind pub 导出）；
  2. `crates/robot-rpc/Cargo.toml`：`[features]` +=
     `test-freeze-hook = []`（同上）；
  3. `crates/robot-tests/Cargo.toml`：新 optional dep
     `robot-s1 = { path = "../robot-s1", optional = true,
     default-features = false, features = ["test-freeze-hook"] }`
     （**沿 `:148` robot-s0 freeze-systemd-smoke 先例**——path +
     default-features=false 不走 workspace 继承，理由同款注释）；
     既有 `robot-rpc` optional dep（`:168`，带 client）**声明行不
     动**；feature 行 = `sd-watchdog-smoke = ["dep:robot-s1",
     "dep:robot-rpc", "robot-rpc/test-freeze-hook"]`（robot-rpc 的
     test-freeze-hook 仅在本 feature 启用，不污染 clear-estop-e2e
     等其他 feature 的构建面；与 client feature 并存 additive
     无害）+ `[[bin]] sd_watchdog_smoke` `required-features =
     ["sd-watchdog-smoke"]`。
  feature 名**对齐 S0 既有 `test-freeze-hook`（单数）**，default-off
  永不进 default。**smoke 时序锁死（AC-E19）**：PID0 达 READY → smoke
  注入一次 freeze（指定 WorkerKind + PID0 pid）→ 观察 withholding
  ping → systemd watchdog 重启 → **PID1 无再次注入且恢复 READY**；
  **反向步（R5-5）**：PID1 READY 后 driver **显式再发**一条
  `target_pid` = PID0（已死 pid）的命令 → 断言 PID1 打 stale-ignored
  日志且无二次 withholding（不依赖 history 补发时机——A1 根因：补发仅
  发生在 publish/update_connections，时机非确定）。新 smoke **现在
  锁名**：bin = `sd_watchdog_smoke`、feature = `sd-watchdog-smoke`
  （robot-tests，required-features，P.0 活清单已具名）。
- S1 observers：tick loop / heartbeat pub / 4 rr server / state cache
  （段 5）——各 `loop_tick_count`（空闲推进）；gateway observers：6
  rr_io + estop_pub + **state pool 逐 hardware 线程各一 observer
  （label 带 hardware_id；R6-5：禁聚合求和——健康线程的计数推进会
  掩蔽卡死线程）** + **tonic
  loopback probe（QA4 改准）**：本地 `GetState(GetStateRequest {
  hardware_id: "" })` 期望 **`InvalidArgument`** 计成功——经 tonic
  transport/handler 但**不依赖 S0/S1/cache 首帧**（原
  `GetTaskStatus(unknown)` 会真 forward S1 RR 等 ≤5s，S1 重启即拖停
  gateway watchdog，违反 Wants= 语义，**撤回**）；`READY=1` 全
  observer ready 后；shutdown `STOPPING=1` 先行。
- 新 smoke（robot-tests bin + feature，**入 P.0 活清单**——PC2 条款首次
  触发）：冻结每类 worker → 重启；空闲 ≥ 数个 WatchdogSec 不重启
  （AC-E19 双向）；**多 hardware 单线程冻结 case（R6-5）**：gateway
  双 hardware config，冻 `StatePool(1)` 一条线程 → 断言 withholding
  →重启（`StatePool(0)` 健康推进不得掩蔽）。

### P.2.8 段(7) 双臂协议 + workflow e2e

- **`src/client/dual_arm_workflow.rs` 完整模块本段原子落**（QA5：段 6
  0 stub）：类型 + `submit_pair` / `await_pair_terminal` /
  `cancel_surviving` / 错误域 + 实现——并发 join 提交 / Ack 偏差测量 /
  终态精确集 / partial-failure `CancelTask` / 不收敛双 `EStop` /
  `GetTaskStatus` 收敛（超时上限 AC 定值）/ 双拒 FAIL。
- **failure 触发器（QA6 + R3-B1 锁死）**：Scripted 运行期只
  Continue/Completed、IK 错误全在 start 阶段——执行期 failure /
  partial-failure **无生产触发器**。新增 **`FaultInjectBackend`**
  （robot-s1 feature `test-fault-backend` 守门，**永不进 default
  features**；inventory 注册同款链）：
  - **分发键 = 既有 wire 键 0 新增**：`name = "fault_inject"`、
    `supported_kinds = [ActionKind::DwellMs]`、`supported_schemas =
    [NO_PAYLOAD_SCHEMA_ID]`；
  - **冲突消除**：e2e config `[s1]` **test profile 启用 `fault_inject`
    并显式移除 `mock_dwell`**（DwellMs 分发唯一化——dispatch 按
    enabled 列表 × (kind, schema)，`mod.rs:162`）；
  - **固定行为**：`fail_after_ticks = 3`；失败 =
    `Err(ActionFailure::InternalError("fault_inject"))`（实证枚举域
    `action.rs:442-453` 内既有变体，0 新 variant）；
  - **release-first**：e2e 前显式 `cargo build --release -p robot-s1
    --bin s1_daemon --features test-fault-backend`（P.0 活清单）。
  "Reverse Sync 降 AC 验证层级"备选**未采**。
- **raw `JointCommand` 注入器（QA6 + R3-B4 场景锁死）**：S1 错 layout
  会启动期 fail-fast，AC-E2 跨进程判据 = **独立 mini 场景**（不进六
  进程拓扑——正常 S1 持续发合法命令会经 latest() 塌缩造成假 PASS）：
  **不起对应 S1**，拓扑 = s0 + FakeHeartbeatPublisher（维持 Enabled）
  + raw 注入器（robot-rpc tests helper 直建 iceoryx2 publisher 向 S0
  command topic 发帧）；**步骤（RB5 补正向对照）**：① 确认
  `Enabled.Active` → ② 注入 **matching-layout** 命令（明显 target）→
  **断言输出确实变化**（正向控制：注入器与判据通路先自证有效）→
  ③ 注入 **≥3 帧错 layout、target = 前值 + 明显偏移** → settle →
  断言**保持 ② 后的值不变**。`unknown_layout_count +1` 断言**仅在
  段(3) unit**（"计数可观测时断言"模糊口径**删除**）。
- `tests/common/dual_ctx.rs`：PD-E3 全套（`dimos_e2e_<pid>_<seq>_{left,
  right}` / `left_arm`・`right_arm` / `127.0.0.1:0` ×2 断言不同 /
  GLOBAL_LOCK / RAII 清理）；bring-up = 2×(s0+s1+gateway) 6 子进程
  （readiness 沿 marker→TCP→RPC probe 三段式 ×2）。
- `tests/dimos_workflow_e2e.rs` case 集（AC 映射）：
  `read_state_both_arms_layout_validated`（E4/E11-M1M2）/
  `chunk_pair_scripted_with_gripper`（E11-M3M4 + E15 终点一致+单调）/
  `consecutive_chunks_no_pullback_no_jump`（E7）/
  `post_success_reset_matrix`（E18；RB5 改准：**各分支独立 case 上下文**
  ——`InternalError` 会把 coordinator 打进 **Degraded**〔`coordinator.rs:
  1746-1754` 实证〕，同上下文串测会让后续 IK/HoldPose 被拒；failure
  分支自身断言链 = fault 注入 → Failed → **Degraded hold == anchor** →
  maintainer `ClearError` → 回 Idle〔顺带锁 Degraded 矩阵行〕；其余分支
  各自 fresh bring-up 后"先 Success 再 X"）/
  `start_pose_stale_rejected`（E6；QA6 改准：**SIGSTOP 对应臂真 S0
  进程 + 等 `max_stale_ms` 超时** → Scripted start 拒
  FailedPrecondition → SIGCONT 恢复——六进程拓扑无 FakeState 可停）/
  `pair_partial_failure_paths`（E14 全路径；执行期失败注入 =
  `test-fault-backend`）/ `nontarget_fill_current`（E9 容差）/
  `s0_drops_wrong_layout_commands`（E2 跨进程；**raw 注入器**直发错
  layout 帧，QA6 改准——S1 配错序会启动期 fail-fast 故不可用）。
- watchdog 语义已由段(8) 落 → 6 子进程按 notify 形态起（e2e 无 systemd，
  `NOTIFY_SOCKET` 未设 no-op 路径沿 S0 先例）。

### P.2.9 段(9) 部署件段（PD-E2(a) 宿主）

- robot-s0 lib 新模块（`xlerobot-real` feature 域内；**R3-B5 签名锁死
  ——safety-critical 启动链不留 Execute 自行设计**）：
  ```rust
  // deploy/types.rs（R9-1：robot-s0 本地定义，禁止依赖
  // robot-kinematics-xlerobot::Side——全仓唯一 Side 在该 crate
  // lib.rs:208，robot-s0 不依赖（Cargo.toml 实证）且不得新增该依赖边）
  #[derive(Debug, Copy, Clone, PartialEq, Eq)]
  pub enum DeployArm { Left, Right }
  // deploy/guard.rs（arm 路径 = 单份 config）
  pub struct GuardConfig { pub arm: DeployArm, pub config_path: PathBuf,
      pub s0_bin: PathBuf /* 必须 is_absolute() + exists()，违者退 86 */,
      pub runtime_dir: PathBuf /* RuntimeDirectory 注入 */,
      pub robot_id: String }
  pub enum GuardExit { ExecFailed /* exec 返回即失败：ENOENT/EACCES
      配置级，不可自动恢复 → 86 */, BudgetExhausted, CounterIo,
      LockConflict, PreStepFailed(u8 /* 普通非零码 */) }
  pub fn run_guard(cfg: &GuardConfig) -> !  // 成功 = exec 不返回；
      // 失败按 GuardExit → process::exit(86 或普通码) 映射表
  // 计数/锁 API（deploy/counter.rs）
  pub fn read_count(path: &Path) -> Result<u8, CountError>
      // NotFound = Ok(0)；严格解析 0..=5；其余 → CountError
  pub fn write_count_atomic(path: &Path, n: u8) -> Result<(), CountError>
      // tmp + rename，只动 .count
  pub fn acquire_instance_lock(lock_path: &Path)
      -> Result<InstanceLock, LockError>  // flock 稳定 inode；
      // InstanceLock: exec 前显式 close/CLOEXEC
  // deploy/preflight.rs（R9-1 per-bus 形态重写；撤 PreflightScope——
  // 单 transport 的 ensure_safe(scope, t) 无法表达双 bus handoff：
  // 两 bus motor ID 重叠（均有 1..6），trait 只有 motor 参数无从区分）
  pub trait DeployTransport { fn write_register(&mut self, motor: u8,
      reg: &'static str, value: i32) -> Result<(), TransportErr>;
      fn read_register(&mut self, motor: u8, reg: &'static str)
      -> Result<i32, TransportErr>; }  // feetech 真实现 + mock 测试实现
  pub enum OpKind { Write, Readback }  // R10-3：撤 Connect variant——
      // TransportErr 无来源信息，调用点只会按位置分类 Write/Readback，
      // round 9 "FailedTransport → op=Connect 自然归因"不成立，撤回
  pub enum PreflightFailure {  // R10-3 采 reviewer 推荐形态
      Connect { arm: DeployArm, err: String },
      Operation { arm: DeployArm /* 双 bus 同号 motor 归属 */,
          motor: u8, reg: &'static str, op: OpKind, err: String },
  }
  pub struct EnsureSafeReport { pub failures: Vec<PreflightFailure> }
  pub fn ensure_safe_arm(arm: DeployArm,
      bus: &mut dyn DeployTransport) -> Result<(), EnsureSafeReport>
      // 本 bus 全量**有序**写序（round 8 P0-2 计划表；R10-2 精确计数）
      // Left  = bus1 [T1..T6, T7, T8]                 —— 8 条写
      // Right = bus2 [T1..T6, V7,T7, V8,T8, V9,T9]    —— 12 条写
      // （T = Torque_Enable=0，V = Goal_Velocity=0；逐轮 V 先于同轮 T）
      // → 逐项 readback → 单项失败继续（记 Operation{op}）→ 末尾聚合
  pub fn ensure_safe_handoff(
      left_bus: Result<&mut dyn DeployTransport, String /* 构造错 */>,
      right_bus: Result<&mut dyn DeployTransport, String>,
  ) -> Result<(), EnsureSafeReport>
      // R10-3：串口构造失败在**签名上显式**——Err 侧直接聚合
      // PreflightFailure::Connect { arm, err }、跳过该侧写序；Ok 侧
      // 照常完整执行（handoff 合计 = 左 8 + 右 12 = 20 条写 + 各自
      // readback）。一侧 Connect 失败不阻断另一侧；任一侧有 failure
      // → 整体 Err
  // deploy/provision.rs
  pub enum ProvisionOutcome { WroteBoth, RepairedMissing, Untouched }
  pub fn provision_latch(latch_path: &Path)
      -> Result<ProvisionOutcome, ProvisionError>
      // 三分支 + 统一最终步骤 parent-dir open+fsync 严格版；
      // 复用 estop_latch::save_single_atomic（私有件抽 pub(crate)）
  ```
- robot-s0 新 bin `xlerobot_deploy.rs`（`required-features =
  ["xlerobot-real"]`，**入 P.0 活清单**）：子命令（R9-1 CLI 配置来源
  锁定）
  `guard --arm left|right --config <单份 arm config> --s0-bin
  <绝对路径> -- <s0 args>`（六步后 `exec <s0-bin>`；内部 arm
  preflight 从这份 config 解析本臂串口）/
  `preflight-arm --arm left|right --config <单份 arm config>` /
  **`preflight-handoff --left-config <path> --right-config <path>`**
  （双份显式传入，各自解析对应 bus 串口 → 构造两 transport →
  `ensure_safe_handoff`；**构造失败侧以 `Err(构造错误)` 传入——签名级
  显式聚合 `Connect{arm}`，R10-3**）/
  `provision`。**unit 接线（QA1 改准——Y1 终拍：guard 必须是主进程，
  86 才被 `RestartPreventExitStatus=86` 捕获）**：arm S0 unit **无
  ExecStartPre**，`ExecStart=xlerobot_deploy guard --arm left
  --config /etc/robot/xlerobot_left_arm.toml --s0-bin <绝对路径> --
  <s0 args，不含 --config>`（右臂对称 `xlerobot_right_arm.toml`；
  **R10-1 单源规则：guard exec 时自动追加
  `--config <GuardConfig.config_path>` 到 s0 args——`--` 后再带
  `--config` = 配置错误退 86**，结构性保证 guard 与 `s0_daemon` 引用
  同一绝对路径）；handoff unit
  **`ExecStart=xlerobot_deploy preflight-handoff
  --left-config /etc/robot/xlerobot_left_arm.toml
  --right-config /etc/robot/xlerobot_right_arm.toml`（systemd 字面
  写死；R10-1 与 D2 权威文件名一致——仓库
  `configs/xlerobot_{left,right}_arm.toml` 安装至 `/etc/robot/`
  同名）**；S1/gateway unit `ExecStart` 各自 daemon 原样。
  **unit ×7**（r6 W4 清单口径，撤"六件"笔误）。
- `scripts/systemd/`：`xlerobot-handoff.service` +
  `xlerobot-s0-{left,right}-arm.service` + `xlerobot-s1-{left,right}.
  service` + `xlerobot-gateway-{left,right}.service`（D2/D12/D13 全部
  指令字面：Requires/After/Conflicts/Wants/RuntimeDirectory 三件套/
  `RestartPreventExitStatus=86`/60s/5/`WatchdogSec`）+ 新 runbook
  `scripts/systemd/README_dimos_replace.md`（handoff transaction 权威
  入口 + **config 安装步（R10-1）**：`install -D -m 0644
  configs/xlerobot_left_arm.toml configs/xlerobot_right_arm.toml
  → /etc/robot/ 同名`——仓库 D2 权威文件名与安装路径一一对应，
  **handoff 双 --config / arm guard --config / guard 自动追加给
  s0_daemon 三处引用同一绝对路径** + **demo 期间旧栈 stop-only 处置**
  〔正式退役/disable/删除等 post-E，D-BI3 r3——round 8 P1-1〕+ 人工
  恢复 + **S0 死亡应急协议**〔权威文本 P.2.10〕）。
- unit：AC-E16（mock transport 注入三路径 + 两档电机集——handoff 17 /
  **per-arm 本 bus 全量〔左 8 电机 = 8 条写 / 右 9 电机 = 12 条写，
  R10-2 精确计数：右 = ID 1-6 断扭 6 条 + ID 7-9 每轮 V+T 各 2 条共
  6 条；handoff 合计 20 条〕，round 8 P0-2**；
  **反向测试两条（R9-1 + R10-2/3 修订）**：① bus1/bus2 各注入同号
  motor〔如 ID 3〕失败 → report 两条按 `PreflightFailure` 的 `arm`
  分别归属不混淆；② bus1 = `Err(构造失败)` → 断言 bus2 **完整有序
  写序全部执行**——精确序列 `[T1..T6, V7,T7, V8,T8, V9,T9]` 12 条
  （**断言序列本身而非只断数量；每轮 V 必须先于同轮 T**）+ 各自
  readback → 最终聚合 Err 含 `Connect{Left}`）/ AC-E21（三
  分支 + 损坏方向 ×2 + dir-fsync 序列 + truth table，tempdir）/
  AC-E22 unit 级（unit 文件指令文本断言 + **R10-1 三处 config 路径
  一致锁**：handoff 双 `--config` / arm guard `--config` / guard 自动
  追加 s0 argv——同一 `/etc/robot/xlerobot_{left,right}_arm.toml`
  字面；`--` 后带 `--config` 退 86 的拒绝 case）+ guard 计数全路径（NotFound=0
  / 0..=5 / 越界 / flock 冲突 / pre-increment 崩溃模拟〔kill 子进程〕）。

### P.2.9b 条件段(9b)〔round 7 删除——D15-R 拍 (c1+)〕

- ① 能力核实结论（收窄口径）= STS3215 **无已文档化、可配置的 command
  watchdog，且当前真机失联持续带扭**（证据链见 D15 节）——原"写
  watchdog 寄存器 + readback"内容不可执行；(c1+) 下**无硬件 fail-safe
  落地段**，本段删除。
- **预留接口 = 仅文档级集成契约**（四类操作 arm / heartbeat(kick) /
  request_safe(disarm) / status+readback；集成点 = handoff/guard 与
  允许 `SetEnabled(true)` 之间；缺失/失联/readback 失败 fail-closed
  ——权威文本见 D15-R 节），**不写假 no-op watchdog trait**；实际 API /
  供电拓扑 / (b)(c2) 选择随 post-E 硬件 CodeMap 再立段。

### P.2.10 段(10-demo) 真机受监督 demo 段（round 7 重写；D15-R (c1+) 显式豁免）

- **demo 安全边界（五条全满足才执行 Enabled 动作；checklist 进
  runbook，记录进证据 docs）**：
  1. 机械隔离、工作区无人、无负载，操作者全程在场且可立即切断总电源；
  2. 新 unit 不设开机自动控制——权威 config `auto_enable = false`
     （P.2.2），每次上扭均显式 `SetEnabled(true)`；
  3. **先单臂低速/小幅验证，再双臂**（round 8 P1-2 可判定数值：单臂
     探针 |target−anchor| ≤ **0.087 rad**、轨迹最大斜率 ≤
     **0.14 rad/s**——既有 ±5°、0.25 Hz 真机验证量级；**撤"低扭矩"
     宣称**，Feetech 路径无可 readback 的 torque/current 限制面）；
     任何单侧异常立即 cancel 双侧并双发 EStop（client 侧协议，
     D17 (ii) 受监督边界）；
  4. 旧栈保留可恢复启动路径（可 `stop`，不 `disable`、不删除 unit）；
  5. kill -9 后持续带扭**不得描述成通过**——仅残余风险证据。
- **步序**：
  1. Orin 本地 build（`xlerobot-real` + release + 活清单 features）→
     装 unit ×7 → `systemctl start` 目标臂（handoff transaction 自动
     链）；
  2. 读状态（只读；不自动上扭）；
  3. 安全边界 checklist 确认 → 单臂低速/小幅（显式 SetEnabled，数值
     判据同上）→ 双臂 chunk → 双夹爪 = **AC-E12** + AC-E16/E21/E22
     真机侧；
  4. 资源基线采集（下）→ 证据 docs；**无退役步**（D-BI3 正式退役推迟
     post-E）；收口宣称 = **"机器人控制执行面受监督功能 demo 跑通"**
     （不得宣称生产替换 / 无人值守 / 硬件 fail-safe 完成）。
- **应急协议（round 8 P0-2，进 runbook）**：Enabled 期间**任一 S0 进程
  死亡/重启 → 立即人工切断总电源并中止本轮**；**不得只依赖 EStop 弱
  Ack**——S0-dead 时 Ack = published_at_ns（候选 D D2 弱 Ack 语义）
  **不证明事件已被消费**；恢复 = 断电确认 → 排查根因 → 重新走安全边界
  checklist 从步 1 起。
- **一次性 characterization（可选，不计 gate，round 8 移出必经步）**：
  kill -9 带扭 ≥10 s 行为证据——条件 = **机械隔离 + 关闭自动 restart**
  （临时 stop/mask 对应 guard unit，防 systemd 拉起干扰观察）+ 人值守
  断电在手；**已有证据足够时可直接不重跑**（D15 节 2026-07-15 桌面 +
  行为证据链已在档）。
- **资源基线（只采不执法，资源裁决沿用）**：
  - CPU/RSS：权威 arm config ×2 `[monitor]`（`interval_secs = 1` +
    `interested_processes = ["s0_daemon", "s1_daemon",
    "robot_rpc_gatew"]`——**截断名**，R5-6 confirmed；robot-health
    精确 comm 匹配 `lib.rs:210`）；(comm,pid) 归档 + 6 unit
    `systemctl show -p MainPID` join 映射左右臂；**warmup = 丢每进程
    首帧；最小样本 ≥30 帧/进程**，不足延长尾窗。
  - **synthetic iceoryx2 transport latency baseline = 既有
    `iceoryx2_rt_baseline` bin**（robot-tests，v0.2 Step 6 验收工具）
    ——**宣称收窄（round 8 P1-2）**：它测的是**同进程双线程 synthetic
    iceoryx2 pub/sub 端到端延迟**，**不是 S0/S1/gateway 生产线程调度
    抖动**；Orin demo 窗口跑 `--duration 10m --hz 50`，**0 新生产
    代码**；**只采样不执法——bin 自带阈值失败如实登记但不阻断受监督
    demo**；生产线程 tick 用时与真实调度抖动留 post-E。
  - 归档格式 = 收口 docs 表格（每进程一行 CPU%/RSS 峰值 + latency
    baseline 一行 **p50/p99/p999/max + drops/overruns + 完整运行参数
    + SCHED_FIFO/mlockall/CPU affinity 是否成功**）+ journal 摘录
    附件。
- 证据（journal 摘录 + 验收 bin 输出 + 安全边界 checklist 记录）进收口
  docs（summary + CHANGELOG demo 执行补录 + CLAUDE 状态块 + handoffs
  README——**口径统一"受监督功能 demo"**）。

## P.3 Plan 拍板项（round 1 全部已拍，2026-07-14）

| # | 项 | 拍板 |
|---|---|---|
| PD-E1 | 段序与并行度 | **全串行落 commit**（round 7 收缩后权威序）：(1)→(2)→(3)→(4)→(5)→(6)→(8)→(7)→(9)→(10-demo)——先落最终 daemon liveness（段 8），再让旗舰 workflow e2e（段 7）覆盖它；段(5b)/(9b) 已移出候选 E |
| PD-E2 | guard / preflight / handoff 可执行入口宿主 | **(a)**：生产 guard/handoff 入口放 **robot-s0 的 `xlerobot-real` bin**，共享实现放 **robot-s0 lib**；验收 bin 仍在 robot-tests；**不得让生产 unit 指向测试 crate** |
| PD-E3 | workflow e2e 资源分配 | 每 case 派生 **`dimos_e2e_<pid>_<seq>_{left,right}`**；hardware 固定 `left_arm`/`right_arm`；**两端口均由 `127.0.0.1:0` 分配并断言不同**；单 test `GLOBAL_LOCK` + RAII 清理；共享语义模块 = **`robot_rpc::client::dual_arm_workflow`**，子进程 harness 留在 robot-rpc/tests |
| PD-E4 | CHANGELOG 节奏 | **(a) 随语义所在段登记**（P.2 清单） |
| PD-E5 | mock_s1 layout 对齐方式 | **(a) config 派生是唯一权威**；mock_s1 补 `--config` 入口 + 既有调用迁移（段 4）；显式 `--layout-id` 仅作一致性断言、冲突 fail-fast |
| PD-E6 | sd_notify / LivenessObserver 共享位置 | **(c) `robot-health::systemd` 单一实现**（round 2 拍，reviewer 更正 (a) 前提：systemd_notify 用 tracing 非纯 std、上移 robot-core 破坏依赖边界；robot-health 定位可复用且 S0 已依赖无循环）；S1/robot-rpc 各 +1 workspace 依赖边，P.0 口径改"0 新外部 crate" |

## P.4 Plan-Execute diff 登记（实施期填）

| # | 段 | 类型 | 描述 | 落点 |
|---|---|---|---|---|
| （待实施期） | | | | |
