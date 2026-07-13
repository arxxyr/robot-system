# Spec · v0.5 候选 E — 替换 dimos-deploy 机器人控制执行面（round 1）

> CodeMap：[`team-swe/codemap/v0_5_dimos_replace.md`](../codemap/v0_5_dimos_replace.md)
> （round 4 **Approved**，amend 记录 round 1-3 = B1-B10 全闭）。本 Spec round 1
> 按 reviewer 指示**先拍 Q10/Q11/Q12 三项 P0**（D1/D2/D3），再 D4-D12 覆盖
> 其余 Q + AC 草案 + hard boundaries。**0 代码改动 / 不开 Plan / 不实施**。
> 前置链：D-BI0~3 拍板（2026-07-13）+ CHANGELOG 2026-07-13 premise 登记。

## What

**E = 替换 dimos-deploy 的机器人控制执行面**：demo workflow 控制执行链在
robot-system 侧原生复现，达到冻结基线（`executor.py:72` 一圈 = 12 维 action →
读关节位形 → 双臂 trajectory〔各 5 臂关节〕+ 双夹爪下发）。验收 = 双 gate
（D-BI2：Rust cross-process e2e 自动 gate + Orin 真机"读状态→双臂→双夹爪"
完结 gate）。policy 推理 / 相机 / Web UI **不在 E**（D-BI0）。

**总形态预告（若 D1-D3 + D6 按推荐拍定）**：E **零 wire 改动**——0 新 proto
method、0 新 fbs schema、0 改 frozen 字段；全部工作落在 robot-msg lib 函数 +
S0 command 防错位 + S1 语义补全（G1）+ configs + client 侧 helpers + e2e +
部署件。

## 影响范围

- **In**：robot-msg（layout_id 计算 pub fn）；robot-s0（真 layout_id + command
  侧 layout gate）；robot-s1（layout 派生/fail-fast + `scripted_start_pose` +
  `idle_hold_target` + 常驻 state cache）；configs（左臂 arm-only 新 config）；
  robot-rpc client 侧 helpers（轨迹转换 + layout 校验）；robot-rpc/tests
  workflow e2e；robot-tests 真机验收 bin；scripts/systemd S1/gateway unit +
  新 runbook。
- **Out（hard boundaries 镜像，详 §"严禁改动"）**：proto/fbs wire 0 改；
  design.md 0 改；dimos-deploy 0 改；robotctl 0 改；HoldPose 行为 0 改；
  IkBackend / kinematics 两 crate 0 改；候选 A-D 已 land 行为 0 退化。

---

## 关键设计决策（D1-D3 = P0 优先拍；D4-D12 随后）

### D1（Q10，P0）⚠ 拍板：joint 名序权威 = **(a) TOML 派生链** + S0 command 防错位硬约束

**采纳 CodeMap Q10 (a) + round 3 B9 共同硬约束**，四段一条链：

1. **计算权威（robot-msg 新 pub fn）**：
   `compute_layout_id(robot_id, hardware_id, joints: &[(name, joint_id)]) -> u64`
   = `blake3(robot_id ‖ 0x00 ‖ hardware_id ‖ 0x00 ‖ ∀i∈config 顺序: name_i ‖
   0x00 ‖ joint_id_i(u32 LE) ‖ 0x00).truncate(8)`。**顺序敏感序列哈希**同时
   覆盖 joint_names + joint_ids + order 三成分（design §3.5.1 字面
   "sorted_joint_names, sorted_joint_ids, order" 的可执行化解读——排序去序 +
   单独 order 成分不如序列哈希直接；**解读差异走 CHANGELOG 登记**，design.md
   0 改）。`schema_lint.rs` 私有 helper（sorted-names 顺序无关，`:190`）改为
   调用新函数——顺附修复其"证不了顺序"缺陷。
2. **S0**：启动期从 config `[[hardware]].joints`（含 motor_ids）算 authoritative
   layout_id；`pub_cfg.joint_layout_id` **撤 `0xCAFE_BABE_DEAD_BEEF` 占位**
   （`s0_daemon.rs:858`）→ `JointState.layout_id` 变真值（wire 字段既有，
   **值语义变更**，登记 CHANGELOG）。**command 侧硬约束（B9）**：ipc_thread
   在 `parse_joint_command` **之前**比对 `fb.layout_id()` ≟ authoritative——
   不一致 → **丢弃、不写 `LatestCommandSlot`** + `unknown_layout_count` 计数
   + 限频结构化 warn。闭 design §3.5.1"未知 layout 不得解析数组"
   （`00_design.md:524`）在 command 面的违反（`ipc_thread.rs:1177-1192`）。
3. **S1**：layout_id **由 config 派生**（同一函数）；`--layout-id` 显式给且
   ≠ 派生值 → **fail-fast exit 2**（参数族错误，沿 gateway config-mismatch
   先例）；不给 → 用派生值（**默认 `0xCAFE…` 撤销**，`s1_daemon.rs:133`）。
   一切 command producer（含 mock_s1，形态 Plan 定）的 layout_id 必须 =
   config 派生值。
4. **client（e2e / 验收 bin / 转换 helper）**：从**同一 config TOML** 读有序
   joints 作 name→index 权威 + `compute_layout_id` 对 `GetState.layout_id`
   （`RobotState.layout_id` 字段既有）校验；不匹配 = 硬失败，**禁止裸固定
   索引**。

**D1 子拍板（阈值升级语义）**：design §3.5.1 要求 unknown_layout 超阈值
"升级 DEGRADED"，但 **S0 状态机无 Degraded 态**（S1 才有）。选项：
- **(i)（推荐）**E 范围内 = 计数 + 限频日志，**不动 S0 状态机**；"阈值 →
  状态升级"条款登记 CHANGELOG 推迟（附复审触发：S0 观测面候选时重估）。
- (ii) 新 `FaultCode::UnknownLayout`（§1.4 enum append，走 CHANGELOG）+
  阈值进 Fault——状态机改动，E scope 变大。

**后果**：S0/S1 同 commit 原子切换（单仓）；既有 4 e2e harness 自写 config、
两端同函数派生 → 天然一致，预期 0 改语义只改来源；`0xCAFE…` 从生产路径消失。

**AC 钩子**：AC-E1（compute 确定性 + 顺序敏感 + id 敏感 + 与 lint 一致）/
AC-E2（S0 mismatch command → slot 不写 + 计数，cross-process）/ AC-E3
（S1 CLI 冲突 fail-fast exit 2）/ AC-E4（client 校验路径 e2e）。

### D2（Q11，P0）⚠ 拍板：E 权威 config = **(i) arm-only 6-motor × 2 bus**

**采纳 CodeMap §5.1 方案 (i)**：底盘轮与 head **不进 S0 hardware 向量**。

- **右臂**：沿既有 `configs/xlerobot_bus2_right_arm_full.toml`（ID 1-6 =
  5 臂 + 夹爪，by-id 串口，calibration 合并拣取先例）——是否原样用 / 对称
  重命名，Plan 定，语义不变。
- **左臂（新增）**：`configs/xlerobot_bus1_left_arm.toml`（命名 Plan 可调）：
  bus1 by-id 串口（`…5A7C121427-if00`）、motor_ids `[1..=6]`（5 臂 + 夹爪
  =ID6，`config.py:112-144` 电机表）、joints 顺序 = 冻结基线 ACTION_DIM
  左臂序（`left_arm_shoulder_pan … left_arm_wrist_roll, left_arm_gripper`）、
  calibration_paths = `left_arm.json`（head.json 冗余给齐与否 Plan 定，
  factory 按 joint 拣取，右臂先例证明冗余无副作用）。
- **robot_id 双栈命名提案**：`xlerobot_left` / `xlerobot_right`（topic 空间
  独立，D4 联动）。

**后果**：
- 底盘 `Operating_Mode=1` 残留风险**整体消除**（S0 永不写轮子；Q5 收窄、
  Q9 无 mode 硬化链）。
- head（ID 7,8）不受 S0 驱动——上电后保持自身扭矩状态，验收姿态可接受
  （登记事实，不作 AC）。
- **底盘/头的驱动 = 非 E 目标**（与 D-BI0 一致：12 维无 head/base）；未来
  要底盘走独立候选 + full config + mode 硬化链（CodeMap §5.1 (ii) 封存
  备查）。
- `xlerobot.toml`（陈旧）与 `xlerobot_bus{1,2}_full.toml` **保留不删**
  （历史/Stage2 参考），E 文档明确权威 = 本 D2 两 config。

**AC 钩子**：AC-E5（左臂 config 载入 + 派生 layout_id + mock adapter 双栈
e2e 全通）。

### D3（Q12，P0）⚠ 拍板：G1 闭合 = **(c) 等价：新增 `scripted_start_pose` + `idle_hold_target` 两概念，HoldPose 0 改**

**采纳 CodeMap Q12 (c) + round 3 B10 执行边界**——不动共享 `anchor_bias`：

1. **`scripted_start_pose`（per-action，修"起点半"）**：Scripted `start()` 的
   segment0 from = **提交时当前位**，不再用启动期 `anchor_bias`
   （`scripted.rs:186` 现状）。
   - **来源**：S1 新增**常驻 state cache**——一条 state subscriber 线程持续
     更新 latest `JointState`（复用启动锚定的校验语义：joint_count 匹配 +
     `max_stale_ms` 新鲜度 + **layout_id == 派生值**〔D1 联动〕）。
   - **失败语义**：cache 无帧 / stale / layout 不匹配 → Scripted start
     **拒绝**（`ActionStartError::Rejected` → tonic `FailedPrecondition`），
     **绝不 fallback 到启动 anchor**（静默跳变危险）。
2. **`idle_hold_target`（coordinator 级，修"回拉半"）**：coordinator 维护
   **last-commanded target**（每次 publish cmd 后更新）；Idle / Degraded 的
   hold cmd target 从 `anchor_bias` 改为 `idle_hold_target`。**初值 = 启动
   anchor_bias** → boot 后首个 action 之前行为与今日 **bit 等价**；首个
   action 终态后 hold = 该 action 最后 target，**不回拉**。
   - **B1 等价语义解锁**：Idle hold ≡ B1（`coordinator.rs:865-871`，v0.3
     reviewer 锁定）按本条**显式解锁**——新等价宣称 = "Idle hold ≡ B1 形态
     （build_hold_command 路径）+ target 源变更"；登记 CHANGELOG + AC-E7
     锁新行为。
3. **HoldPose 0 改（B10 执行边界）**：继续每 tick 读 `anchor_bias`（启动值，
   构造期固化）；`hold_pose.rs` 不动。两个新概念与共享 anchor **物理隔离**
   （字段/传参形态 Plan 定；语义边界本条锁死）。

**G1 闭合声明**：起点半（1）+ 回拉半（2）= CodeMap Q12 (c) 等价统一方案；
Q2 的"G1 两半全部闭合"前置由本条满足。

**D3 子拍板**：stale 阈值缺省沿既有 anchor `max_stale_ms` config（不新增
配置面）；拒绝码固定 `FailedPrecondition`。

**AC 钩子**：AC-E6（start_pose = 提交时当前位，非启动位；stale/layout 失败
→ FailedPrecondition，cross-process）/ AC-E7（连续两 chunk 无回拉：chunk1
终态后 hold = chunk1 末位；chunk2 从当前位起插值无跳变）/ AC-E8（**HoldPose
回归**：行为与既有 bit 等价，hold target == anchor_bias）。

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

### D6（Q3）拍板：gripper = (a) 复用 Scripted 全向量同车

D2(i) 下 gripper 是 6 维向量第 6 位——**M4 缺口的闭合形态 =
`set_gripper_position` ↦ gripper 目标位入 waypoint 向量**（与臂同车）。
0 新 method / 0 新 schema。CodeMap Q3(b)（新 Gripper RPC，b1 强 RR / b2 弱
Ack 拆分）**封存备查**：post-E 若需绕-action-槽直写再起独立候选。

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
真机验收 bin 进 robot-tests（required-features），**两者复用同一 case 库**
（共享 lib 形态 Plan 定）。

### D10（Q7）拍板：真机完结 gate 驱动 = (c) 专用验收 binary 为主

robot-tests 新验收 bin（required-features 守门；phase 化：读状态 → 双臂
chunk → 双夹爪 → 汇总），机器可判 + 人眼可看；robotctl 作旁路证据面
（现有 `status`/`watch`；**不新增子命令**——补齐 get-state 等登记为
post-E robotctl 候选）。

### D11（Q8）拍板：单位 = 全程 rad，[0,1] 概念不进 E

wire / S0 / S1 / client 全程 rad（S0 0 改）。`[0,1]` 仅存在于"从 dimos
基线数据导入"的转换输入层：`rad = rad_lo + v × (rad_hi - rad_lo)`，
rad_lo/hi 由该 gripper 标定 `range_min/max`（raw）经 `raw↔rad` 公式而来
（两栈共用同一批标定 JSON，CodeMap §0.3/§4.2）。答案落 CodeMap §5.2 之问：
**"[0,1]" 这个词在 E 运行时不存在**。

### D12（Q9）拍板：部署段 = S1/gateway unit + 新 runbook；deploy.sh 不进 E

- systemd unit：S1 ×2 + gateway ×2（沿 S0 unit 先例：`Type=notify`？——
  S1/gateway 无 sd_notify 集成，unit Type 与 watchdog 形态 **Plan 定**，
  Spec 只锁范围）+ `Conflicts=xlerobot-coordinator.service
  xlerobot-web.service` 沿 S0 先例。
- 新真机 runbook（替代两份 pre-strict 档案；含 D-BI3 顺序锁操作面：验收前
  `sudo systemctl stop` 旧栈、不 disable、不删 unit、回滚 = `systemctl
  start`）。
- **deploy.sh 不进 E**（登记独立 follow-up；Orin 本地 build 惯例沿用）。
- D2(i) 下**无底盘 mode 硬化链**。

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
8. 候选 A-D 已 land 行为 0 退化（AC-E13）；B1/B3/B4 既有契约除 D3 显式解锁
   的 Idle hold target 源外全保（source_task_id 单调 / RR wire / events
   drain 全不动）。

## 不在范围（登记）

底盘/头驱动（D2）、mid-chunk 抢占（D5）、Gripper 独立 RPC（D6 封存 b1/b2）、
robotctl 子命令补齐（D10）、deploy.sh（D12）、Python client（D-BI1(i)）、
相机 / policy 推理 / Web UI（D-BI0）、S0 unknown_layout 阈值→状态升级
（D1 子拍板 (i) 时登记推迟）。

## 拍板记录

| # | 决策 | 拍板 | 日期 | 备注 |
|---|---|---|---|---|
| D1 | Q10 layout 链 (a)+硬约束；子拍板 阈值语义 (i)/(ii) | 待 | | P0 |
| D2 | Q11 config (i) arm-only ×2 | 待 | | P0 |
| D3 | Q12 G1 (c) 拆分两概念 + HoldPose 0 改 | 待 | | P0 |
| D4 | Q1 双栈双 gateway (a) | 待 | | |
| D5 | Q2 per-chunk + 等终态 (a) | 待 | | |
| D6 | Q3 gripper 同车 (a) | 待 | | |
| D7 | Q4 adapter = robot-rpc client helper (iii) | 待 | | |
| D8 | Q5 填当前位（仅臂向量） | 待 | | |
| D9 | Q6 e2e 双轨 (c) | 待 | | |
| D10 | Q7 验收 bin 为主 (c) | 待 | | |
| D11 | Q8 全程 rad | 待 | | |
| D12 | Q9 unit+runbook、deploy.sh 不进 E | 待 | | |
