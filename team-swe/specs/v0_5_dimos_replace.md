# Spec · v0.5 候选 E — 替换 dimos-deploy 机器人控制执行面（round 2）

> CodeMap：[`team-swe/codemap/v0_5_dimos_replace.md`](../codemap/v0_5_dimos_replace.md)
> （round 4 **Approved**，amend 记录 round 1-3 = B1-B10 全闭）。Spec round 1
> 按 reviewer 指示**先拍 Q10/Q11/Q12 三项 P0**（D1/D2/D3），再 D4-D12 覆盖
> 其余 Q + AC 草案 + hard boundaries；round 1 复核 = **方向拍定（D1 (a)+(i) /
> D2 (i) / D3 (c)，条件生效）+ 4 P0 阻断 + 3 执行语义阻断**，本 round 2 逐条
> 回修（见"Spec round 记录"）。**0 代码改动 / 不开 Plan / 不实施**。
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
（unit ×6 + preflight + runbook）。

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
  scripts/systemd **unit ×6** + 新 runbook。
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
   从 extras 提升为 **typed 字段**；校验（违者 exit 2）：
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

**AC 钩子**：AC-E5（双新 config 载入 + typed 校验 + 派生 layout_id + mock
双栈 e2e 全通）。

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
3. **`idle_hold_target` 更新规则（r2 收窄）**：**仅** Scripted 以
   `Completed(Success)` 收束**且**末 waypoint target 已按第 2 条发布后，
   coordinator 提交 `idle_hold_target = 末 waypoint target`。**其余一切路径
   不更新**。**撤回 r1 "每次 publish 更新"**（会连带改 IK 完成后行为——
   `ik.rs:315` 首 tick 即发 target hold 帧）；**撤回 r1 "Idle / Degraded
   一起改"**（hard boundary 只解锁 Idle）。初值 = 启动 anchor_bias → boot
   后首个 Success-Scripted 之前 Idle hold 与今日 **bit 等价**。
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

   cancel / failure 维持"回拉启动 anchor"= 既有行为原样；改为"hold 当前位"
   属独立语义变更，**不在 E**（登记不在范围）。
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
   同时 `InvokeTask`，并发 join 形态）；**最大启动偏差**上限进 AC-E14
   （实测锁数值）。
2. **等待双终态**（两侧 Completed 家族）后才进下一 chunk。
3. **部分失败**：一侧 accepted、另侧 rejected → **立即 `CancelTask` 已接收
   侧** → 等 cancel 终态；**cancel 不收敛（超时）→ 双侧 `EStop`**
   （operator+ RPC 既有，安全方向，弱 Ack 语义照旧）。双侧 rejected →
   本 chunk 中止，验收脚本判定重试/终止。
4. 全部路径进 AC-E14（并发偏差 / 双终态 / 部分失败 cancel / 不收敛双
   EStop）。

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
- **换算权威唯一（r2）**：`raw↔rad` 公式**只住 robot-adapter-feetech**
  （`adapter.rs:83-103`）；**robot-rpc 不得复制该公式**——验收 bin
  （robot-tests，已 dep feetech crate）用 feetech 公式从标定 JSON raw
  `range_min/max` 算 rad_lo/rad_hi，再作参数传给 shim。
- **失败语义（r2）**：NaN/Inf → reject（helper 与 shim 均）；长度不匹配 →
  reject；标定 `range_min >= range_max` → 派生期 **fail-fast**；rad-native
  路径无 clamp（垃圾值由 S0 calibration clamp 兜底 + AC 锁 reject 面）。
- r1 结论保持：**"[0,1]"（连同"度"/[0,100]）不进 E 运行时**，仅存在于 shim
  输入层（CodeMap §5.2 之问的完整答案）。

**AC 钩子**：AC-E10（shim 度/[0,100] 镜像换算 + clamp 计数 + rad_lo/hi 参数
化；helper reject 全路径；range 反转 fail-fast）。

### D12（Q9）⚠ 拍板（r2 重写，S7 阻断：不留 Plan）：S1/gateway 纳入 sd_notify + watchdog；unit ×6 + 新 runbook；deploy.sh 不进 E

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
- **unit ×6**：S0 arm-only ×2（D2 表）+ S1 ×2 + gateway ×2；全部
  `Conflicts=` dimos 两 unit；S0 新 unit 另 `Conflicts=` 旧 full S0 unit
  （D2）。旧 unit 全部保留（回滚）。
- 新真机 runbook（替代两份 pre-strict 档案；步序 = D-BI3 顺序锁 + **D13
  preflight 硬步序**：stop 旧栈 → preflight → 起新栈 → 验收 → 通过后才
  disable 旧栈）。
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

**fail-closed preflight（锁定）**：真机启动任一 arm-only S0 **之前**：

1. 停旧栈（`sudo systemctl stop xlerobot-coordinator xlerobot-web`，
   D-BI3 顺序锁；不 disable、不删 unit）；
2. 直连串口逐电机 **ensure-safe**：bus1 head（ID 7,8）`Torque_Enable`
   读回 == 0（非 0 → 写 0 → 再读回）；bus2 wheels（ID 7,8,9）
   `Goal_Velocity` 读回 == 0 **且** `Torque_Enable` 读回 == 0（同款写后
   读回）；
3. **任一读 / 写 / readback 失败或值不安全 → 禁止切换**（不起任何新栈），
   旧栈回滚能力保留（unit 未 disable，`systemctl start` 即回）。

**载体**：验收 bin（D10）preflight phase（feature `real-hw`，直连 feetech
transport 读写寄存器——此窗口串口无人占用：旧栈已停、新栈未起）；runbook
把 preflight 列为**硬步序**（跳过 = 违规）。

**AC 钩子**：AC-E16（fail-closed 三路径：读失败 / 写失败 / readback 值不
安全 → 均不起新栈；mock transport unit + 真机 gate 实测）。

---

## AC 草案总表（round 2+ 细化编号与判据）

| AC | 锁什么 | 层 |
|---|---|---|
| AC-E1 | `compute_layout_id` 确定性 / 顺序敏感 / id 敏感 / lint 同源 | unit |
| AC-E2 | S0 layout mismatch command → 丢弃不写 slot + 计数 | unit + e2e |
| AC-E3 | S1 `--layout-id` ≠ 派生值 → exit 2 | unit |
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
| AC-E14 | 双臂并发提交最大启动偏差 + 双终态 + 部分失败 cancel + 不收敛双 EStop | e2e |
| AC-E15 | gripper 阶跃→插值语义变化：终点一致 + 轨迹单调（非等价宣称） | e2e |
| AC-E16 | D13 preflight fail-closed 三路径（读失败/写失败/readback 不安全 → 不起新栈） | unit + 真机 gate |
| AC-E17 | 末 waypoint 先 Continue（target=末位）后 Completed（dwell=0 路径发布保证） | unit + e2e |
| AC-E18 | D3 行为矩阵全行（Success 新 hold；cancel/failure/IK/Degraded 回拉现状） | e2e |
| AC-E19 | S1/gateway sd_notify watchdog（worker 停滞 → 跳 ping → systemd 重启） | smoke |
| AC-E20 | state cache 线程 stop+join 正常 Drop（无 mem::forget） | unit |

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
| D2 | Q11 config (i) arm-only ×2 + 身份全表 | **方向已拍 (i)**；r2 条件落字待确认 | 2026-07-13 | P0；units/交接经 D12/D13 |
| D3 | Q12 G1 (c) + 发布保证 + 行为矩阵 | **方向已拍 (c)**；r2 条件落字待确认 | 2026-07-13 | P0 |
| D4 | Q1 双栈双 gateway (a) | 待 | | |
| D5 | Q2 per-chunk + 等终态 (a) + 双臂成对协议（r2） | 待 | | |
| D6 | Q3 gripper 同车 (a) + 插值语义变化登记（r2） | 待 | | |
| D7 | Q4 adapter = robot-rpc client helper (iii) | 待 | | |
| D8 | Q5 填当前位（仅臂向量） | 待 | | |
| D9 | Q6 e2e 双轨 (c) + case 库落位 robot-rpc（r2） | 待 | | |
| D10 | Q7 验收 bin 为主 (c) + preflight phase 宿主（D13） | 待 | | |
| D11 | Q8 单位分层：helper rad-native + 原始 12 维 shim（r2） | 待 | | |
| D12 | Q9 S1/gateway sd_notify+watchdog 纳入 + unit ×6 + runbook；deploy.sh 不进 E（r2） | 待 | | B1 决策 #7 解锁 |
| D13 | 真机交接 fail-closed preflight（r2 新增） | 待 | | **P0 安全** |
