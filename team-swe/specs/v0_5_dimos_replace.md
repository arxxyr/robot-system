# Spec · v0.5 候选 E — 替换 dimos-deploy 机器人控制执行面（round 6）

> CodeMap：[`team-swe/codemap/v0_5_dimos_replace.md`](../codemap/v0_5_dimos_replace.md)
> （round 4 **Approved**，amend 记录 round 1-3 = B1-B10 全闭）。Spec round 1
> 按 reviewer 指示**先拍 Q10/Q11/Q12 三项 P0**（D1/D2/D3），再 D4-D12 覆盖
> 其余 Q + AC 草案 + hard boundaries；round 1 复核 = **方向拍定（D1 (a)+(i) /
> D2 (i) / D3 (c)，条件生效）+ 4 P0 阻断 + 3 执行语义阻断**，round 2 逐条
> 回修；round 2 复核再出 **7 执行级阻断（T1-T7，P0 ×4 + P1 ×3）**，round 3
> 逐条回修；round 3 复核（T4/T6 闭环）再出 **6 项（U1-U6，P0 ×3 + P1 ×3）**，
> round 4 逐条回修；round 4 复核（U3/U5/U6 闭环）再出 **4 项（V1 P0 +
> V2-V4 P1）**，round 5 逐条回修；round 5 复核（V3/V4 闭环）再出 **4 项
> （W1 P0 + W2-W4 P1）**，本 round 6 逐条回修（均见"Spec round 记录"）。
> **0 代码改动 / 不开 Plan / 不实施**。
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
       副本**严格解析为 0/1** → **只写缺失文件**（原子写 + fsync，值 =
       现存副本值，**不经 `save_estop_latch` 重写现存副本**）；现存副本
       **损坏 / 不可读 / 值非法 → provision 直接失败且不得创建另一份**
       （交给限流 → failed，人工介入）。（依据：既有双写允许"primary
       成功、backup 失败并返 Err"〔`estop_latch.rs:183` 区块文档〕，r4
       "任一份存在即跳过"会单副本起跑；而 r5 按存在性补写在"primary 损坏
       + backup 缺失"时无合法值可复制——本分支两面都闭）；
     - 双份均存在 → **不动任何文件**，交给 loader 既有语义（下表）。
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
  4. provision 步序：本臂 unit `ExecStartPre` 第二步（preflight 之后、
     `ExecStart` S0 之前，systemd 事务内——r4 U1，详 D13）。

**AC 钩子**：AC-E5（双新 config 载入 + typed 校验 + 派生 layout_id + mock
双栈 e2e 全通）/ AC-E21（r4 对齐 + r5 三分支：新身份首启 latched Safe →
ClearEStop → 仍 Safe → SetEnabled(true) → Ready 全链；provision 三分支
各一 case〔双缺失写入 / 单份缺失补写既存值 / 双份存在不动〕；**backup 写
失败 → provision Err → unit inactive → 重启后补写成双份 → 启动**；truth
table 四行各一 case）。

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
  显式 Type=simple 非目标（`s1_daemon.rs:54`）——**E 是旧控制栈退役 gate，
  按设计补齐**：S1 与 gateway 各接 **liveness-aware sd_notify**（沿 S0
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
  旧栈 + 全 17 电机 ensure-safe〕→〔本臂 ExecStartPre：preflight +
  provision〕→ S0 起 → 验收 → 通过后才 disable 旧栈〔D-BI3 顺序锁〕；
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

**fail-closed preflight（锁定；r4 U3 ensure-safe 语义重写）**：真机启动
任一 arm-only S0 **之前**，对该臂所属 bus 的 **omitted motors**（左 = bus1
head ID 7,8；右 = bus2 wheels ID 7,8,9）：

1. （审计步，可选）预读当前值——**仅记录，不决定是否执行安全写**；
2. **无条件 best-effort 安全写全部 omitted motors**：bus1 head 逐电机
   `Torque_Enable = 0`；bus2 wheels 逐电机**先 `Goal_Velocity = 0` 后
   `Torque_Enable = 0`**（顺序锁定：先零速再断扭）；**单电机/单寄存器写
   失败不中断**，继续处理其余电机；
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
  聚合语义同上 fail-closed 段（单项失败继续、末尾聚合 FAIL）；"禁止单臂、
  强制成对启动"备选**未采**。两个 arm-only S0 unit **`Requires=` +
  `After=`** handoff unit → **仅启动一臂也先保全部 17 电机安全**（随后
  该臂 S0 的 enable 序列只重新上扭自己的 6 电机）；同 boot 内第二臂启动
  时 handoff 已 active（RemainAfterExit）**不再重跑**（不碰第一臂已占
  串口）；reboot 后自动重新验证（断电后扭矩本就复位，重验无害且
  fail-closed）。
- **每臂 `ExecStartPre` 两步（restart 路径守门，r5 定位收窄）**：
  `ExecStartPre=① 本臂 bus preflight`（**只触本臂 bus**——左碰 bus1、右碰
  bus2，任何时刻不碰另一臂串口）→
  `ExecStartPre=② 本臂 latch provision`（D2 三分支规则）→
  `ExecStart=s0_daemon`。同 unit 事务内顺序执行；**失败/重试语义
  （r6 W3 修正——`Restart=` 同样作用于 ExecStartPre 失败，"稳定 inactive"
  表述撤回）**：沿 S0 unit 既有策略 `Restart=on-failure` +
  **`RestartSec=2s` + `StartLimitIntervalSec=60s` + `StartLimitBurst=5`**——
  ExecStartPre 失败 → 自动重试（每次重试重跑两步 Pre，**失败期间绝不执行
  ExecStart**）→ 60s 内 5 次仍失败 → **进入 failed（start-limit-hit）**
  稳定停止；watchdog 重启同样重跑两步 Pre（handoff unit 因
  RemainAfterExit 不重跑；restart 窗口本臂只守本 bus——另一 bus 的安全由
  handoff 首次切换建立〔全 17 电机〕，之后由其自身 S0 生命周期接管，
  **单臂运行期间另一臂电机保持 handoff 断扭态**，r5 "归另一臂管辖"表述
  更正）。
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
写寄存器——ExecStartPre 窗口本臂串口无人占用：旧栈已停〔Conflicts+After〕、
本臂 S0 未起）+ **每臂 ExecStartPre 强制** + runbook 硬步序（三层同向）。

**AC 钩子**：AC-E16（r4 语义：无条件安全写全部 omitted motors〔bus2 先零速
后断扭〕+ 单项失败继续 + 末尾聚合 FAIL 禁启；写失败 / readback 失败 /
readback 值不安全三路径均不起新栈；mock transport unit + 真机 gate 实测）/
AC-E22（r4 四场景：**同时启动**双臂〔各自 preflight 只碰本 bus 无串口冲突〕
/ **错峰启动**第二臂〔不碰第一臂已占串口〕/ **直接 `systemctl start`**
〔ExecStartPre 失败 → unit 不进 active〕/ **watchdog 自动重启**
〔ExecStartPre 重跑仍 fail-closed〕）。

---

## AC 草案总表（round 2+ 细化编号与判据）

| AC | 锁什么 | 层 |
|---|---|---|
| AC-E1 | `compute_layout_id` 确定性 / 顺序敏感 / id 敏感 / lint 同源 | unit |
| AC-E2 | S0 layout mismatch command → 丢弃不写 slot + 计数 | unit + e2e |
| AC-E3 | exit 码全表（r4）：CLI 身份/`--layout-id` 冲突 → S0/S1 均 exit 2；config 内容错误 → S1 exit 3 / S0 exit 2；S0 adapter 构建失败 exit 1 区分 | unit |
| AC-E4 | client layout 校验不匹配硬失败 | e2e |
| AC-E5 | 左臂新 config 载入 + 双栈（2×s0+s1+gateway）mock e2e 全通 | e2e |
| AC-E6 | scripted_start_pose = 提交时当前位；stale/layout 失败 → FailedPrecondition 不 fallback | e2e |
| AC-E7 | 连续两 chunk 无回拉、无跳变（G1 闭合） | e2e |
| AC-E8 | HoldPose 回归 bit 等价（target == anchor_bias） | unit + e2e |
| AC-E9 | 非目标关节填当前位 + 漂移容差数值 | e2e |
| AC-E10 | 转换 helper：差分时长 / 首点宽容校验 / gripper [0,1]→rad 导入 | unit |
| AC-E11 | 对照移植清单三调用面 e2e 全绿（M1/M2/M3+M4 经 D6 同车） | e2e |
| AC-E12 | Orin 真机三步（读状态→双臂→双夹爪）验收 runbook 化 | 真机 gate |
| AC-E13 | 既有 1185 基线 + 6 smoke 0 退化 | workspace |
| AC-E14 | 双臂协议全路径（r3+r4）：**Ack 偏差上限 gate**（Started.at_ns 若收到仅诊断记录，缺失不判失败）/ 终态精确集与 pair success 定义 / 提交期与执行期部分失败 cancel / partial completion 报告 / stream 断开 GetTaskStatus 收敛（含超时上限）/ 双拒绝确定性 FAIL / 不收敛双 EStop | e2e |
| AC-E15 | gripper 阶跃→插值语义变化：终点一致 + 轨迹单调（非等价宣称） | e2e |
| AC-E16 | D13 ensure-safe（r4+r6）：无条件安全写 + 单项失败继续 + 末尾聚合；写失败/readback 失败/值不安全 → 均不起新栈；**范围两档各一 case**——handoff 全 17 电机（bus1 1-8 断扭 / bus2 1-6 断扭 / bus2 7-9 先零速后断扭）+ per-arm restart 子集（本 bus omitted motors） | unit + 真机 gate |
| AC-E17 | 末 waypoint 先 Continue（target=末位）后 Completed（dwell=0 路径发布保证） | unit + e2e |
| AC-E18 | D3 穷尽式规则 + **序列测试**（r3+r4：先 Success 一次再分别 cancel/failure/IK/**HoldPose cancel**，逐一断言 reset 回启动 anchor；不得只测 boot 初态） | e2e |
| AC-E19 | S1/gateway sd_notify watchdog 双向（r3）：**长时间零请求不重启**（空闲 ≥ 数个 WatchdogSec 周期）+ **冻结每类 worker 均触发重启** | smoke |
| AC-E20 | state cache 线程 stop+join 正常 Drop（无 mem::forget） | unit |
| AC-E21 | latch provision（r5 三分支 + r6 合法性收紧）：状态链 latched Safe → ClearEStop → 仍 Safe → SetEnabled(true) → Ready；分支 case = 双缺失写入 / 单份缺失且现存合法补写 / **primary 损坏+backup 缺失 → provision 失败不创建**（+ 反方向 backup 损坏+primary 缺失）/ 双份存在不动；backup 写失败重启补写路径；**瞬态失败自动重试恢复 + 持续失败 60s×5 进 failed**；loader truth table 四行各一 case | unit + e2e |
| AC-E22 | 启动拓扑强制（r4 四场景 + r5/r6 修订）：同时启动双臂 / 错峰启动第二臂（不碰已占串口）/ 直接 `systemctl start`（Pre 失败 → 限流重试期不执行 ExecStart → 5 次后 failed）/ watchdog 自动重启（Pre 重跑仍 fail-closed）/ **旧栈 active 时仅启动一臂 → 唯一结果 = handoff 对全部 17 电机 ensure-safe 后该臂启动**（断言精确 ID 集：bus1 1-8 / bus2 1-6 / bus2 7-9，"或启动被拒"歧义删除） | 真机 gate（+unit 级依赖声明检查） |

## 严禁改动（hard boundaries）

1. **wire 0 改**：0 新 proto method / 0 新 fbs schema / frozen 字段 0 改
   （`JointState.layout_id` / `JointCommand.layout_id` 字段既有，仅**值语义**
   变更走 CHANGELOG）。
2. §6.7 五条全保（D1 的 S0 command gate = 防错位**强化**，不触 latch/EStop
   链）。
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

## 拍板记录

| # | 决策 | 拍板 | 日期 | 备注 |
|---|---|---|---|---|
| D1 | Q10 layout 链 (a)+硬约束 + 子拍板 (i) | **方向已拍 (a)+(i)**；r2 条件落字待确认 | 2026-07-13 | P0；§3.5.1 部分落地登记 |
| D2 | Q11 config (i) arm-only ×2 + 身份全表 + latch provision（r3；r4 truth table + 状态链更正） | **方向已拍 (i)**；r2-r4 条件落字待确认 | 2026-07-13 | P0；units/交接经 D12/D13 |
| D3 | Q12 G1 (c) + 发布保证 + 行为矩阵 + 穷尽式单一规则（r3 reset；r4 穷尽化） | **方向已拍 (c)**；r2-r4 条件落字待确认 | 2026-07-13 | P0 |
| D4 | Q1 双栈双 gateway (a) | 待 | | |
| D5 | Q2 per-chunk + 等终态 (a) + 双臂成对协议（r2；r3 终态精确集/不对称/partial/双拒；r4 偏差 gate 降级 Ack、Started 仅诊断） | 待 | | |
| D6 | Q3 gripper 同车 (a) + 插值语义变化登记（r2） | 待 | | |
| D7 | Q4 adapter = robot-rpc client helper (iii) | 待 | | |
| D8 | Q5 填当前位（仅臂向量） | 待 | | |
| D9 | Q6 e2e 双轨 (c) + case 库落位 robot-rpc（r2） | 待 | | |
| D10 | Q7 验收 bin 为主 (c) + preflight phase 宿主（D13） | 待 | | |
| D11 | Q8 单位分层：helper rad-native + 原始 12 维 shim（r2） | 待 | | |
| D12 | Q9 S1/gateway sd_notify+watchdog 纳入 + 运行时 unit ×6 + handoff ×1 + runbook〔handoff transaction 权威入口，r6〕；deploy.sh 不进 E | 待 | | B1 决策 #7 解锁 |
| D13 | 真机交接 fail-closed preflight（r2 新增；r3 systemd 强制；r4 每臂 ExecStartPre + 无条件安全写；r5 全局 handoff + gateway Wants=S1；r6 handoff 全 17 电机拍死 + 限流语义） | 待 | | **P0 安全** |
