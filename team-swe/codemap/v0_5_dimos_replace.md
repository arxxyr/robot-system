# CodeMap · v0.5 候选 E — 替换 dimos-deploy 机器人控制执行面（Pre-Research，不开 Spec/Plan/实现）

> 范围：D-BI0~3 拍板（2026-07-13）后 reviewer 放行的**四项只读勘探**：
> ① 3 个调用面映射 ② gripper 两方案比较 ③ adapter 定位 ④ Orin 双 gate 部署
> 路径。**0 代码改动 / 0 dimos-deploy 改动 / 不开 Spec、Plan 或实现**；产出 =
> 事实 + 张力 + §6 Spec 拍板 Q 表。
> 入口：[`docs/handoffs/v0.5_dimos_buyin_decision.md`](../../docs/handoffs/v0.5_dimos_buyin_decision.md)
> （已拍板）+ [`docs/handoffs/v0.5_entry.md`](../../docs/handoffs/v0.5_entry.md)
> §3.4（2026-07-13 勘误后口径）+ `docs/CHANGELOG.md` 2026-07-13 条。
> 基线：dimos-deploy master **`9ece6bd`**（勘探时实测确认，工作树干净）；
> 本仓 HEAD `6dcce1f`。dimos 框架源在独立仓 `/home/loosqk/repo/robot/dimos`
> （`a035fb315`）——dimos-deploy 的 `pyproject.toml` **不声明** dimos 依赖，
> `client.*` 语义全由框架仓决定；下文锚点 `dimos/…` 属框架仓、
> `src/xlerobot_dimos/…` 属 dimos-deploy、其余属本仓。

---

## CodeMap amend 记录（round 1；reviewer 复核后）

reviewer 4 阻断（P0 ×2 + P1 ×2），已回修：

| # | 级别 | 阻断 | 修订 |
|---|---|---|---|
| B1 | P0 | **bus_full 配置底盘失控风险**：bus2 轮可能遗留 `Operating_Mode=1`（velocity），写 `Goal_Position` 被解释为速度命令（config 必读警告 `xlerobot_bus2_full.toml:49-58`）；启用路径仅 `Enable(true)` 无 set_mode（`state_machine.rs:367-374`）。"填当前位"对轮子 = 整车移动 | §5.1 撤回"bus_full 才对"，新增配置方案比较（arm-only ×2 vs full+mode 硬化）；Q5 收窄"仅 position-mode 关节"；Q9 联锁；新增 **Q11** |
| B2 | P0 | **JointTrajectory 首点不可丢弃、M3 未闭环**：segment0 from = **S1 启动期一次性** `anchor_bias` 非当前位（`scripted.rs:186`、`coordinator.rs:1437`）；action 完成回 Idle 后 hold target 也 = 启动 anchor（`coordinator.rs:865`）→ 连续 chunk 从启动姿态重放 + action 间回拉 | §1.2 新增第 4 语义差 **G1**（P0 独立缺口），M3 判定降"**未闭环**"；撤回 §2 "(a) 0 S1 改动"宣称；§3.1 首点行改写"不可丢弃"；Q2 加 G1 前置；新增 **Q12** |
| B3 | P1 | **name→index 无权威源**：`RobotState` 无关节名（`robot_control.proto:439-450`）；`JointLayout` RPC 未暴露、layout_id 不可逆推（补充实测：S0 今日也不 publish JointLayout，仅 `pub_thread.rs:25-27` 占位 layout_id） | §3.1 行改写"禁止裸固定索引"；新增 **Q10**（TOML+layout_id 校验 vs 新增可观测面） |
| B4 | P1 | **新 Gripper RPC wire 成本低估**：绕 action 槽走 S0 RR 需 request/response **成对**内部 schema（沿 SetEnabled `0x040D/0x040E` 成对先例，`schema_id.rs:110-122`），0x040F 只是起点 | §2.2/§2.3 改"成对 schema（0x040F/0x0410 起）或另拍 publish/弱 Ack 路由，不按单 schema 估算" |

reviewer 已核实成立（保留不动）：双 robot_id / 双 gateway / BusyExecuting→AlreadyExists / 4 个 CI cross-process e2e / 0x040F 为下一空闲**起点** / 两外部仓基线干净。

### round 2（reviewer 复核后）

reviewer 4 阻断（P0 ×2 + P1 ×2）+ 非阻断 ×2，已回修：

| # | 级别 | 阻断 | 修订 |
|---|---|---|---|
| B5 | P0 | **Q10(a) layout_id 校验不可执行、也证不了顺序**：S0 固定发布占位 `0xCAFE_BABE_DEAD_BEEF`（`s0_daemon.rs:858`）；S1 `--layout-id` 独立传入无一致性闭环；唯一计算函数是私有 lint helper 且 **sorted names 顺序无关**哈希（`schema_lint.rs:190`），未实现 design §3.5.1 的 `sorted_joint_ids + order` 入哈希与 1 Hz `JointLayout` 广播/RR（`docs/00_design.md:522`） | Q10(a) 重写为"TOML 名序权威 + **补齐 layout_id 一致性链**"（order 入哈希 + S0 撤占位 + S1 对齐 + client 校验）；M1/M2 判定降"**向量载体存在，name→index 待 Q10 闭环**" |
| B6 | P0 | **Q12 非可执行决策**：(a) 需"提交时当前位"，但 S1 无常驻 state 订阅（anchor subscriber 在 `anchor_or_fail` 函数内建、读完即弃，`s1_daemon.rs:783-812`），coordinator 亦无动态 state cache / last-commanded 权威；且 (a) 单选 Idle 仍回拉、(b) 单选新 action 仍从启动 anchor 起 | Q12 重写：(a)/(b) 标**局部机制**，闭合 G1 = (c) 或等价统一方案；必答子项 = 当前位来源 / freshness+layout 校验 / 失败语义；Q2 前置改"**G1 两半全部闭合**" |
| B7 | P1 | **FeetechAdapter 事实写错**：adapter **有** `set_mode()` 且确实写 `Operating_Mode`（`adapter.rs:603-633`）；真问题 = 启用链从不调用它 + 现有 set_mode 无断扭/解锁/写入/readback 事务（裸寄存器写循环） | §5.1 事实改准 + 登记 config 注释（"不触碰 Operating_Mode"句）为陈旧勘误对象；风险结论保留 |
| B8 | P1 | **Gripper (b) "1:1" 过度宣称**：dimos `set_gripper_position` 同步返回实际 adapter 写结果；publish/弱 Ack 只证 publish，不证 S0 消费与硬件写入 | §2.2 语义收益拆 **(b1) 强 RR / (b2) publish 弱 Ack（非 1:1）**；§2.3 行拆分；Q3 改"须同时拍路由与 Ack 语义" |

非阻断同步 ×2：M4 "唯一硬缺口"→"唯一 **RPC/method** 硬缺口"；§5.1 "现有三款"→"现有可组成 E 双臂权威配置的方案"。

### round 3（reviewer 复核后）

reviewer 2 阻断（P0 ×1 + P1 ×1），已回修：

| # | 级别 | 阻断 | 修订 |
|---|---|---|---|
| B9 | P0 | **Q10 缺 S0 command-side 防错位闭环**：S0 解析 `JointCommand` 时**完全丢弃** `fb.layout_id()`、按数组索引直写（`ipc_thread.rs:1177-1192` `parse_joint_command` 无任何 layout 比对）——S1 配置错序仍可驱动错误电机，违反 design §3.5.1 "未知 layout 不得解析数组"（`00_design.md:524`） | Q10 增**两方案共同硬约束**：S0 非 RT IPC 路径比对 command layout_id 与本机权威 ID，不一致丢弃、不写 `LatestCommandSlot` + metric/state/失败语义；(a) 哈希明确 **joint_names + joint_ids + order** 全入、S1 由 config 派生或对显式 CLI 值 fail-fast（撤"对齐"模糊词） |
| B10 | P1 | **Q12 未登记共享 anchor_bias 对 HoldPose 的影响**：`ActionContext.anchor_bias` 同时是 HoldPose **每 tick 真实输出目标**（`hold_pose.rs:117-121`，每 tick 读共享值；注释留"扩动态 anchor 扩展点"但语义变更未披露）——动态化共享 anchor = HoldPose 从"保持启动锚点"变"跟随动态值" | Q12 增**执行边界**：优先拆分 `scripted_start_pose` / `idle_hold_target` 与启动 anchor_bias（HoldPose 不动）；若动共享 anchor，须显式拍 HoldPose 新语义 + 回归 AC；§1.2 G1 段登记共享面 |

---

## 0. 冻结基线执行面剖面（映射的"源"语义）

### 0.1 executor 一圈（12 维 → 双臂 trajectory + 双夹爪）

- **12 维布局**（`src/xlerobot_dimos/policy/executor.py:39-52`，`ACTION_DIM_TO_JOINT`）：
  idx 0-4 = `left_arm_{shoulder_pan,shoulder_lift,elbow_flex,wrist_flex,wrist_roll}`，
  **idx 5 = `left_arm_gripper`**；idx 6-10 = `right_arm_*` 同构，**idx 11 =
  `right_arm_gripper`**。维度守卫 `<12 → ValueError`（:59-60）。
- **单位转换**（:61-68）：臂 `度 × π/180 → rad`；夹爪 `[0,100] / 100 → [0,1]`。
  观测端严格互逆（`observation.py:124-132`）。
- **execute_step**（`executor.py:72-119`）每步：
  1. `current = client.get_joint_positions()`（rad dict，**不含夹爪**，见 §0.2）；
     异常 → warning + **整步跳过**（:91-93）。
  2. 每臂一条 **两点 JointTrajectory**：`points = [当前位形 @t=0, 目标 @t=dt_s]`
     （dt_s=0.1，:96-105），`task_invoke("traj_left_arm"/"traj_right_arm",
     "execute", {"trajectory": traj})`（:107）。task 的 joint_names = 每臂
     **5 个臂关节，不含 gripper**（:23-36）。
  3. 夹爪独立 RPC：`set_gripper_position("bus1"/"bus2_arm", [0,1] 值)`（:111-119）。
- **调用频率 = 约 10 Hz 持续覆盖，不等完成**：policy 主循环每
  步 `execute_step + sleep(dt_s)`，`steps_per_chunk=10` 步后回外层重新推理
  （`policy_task.py:80-84,103,106`）。新轨迹**抢占覆盖**上一条（§0.2）。
- 并存实现 ×2 语义一致：独立进程版 `executor.py:72` + Module 版
  `policy_module.py:319`（注释明写"复制自 executor.execute_step"；差异仅当前
  位形优先取 JointState stream 缓存，:324-334）。

### 0.2 调用面底层语义（dimos 框架侧）

- 传输 = **LCM 同步阻塞 RPC**（`dimos/core/rpc_client.py:69`，
  `call_sync("ControlCoordinator/<method>", …)`）。
- **`get_joint_positions()`**（`dimos/control/coordinator.py:439-448`）：遍历
  hardware 实时 serial `sync_read` → `{joint_name: rad}`。**只含
  `component.joints`（臂+头）**；blueprint 未设 `gripper_joints` → **永远不含
  夹爪**（`dimos/control/components.py:59-64`）。夹爪读走**另一条 RPC**
  `get_gripper_position(hw_id)` → `[0,1]`（`coordinator.py:629-642`；观测/录制
  /web 三处都用：`observation.py:159-170`、`policy_module.py:297-308`、
  `web/server.py:631-639`）。
- **`task_invoke(task, "execute", {trajectory})`**：提交即返回；已在执行则
  log "Preempting active trajectory" 后**覆盖（非排队）**
  （`dimos/control/tasks/trajectory_task.py:178-211`）。实际运动由 coordinator
  **100 Hz tick loop** 播放：`trajectory.sample(t)` 分段线性插值，
  `t >= duration` 转 COMPLETED 并 hold 末点（:120-160）。duration = max
  `time_from_start` = 0.1s。
- **`set_gripper_position(hw_id, val)`**（`coordinator.py:611-627`）：
  `adapter.write_gripper_position` → clamp [0,1] → `raw = lo + v*(hi-lo)`
  （lo/hi = 校准 range）→ serial 直写 `Goal_Position`
  （`_manipulator_base.py:546-586`）。**绕开 tick loop / trajectory 仲裁**，
  与臂轨迹天然并行；要求 `_enabled`。
- **JointTrajectory / TrajectoryPoint**（`dimos/msgs/trajectory_msgs/`）：
  `TrajectoryPoint{time_from_start:s, positions:[rad], velocities(缺省 0),
  num_joints(派生)}`；`JointTrajectory{timestamp(缺省 now), points,
  joint_names(可选), duration(派生 = max tfs)}`；`sample(t)` 分段线性
  （`JointTrajectory.py:29-119`）。

### 0.3 硬件 / 标定边界

- **blueprint**（`src/xlerobot_dimos/blueprints/coordinator_xlerobot_full.py:42-106`）：
  tick 100 Hz；3 hardware：`bus1`（`/dev/ttyACM0`，左臂5+头2）/ `bus2_arm`
  （`/dev/ttyACM1`，右臂5）/ `bus2_base`（同 ttyACM1 共享，三轮）；4 task：
  `traj_left_arm` / `head_position` / `traj_right_arm` / `vel_base`
  （**无 `traj_supply_arm`**——历史文档若提到即勘误对象）。
- **关节 → motor id**（`src/xlerobot_dimos/common/config.py:112-180`）：每总线
  臂 = 1-5，**夹爪 = 6**，bus1 头 = 7,8，bus2 轮 = 8,7,9。夹爪
  "通过 read/write_gripper_position 访问，不计入 dof"（`adapters/bus1.py:8`）。
- **单位换算权威**：臂 STS3215 `raw↔rad`：4096 tick/rev、center 2047、
  `(raw-2047)×2π/4096×direction + zero_offset_rad`（`units.py:31-50`）；夹爪
  `raw↔[0,1] = (raw-lo)/(hi-lo)`（**不走 units.py**，
  `_manipulator_base.py:534-554`）。标定 JSON
  `{motor: {id, drive_mode, homing_offset, range_min, range_max}}`
  （`calibration.py:4-14`），目录指向
  `/home/loosqk/repo/dimensional-applications/calibration`（`config.py:21`，
  本机不存在、在 Orin）——**与本仓 configs 引用的是同一批文件**（§4.2）。

### 0.4 范围边界实测确认（D-BI0 排除项）

- **policy 推理不是本地 ONNX**：`openpi_client.WebsocketClientPolicy` 连远端
  policy server（默认 `10.0.1.81:8000`，真模型跑远端 5090；
  `policy_task.py:56-69`、`policy_module.py:143-232`）——排除项"policy 推理"
  实体 = websocket 客户端 + observation 打包（`observation.py`），均不在 E。
- **web ArmIK 3 处**：`web/server.py:289 / :337 / :369`（遥操作面，不在 E）。
- **head / base 不被 policy 驱动**（12 维无 head/base），由 web 面用
  `head_position` / `vel_base` task 驱动——不在 E 验收环，但 head 关节**物理上
  在 bus1 硬件里**，对 §2 方案 (a) 有直接影响（Q5）。

---

## 1. 勘探项 ①：对照移植清单——调用面 → 10 RPC 映射

> 前提（D-BI0 r1）：**核对覆盖并闭环缺口，不预设 10 RPC 足够**。执行面 3 个
> call site 之外，把观测链的夹爪读（同属"读状态"验收步）一并列入。

### 1.1 映射总表

| # | dimos 调用面 | robot-system 映射 | 判定 |
|---|---|---|---|
| M1 | `get_joint_positions()` → `{joint: rad}`（不含夹爪） | `GetState(hardware_id)` → `RobotState.position_rad`（`robot_control.proto:444`；源 = S0 `JointState.position_rad`，`joint.fbs:103`） | 🟡 **向量载体存在**（含 gripper 位，见 M2）；**name→index 待 Q10 闭环**（round 2 B5：wire 无关节名 + layout_id 现为占位常量） |
| M2 | `get_gripper_position(hw_id)` → `[0,1]`（观测链） | 同 `GetState`——gripper 就是 `position_rad[i]` 的一项（`configs/xlerobot_task29_right_gripper.toml:21-30`），**单位 rad 非 [0,1]** | 🟡 **向量载体存在**（同 M1，待 Q10）；单位差 rad vs [0,1]（§3/Q8） |
| M3 | `task_invoke(traj_*_arm, "execute", 2 点 0.1s 轨迹)` ×2 臂，10 Hz 抢占覆盖 | `InvokeTask(Scripted{waypoints})`（`scripted_payload.fbs:37-47`） | 🟠 **未闭环**（round 1 B2）：四重语义差（§1.2），第 4 条 G1 = P0 级 S1 语义缺口 |
| M4 | `set_gripper_position(hw_id, [0,1])`，绕仲裁直写 | **无对应 RPC**（10 RPC 无 gripper 下发） | ❌ **唯一 RPC/method 硬缺口** → §2 两方案 |

**核对结论（round 1 B2 + round 2 B5 修订）**：M1/M2 **向量载体存在**（读面
载体完整，name→index 待 Q10 闭环）；M3 **载体存在但映射未闭环**（四重语义
差，第 4 条 G1 为 P0 级）；**RPC/method 面缺口 = M4 夹爪下发；S1 语义缺口 =
G1（anchor/hold）；名序权威缺口 = Q10**。"10 RPC 够用"在字面调用面上
**不成立**（M4）；且即便补齐 M4，连续运动仍压在 G1 与 Q10 上。

### 1.2 M3 的四重语义差（Spec 必答的真问题；第 4 条 = P0 缺口 G1）

1. **关节子集 vs 全向量**：dimos trajectory 按 `joint_names` 驱动**臂 5 关节
   子集**（head/gripper 不在内）；本仓 `ScriptedPayload.waypoints[].positions`
   **长度必须 == 该 hardware 全部 joints.len()，按索引对位**
   （`scripted_payload.fbs:8-10` + `scripted.rs:123-209`），无按名子集机制。
   Orin bus1 hardware = 8 joint（臂5+夹爪+头2，
   `configs/xlerobot_bus1_full.toml:43-65`）→ 波形必须给全 8 位，非目标关节
   填当前位（来源 = GetState）。
2. **抢占覆盖 vs 串行拒绝**：dimos `execute` 对执行中任务**抢占覆盖**
   （`trajectory_task.py:200-201`）；本仓 coordinator **单 action 串行**：
   Executing 时再 submit → `BusyExecuting` → tonic `AlreadyExists`
   （`coordinator.rs:1231-1235,1498-1580` + `task_handlers.rs:426`），**无抢占
   语义**，只有显式 `CancelTask`（cancel → 下 tick `PartialSuccess` 终态，
   `scripted.rs:418-422`）。10 Hz 逐步覆盖模式直接平移会撞 AlreadyExists 墙。
   **缓解事实**：dimos 每次推理产出 `steps_per_chunk=10` 步 ×0.1s——天然可折叠
   为**一条 10-waypoint ScriptedPayload（每段 segment_duration_ms=100）**，把
   "10 Hz 抢占"降为"~1 Hz 每 chunk 一个 action"；chunk 间衔接仍需
   Cancel+Invoke 或等待终态（Q2；**两选项在 G1 修复前都不构成连续运动**——
   chunk 间隙 Idle hold 会回拉，见第 4 条）。byte 预算充足：4 KiB gate
   （`task_handlers.rs:98`）下 8-joint waypoint ≈ 每点 ~80B 量级，10 点远低于
   上限。
3. **双臂并发 vs 单 robot_id 单 action**：dimos 单 coordinator 内左右臂两条
   trajectory task 并行 tick；本仓 `InvokeTask` **无 hardware_id**
   （`robot_control.proto:270-277`），s1_daemon 绑定**单 hardware**
   （`s1_daemon.rs:144-146,302-309`），coordinator 单 action——**同一 robot_id
   下双臂无法并发**。现架构唯一双臂形态 = **双 robot_id 拓扑**（每 bus 一套
   s0+s1，topic 空间独立；`configs/xlerobot_task29_right_gripper.toml:6-11`
   明文先例 + Orin 双 S0 unit 现状 §4.2）。连带事实：gateway 亦单
   `--robot-id`（`robot_rpc_gateway.rs:155` 必填）→ 双臂 = **双 gateway 双
   端口**（Q1）。
4. **首点 / anchor 语义（G1，P0 级独立缺口——M3 未闭环的根，round 1 B2）**：
   dimos 每步轨迹显式带"当前位形 @t=0"首点（§0.1）；本仓 Scripted
   **segment0 的 from = `ctx.anchor_bias`**，而 anchor_bias 是 **S1 启动期
   一次性读取、构造后不再变**的启动锚定位（`scripted.rs:186` 注释明文"启动期
   initial pose 来源，不作 tick 持续动态 target" + `coordinator.rs:1437`
   "构造期传入，不再变"）。更根本：action 完成后 coordinator 回 Idle，
   **Idle/Degraded 的 hold cmd target 也 = anchor_bias**
   （`coordinator.rs:865-871`）——连续 chunk 场景下，chunk 间隙 S1 把臂
   **回拉到启动姿态**，且下一条 Scripted 从启动 anchor 起插值（当前位 ≠
   启动位即跳变）。**修复必然触 S1 语义**（per-action anchor 刷新 /
   hold-last-commanded / Idle hold 策略），不是 client 侧转换能绕过的；且
   S1 今日**无当前位来源**——启动锚定 subscriber 在 `anchor_or_fail` 函数内
   建、读完即弃（`s1_daemon.rs:783-812`），coordinator 亦无动态 state cache
   / last-commanded 权威（round 2 B6）→ **Q12**。共享面注意（round 3 B10）：
   `anchor_bias` 同时是 **HoldPose 每 tick 输出目标**
   （`hold_pose.rs:117-121`）——修复若动共享 anchor 即牵连 HoldPose 语义
   （Q12 执行边界）。附加约束：Idle hold ≡ B1
   路径是 v0.3 reviewer 阻断锁定的等价语义（`coordinator.rs:866-868` 注释），
   改动须 Spec 显式拍板解锁。

### 1.3 M1/M2 的次要差异（登记不阻塞）

- `GetState` 按 hardware 一次一读 → 双臂读 = 2 次调用（或 `StreamState` ×2）；
  dimos 一次 RPC 全量。无功能缺口。
- fbs `JointState` 有 `velocity_rps/effort_nm`（`joint.fbs:102-116`）但 proto
  `RobotState` **只镜像 position_rad**（`state_convert.rs` 只透传 position +
  4 enum + fault_code）——执行环不需要（dimos 观测也只用 position），登记为
  已知面不动。
- 读 RPC 无 auth（C1 延续，非本候选修）。

---

## 2. 勘探项 ②：gripper 两方案比较（只比较，不拍板）

### 2.1 方案 (a) 复用 InvokeTask/Scripted 的事实基础

- **S0 眼里 gripper = 普通 joint，零特判**：同一条
  `rad_to_raw + calibration clamp` 写路径（`adapter.rs:561-601`），值域仅由
  calibration `range_min/max` 决定（task29 实测 raw[1384,2847] ≈
  rad[-1.02,+1.23]，`xlerobot_task29_right_gripper.toml:13-15`）。gripper 已在
  bus 级 hardware 的 joints 数组内（`xlerobot_bus1_full.toml` 8 joint 含
  `left_arm_gripper`）。
- **形态**：夹爪目标作为全向量 waypoint 的第 i 位随臂轨迹**同车下发**——M3+M4
  合并成同一条 Scripted；或单独动夹爪时发"其余关节填当前位"的 1-waypoint
  Scripted。
- **代价 / 语义落差**：
  - dimos 夹爪**绕仲裁直写、与臂轨迹并行**；(a) 下夹爪动作与臂 action 在同一
    coordinator 串行化——单独夹爪 Scripted 会被执行中的臂轨迹顶回
    `AlreadyExists`（§1.2-2）。**同车下发**则无此问题（chunk 内夹爪随波形走）。
  - 单位：`[0,1] → rad` 换算必须在 client/adapter 层做（§3，Q8）。
  - 0 新 method / 0 新 schema / 0 S0 改动；**但依赖 G1 修复 = S1 语义改动
    （Q12）**——G1 对一切 Scripted 连续运动共通、非 (a) 独有，然而 (a) 原
    "纯 client 侧表达 0 S1 改动"宣称**撤回**（round 1 B2）。
- **权限现状**：`invoke_task = operator+`（`configs/rpc_roles.toml:54-59`）。

### 2.2 方案 (b) 新增 Gripper RPC 的事实基础

- **wire 成本（round 1 B4 修订）**：frozen proto 只可 append（entry §5 禁止
  项 #4 + 2026-07-13 核对标注）——service +1 method。**若绕 action 槽走 S0
  RR**（与 dimos 直写并行语义对齐的形态），需 **request/response 成对内部
  schema**——沿 SetEnabled `0x040D/0x040E` 成对先例（`schema_id.rs:110-122`），
  **0x040F 只是起点（+0x0410）**，各自走 6 步注册链 atomic
  （`v0.4_scripted_backend_summary.md:26-44`：fbs table → build.rs 数组 →
  schema_id → BodyMarker → SizeTier → build_helpers）；除非另拍
  **publish/弱 Ack 路由**（沿 EStop route (a) 先例，无 RR 回执）——**不能按
  单 schema 估算**。+ rpc_roles capability 新条目。
- **真实成本（深层，B/C/D 量级）**：S0 command 面现状是**全向量位置命令**
  （`JointCommand.target:[double]`，`joint.fbs:119-127`）——"直写单个夹爪"在
  S0 侧**没有现成通路**，需要新 control RR（沿 SetEnabled 第 2 rr_server 先例）
  或部分向量命令语义，外加 gateway rr_io_thread、e2e、idempotency method 注册。
  即 (b) 不是"proto append 一下"，是一个完整 additive 候选。
- **语义收益（round 2 B8 拆分）**："不占 action 槽、不受臂轨迹串行化影响"
  两条路由皆成立；但 **Ack 语义分岔**——dimos `set_gripper_position` 是同步
  RPC、**返回即代表 adapter 串口写已执行**（§0.2）：
  - **(b1) 强 RR 路径**：response 可携带写结果 → 与 dimos "写返回"语义对齐；
  - **(b2) publish/弱 Ack 路径**：只证 publish、**不证 S0 消费与硬件写入**
    （EStop D2 弱 Ack 先例同款诚实边界）→ **非 1:1**。
  单位可在 RPC 语义里定为 [0,1] 归一化（与 dimos client 面对齐）。

### 2.3 比较表（事实汇总）

| 维度 | (a) 复用 Scripted | (b) 新增 Gripper RPC |
|---|---|---|
| wire 改动 | 0 | +1 method（append）+ S0 RR 成对 schema（0x040F/0x0410）**或** publish/弱 Ack 路由 + capability（B4） |
| S0 改动 | 0 | 新 RR server 或部分命令语义（无现成通路） |
| S1 改动 | 0（**但共通前置 G1 = S1 语义修复**，Q12） | 0（若走 S0 direct）或 action 化（另议）；臂侧连续运动同受 G1 前置 |
| 与 dimos 并行/Ack 语义 | 🟡 同车下发等价；独立下发受单 action 串行限制 | (b1) 强 RR：并行 + "写返回"均对齐 / (b2) publish 弱 Ack：并行对齐但只证 publish，**非 1:1**（round 2 B8） |
| 单位 | client 层 [0,1]→rad 换算 | 可在 RPC 定义 [0,1] |
| 与验收环契合 | "双臂动作 + 双夹爪"一条 chunk 载荷即达成 | 需三路调用编排 |
| 工程量 | client/adapter 侧小 | B/C/D 级 additive 候选 |

→ **Spec 拍板 Q3**；本 CodeMap 不给结论。

---

## 3. 勘探项 ③：JointTrajectory → ScriptedPayload adapter 定位

### 3.1 字段映射表（源 §0.2 ↔ 靶 B.4/B.5 事实）

| JointTrajectory（dimos） | ScriptedPayload（本仓） | 转换规则 |
|---|---|---|
| `joint_names` + 子集 positions | 全 hardware 向量按索引 | **name→index 解析无 wire 权威源（round 1 B3）**：`RobotState` 只有 `layout_id + position_rad`、无关节名（`robot_control.proto:439-450`）；`JointLayout`（names/joint_ids，`joint.fbs:89-95`）注释称"低频广播"但**实测 S0 今日不 publish、RPC 亦未暴露**（robot-s0 仅 `pub_thread.rs:25-27` 占位 layout_id），layout_id 不可逆推顺序。**禁止裸固定索引**——来源选项 = Q10；缺位填充 = Q5（受 Q11 约束） |
| `points[i].positions`（rad） | `waypoints[i].positions`（rad） | 单位同为 rad，直传；夹爪若并入需 [0,1]→rad |
| `points[i].time_from_start`（s，绝对） | `segment_duration_ms`（相对，>0） | 差分：`(tfs[i]-tfs[i-1])×1000` |
| `points[0] = 当前位形 @t=0` | **无承载**——segment0 from = S1 **启动期** `anchor_bias`（非当前位，G1；`scripted.rs:186`） | **不可丢弃（round 1 B2）**：现架构无"从当前位起插值"语义；依赖 G1 修复（Q12）后再定丢弃或显式承载 |
| `velocities`（缺省 0） | 无字段 | 丢弃（线性插值语义两侧一致：dimos `sample()` 亦线性） |
| `timestamp` | 无 | 丢弃 |
| — | `dwell_ms` | 新增维度，映射置 0 |

插值语义对齐事实：两侧都是**分段线性**（dimos `JointTrajectory.py:74-119` /
本仓 `scripted.rs:369-378`）；播放时钟不同（dimos coordinator 100 Hz tick vs
本仓 S1 tick 推进 + S0 50 Hz 执行）。

### 3.2 候选位置（D-BI3：归属 robot-system 侧已定，形态 = 本 Q）

| 选项 | 事实基础 | 张力 |
|---|---|---|
| (i) robotctl `invoke` 子命令扩展 | `invoke scripted` 已存在（`dispatch.rs:78`），加"轨迹 JSON→payload"输入面 | robotctl 已定位**辅助入口非验收主口径**（D-BI2 r1） |
| (ii) e2e/验收 harness helper（robot-rpc tests common 或 robot-tests lib） | 自动 gate 直接消费；`case_ctx.rs` 先例 | 生产不可复用（test-only 代码位） |
| (iii) robot-rpc client 侧小模块（lib 内 helpers） | task_rpc_e2e 已用 client helpers；对 (i)(ii) 都可复用 | 给 robot-rpc 加"非 wire"职责 |
| (iv) 推迟到 robot-s2-bridge | D-BI1(i) 的 Python 入口宿主 | E 验收期就需要转换（基线数据是 JointTrajectory 形状） |

→ **Spec 拍板 Q4**。注意消费者事实：E 期间 adapter 的消费者只有**验收/测试
面**（policy 循环不在 E）；真正的运行时消费者出现在 post-E 的 S2 接入。

---

## 4. 勘探项 ④：Orin 双 gate 部署路径

### 4.1 自动 gate（Rust cross-process e2e）现状与延伸面

- **CI 实体**（`.github/workflows/ci.yml`）：fmt → clippy → `cargo test
  --workspace --all-features`（**含 `crates/robot-rpc/tests/` 4 个跨进程 e2e**）
  → mock 保底 → release build → matrix-build → rt_baseline 30s → deny。
  **6 个 required-features smoke bin 不进 CI**（人工/本地/Orin 跑）——"自动
  gate"若要真自动，落点 = `robot-rpc/tests/`（或解除 smoke 守门，Q6）。
- **harness 三先例拓扑**（`tests/common/case_ctx.rs:105-184` +
  `ready.rs:71-100`）：
  task_rpc_e2e = FakeState + 真 s1 + 真 gateway；set_enabled_e2e = **真 s0** +
  FakeHeartbeat + 真 gateway（无 s1）；state_rpc_e2e = 仅 gateway + FakeState。
  **E workflow e2e 需要的"真 s0 + 真 s1 + 真 gateway"三真同堂尚无先例**
  （s1 启动锚定依赖 s0 state，readiness 链已有成熟模式：marker → TCP → RPC
  probe）。
- 双臂验收环在 e2e 里 = 双 robot_id 双栈（§1.2-3），mock adapter 即可跑
  （`robot-s0` mock feature），不依赖真机。

### 4.2 真机完结 gate 路径（"读状态 → 双臂动作 → 双夹爪"）

**已有的**：
- **S0 systemd unit ×2 已存在且已对旧栈互斥**：
  `scripts/systemd/xlerobot-s0-bus{1,2}.service`，`Type=notify` +
  `WatchdogSec=2s` + **`Conflicts=xlerobot-coordinator.service
  xlerobot-web.service`**（bus2:95 / bus1:39）——与 D-BI3 退役顺序锁天然一致。
- **真机 config ×2 已存在**：`configs/xlerobot_bus{1,2}_full.toml`（串口
  **by-id 稳定路径**；8/9 joint 含 gripper；`calibration_paths` 指向
  `/home/loosqk/repo/dimensional-applications/calibration/*.json`——**与 dimos
  同一批标定文件**，homing_offset/range 语义两栈一致，§0.3）。
- S0 watchdog/notify 链（`systemd_notify.rs:30-67`）+ freeze_systemd_smoke。
- 构建惯例：**Orin 本地 build，无交叉编译**（`2026-04-26_bus2_runbook.md:104`；
  feature `xlerobot-real = ["xlerobot", "robot-adapter-feetech/real-hw"]`，
  `robot-s0/Cargo.toml:15`）。

**缺的（E 部署段工作清单，事实性缺口）**：
- **S1 / gateway 无 systemd unit**（`scripts/systemd/README.md:189-190` 明文
  推迟至相应 milestone）。
- **无 deploy.sh**（全仓零命中；全局 CLAUDE.md §9 的打包入口未落地）。
- v0.2 真机 runbook ×2 均带 **"pre-strict 历史档案不可照跑"banner**
  （A1-5 后 `rm latch + restart ≠ ClearEStop`）——E 需要新 runbook。
- robotctl 无 get-state / set-enabled / estop-RPC / gripper 子命令
  （D-BI2 已定位辅助入口；是否补齐 = Q7 的一部分）。

**旧栈退役顺序锁的实施对象（实测）**：
- 旧栈 = **system unit**（`/etc/systemd/system`，`WantedBy=multi-user.target`，
  `User=loosqk`；`dimos-deploy/systemd/xlerobot-{coordinator,web}.service` +
  `install_services.sh:19-35`）。**ExecStart 指向
  `/home/loosqk/repo/dimensional-applications`**（非 dimos-deploy——冻结快照仓
  ≠ Orin 部署树，退役操作对象是后者的 unit）。
- 占用硬件 = `/dev/ttyACM0` + `/dev/ttyACM1`（与本仓同两根物理串口：
  bus1=5A7C121427、bus2=5A7C122340，本仓用 by-id 名）+ RealSense ×3（本仓无
  相机 adapter，E 不接管）。
- 验收前操作 = `sudo systemctl stop xlerobot-coordinator xlerobot-web`
  （**不 disable、不删 unit**；回滚 = `systemctl start` 原样恢复）；
  S0 unit 的 `Conflicts=` 提供第二道互斥。**正式退役（disable）仅在完结 gate
  通过后**（D-BI3）。

**验收环拓扑张力**：真机"双臂动作"按现架构 = 2×（s0+s1）+ gateway 的组合；
gateway 单 `--robot-id` → 双栈双 gateway 双端口（每端口 loopback-only），或
gateway 侧扩多 robot（架构改动）→ **Q1**。夹爪与 head 关节在 bus 级 hardware
向量内的处理 → **Q3/Q5**；**权威 config 方案（底盘 mode 风险）→ Q11**；
连续动作 anchor/hold（G1）→ **Q12**。

---

## 5. 跨面系统性发现（登记）

1. **E 权威 config 现状 = 现有可组成 E 双臂权威配置的方案皆不可直接用
   （round 1 B1 修订 + round 2 非阻断收窄，原"bus_full 才对"表述撤回）**：
   - `xlerobot.toml` 陈旧：每臂 5 joint（4 臂关节+夹爪，`xlerobot.toml:16-49`）
     与 12 维基线（每臂 5 臂关节+1 夹爪）不符。
   - `xlerobot_bus{1,2}_full.toml` 电机表虽与 blueprint 对齐（8/9 motor），
     **但把底盘轮编入 S0 hardware 向量 = P0 失控风险**：bus2 轮可能遗留
     dimos 时代 `Operating_Mode=1`（wheel/velocity 模式，dimos stop 不切回，
     电机自持上次设置），此时 `Goal_Position` 被解释为 `Goal_Speed`——
     "填当前位"= 持续速度命令 = 整车移动（config 自带必读警告
     `xlerobot_bus2_full.toml:49-58`）。
   - **adapter 侧事实（round 2 B7 改准）**：`FeetechAdapter` **有**
     `set_mode()` 且确实写 `Operating_Mode` 寄存器（`adapter.rs:603-633`：
     Position/ServoPosition→0、Velocity→1，逐 joint 裸写循环）。真实缺口
     有二：**(1)** 启用链 Ready→Enabled 仅 Effectful `Enable(true)`、
     **从不调用 set_mode**（`state_machine.rs:367-374`）；**(2)** 现有
     `set_mode()` **没有**断扭、解锁、写入、readback 的完整事务。附带勘误
     登记：`xlerobot_bus2_full.toml` 警告块内"FeetechAdapter 只写
     Goal_Position、不触碰 Operating_Mode"一句系**陈旧表述**（与
     `adapter.rs:603` 不符），该 config 注释是 E Spec 期勘误对象。
   - **配置方案比较（→ Q11）**：
     **(i) arm-only 6-motor per bus**——右臂已有
     `configs/xlerobot_bus2_right_arm_full.toml`（ID 1-6 = 5 臂 + 夹爪）；
     **左臂缺对应 config**（bus1 现仅 8-joint full 含头），需新增；底盘/头
     不进向量 → mode 风险与"非目标关节填充"大半问题直接消失。
     **(ii) full config（8/9 joint）**——若坚持，Q5/Q9 **必须锁**底盘 mode
     的断扭、解锁、写入、readback fail-fast 全链。
   E 的权威 config 由 Spec 从冻结 blueprint derive + Q11 拍板产出。
2. **单位三方约定**：dimos client 面夹爪 [0,1] / 本仓 wire 与 S0 全程 rad /
   电机 raw tick（标定 range）。换算住哪层（§3/Q8）决定"[0,1]"这个词在 E 里
   还存不存在。
3. **`traj_supply_arm` 不存在**（blueprint 仅 4 task）——凡历史文档引用即勘误
   对象。
4. **两份 executor 并存**（独立进程版 + Module 版）语义一致——移植对照以
   `executor.py` 为准即可覆盖两者。
5. 观测链的夹爪读（`get_gripper_position`）与执行链的夹爪写各自独立于
   trajectory——本仓 GetState 一次读全向量天然合并读面（M2，🟡 载体存在、
   name→index 待 Q10 闭环）。

---

## 6. Spec 拍板 Q 表（Pre-Research 出口 → Spec round 1 入口）

| # | 问题 | 选项（事实基础见节） |
|---|---|---|
| Q1 | 双臂拓扑 | (a) 双 robot_id 双栈 + 双 gateway（0 架构改动，先例 task29 + Orin 双 S0 unit；client 侧编排两连接）/ (b) gateway 或 s1 多 hardware 化（架构改动，触 §1.2-3 全链） |
| Q2 | 10 Hz 抢占语义映射（**前置 = G1 两半全部闭合**〔action 起点 + Idle 回拉，即 Q12(c) 或等价统一方案〕：任一半未修，下列任一选项都不构成连续运动，round 2 B6） | (a) per-chunk 折叠：一次推理 = 一条 10-waypoint Scripted（1 Hz action 粒度；chunk 间 Cancel+Invoke 或等终态）/ (b) 每步 Cancel+Invoke 抢占对（10 Hz RPC ×2，竞态面）/ (c) 新增抢占/streaming 语义（架构改动，默认不选） |
| Q3 | gripper 方案（**含路由与 Ack 语义拆分**，round 2 B8） | (a) 复用 Scripted 全向量同车（§2.1）/ (b) 新增 Gripper RPC（§2.2，B/C/D 级）——**选 (b) 须同时拍**：(b1) 强 RR（成对 schema，response 携写结果，与 dimos "写返回"对齐）或 (b2) publish/弱 Ack（仅证 publish，非 1:1）——比较表 §2.3 |
| Q4 | adapter 形态/位置 | §3.2 (i)-(iv) |
| Q5 | 非目标关节填充策略（round 1 B1 收窄：**仅对 position-mode 关节成立**，受 Q11 约束） | head/臂填 GetState 当前位（波形不动语义）+ 读写间隙漂移容差归属（Spec AC）。**底盘轮不得按此处理**——Operating_Mode=1 残留下"填当前位"= 速度命令（§5.1）：Q11(i) 下轮子不在向量、问题消失；Q11(ii) 下依赖 Q9 mode 硬化链 |
| Q6 | E workflow e2e 放置 | (a) `robot-rpc/tests/`（CI 自动跑，沿 4 e2e 先例）/ (b) robot-tests smoke bin（required-features，人工）/ (c) 双轨：e2e 进 CI + 真机 smoke bin 复用同 case 库 |
| Q7 | 真机完结 gate 驱动方式 | (a) 专用验收 binary（沿 smoke 先例，脚本化"读状态→双臂→双夹爪"）/ (b) robotctl 补 get-state 等子命令后人工驱动 / (c) a+b（binary 为主 robotctl 为证据面） |
| Q8 | 夹爪 [0,1]↔rad 换算权威层 | (a) client/adapter 层换算，wire 全程 rad（S0 0 改）/ (b) 若 Q3 选新增 RPC，RPC 语义定 [0,1]（S0 侧需换算） |
| Q9 | S1/gateway systemd unit + 新 runbook | E 部署段范围确认（unit ×3~4 + runbook 替代 pre-strict 档案；deploy.sh 是否一并落）。**若 Q11 选 (ii) full config，必须含底盘 mode 断扭 / 解锁 / 写入 / readback fail-fast 全链**（round 1 B1） |
| Q10 | joint 名→index 权威源（**禁止裸固定索引**，round 1 B3 + round 2 B5 重写） | **现状三事实（(a) 原样不可执行的原因）**：S0 wire `layout_id` = 固定占位 `0xCAFE_BABE_DEAD_BEEF`（`s0_daemon.rs:858`）；S1 `--layout-id` 独立传入、无一致性闭环；唯一计算函数 = 私有 lint helper 且 **sorted names 顺序无关**哈希（`schema_lint.rs:190`，证不了顺序），未实现 design §3.5.1 的 `sorted_joint_ids + order` 入哈希与 1 Hz `JointLayout` 广播/RR（`docs/00_design.md:522`）。**选项**：(a) TOML 名序权威 + **补齐 layout_id 一致性链**——真实计算（**joint_names + joint_ids + order 全部入哈希**）→ S0 撤占位 → S1 layout_id **由 config 派生**、显式 `--layout-id` 与派生值不一致即 **fail-fast**（round 3 B9 撤"对齐"模糊词）→ client 校验（= design §3.5.1 部分落地，工作量非零）/ (b) 新增 layout 可观测面（S0 广播 `JointLayout` 或 gateway layout 查询，additive）。**两方案共同硬约束（round 3 B9）**：S0 须在**非 RT IPC 路径**（parse / 写 slot 之前）比对 `JointCommand.layout_id` 与本机权威 ID——不一致则**丢弃、不写 `LatestCommandSlot`**，并定义 metric（§3.5.1 `unknown_layout_count`）/ state / 失败升级语义（DEGRADED 阈值）；现状缺口 = `parse_joint_command` **完全丢弃** `fb.layout_id()`、按索引直写（`ipc_thread.rs:1177-1192`），S1 错序配置可驱动错误电机，违反 design §3.5.1 "未知 layout 不得解析数组"（`00_design.md:524`）。**任一闭环前 M1/M2 停留"向量载体存在"** |
| Q11 | E 权威 config 方案（round 1 B1，P0） | (i) arm-only 6-motor × 2 bus（右臂已有 `xlerobot_bus2_right_arm_full.toml`，**左臂 config 需新增**；底盘/头出向量，mode 风险消失）/ (ii) full 8/9 joint + 底盘 mode 硬化全链（与 Q5/Q9 联锁） |
| Q12 | **G1**：S1 anchor/hold 语义缺口修复方向（round 1 B2 + round 2 B6 重写，P0） | **(a)/(b) 均为局部机制、非完整终态**：(a) per-action anchor 刷新只修 action 起点，Idle 仍回拉；(b) hold-last-commanded 只修 Idle 回拉，新 action 仍从启动 anchor 起插值；**闭合 G1 = (c) 两半兼修或等价统一方案**。**必答子项**：① "提交时当前位"来源——S1 今日**无**常驻 state 订阅（anchor subscriber 在 `anchor_or_fail` 函数内建、读完即弃，`s1_daemon.rs:783-812`），coordinator 亦无动态 state cache / last-commanded 权威，须新建其一；② 该来源的 freshness（stale 阈值）+ layout 校验（与 Q10 联动）；③ 取不到/过期时的失败语义（拒 start / 降级 / fallback 到何值）。**约束**：Idle hold ≡ B1 等价路径是 v0.3 reviewer 阻断锁定的语义（`coordinator.rs:866-868`），任一改动须 Spec 显式拍板解锁 + e2e 锁新行为。**执行边界（round 3 B10）**：`ActionContext.anchor_bias` 同时是 **HoldPose 每 tick 真实输出目标**（`hold_pose.rs:117-121`，每 tick 读共享值；注释虽留"扩动态 anchor 扩展点"，机械扩展点存在 ≠ 语义变更免拍）——**优先拆分**新概念 `scripted_start_pose`（action 起点）与 `idle_hold_target`（Idle hold），既有启动 `anchor_bias` 与 HoldPose 语义**不动**；若选择动态化共享 anchor，**必须显式拍 HoldPose 新语义**（"保持启动锚点"→"跟随动态值"）**+ 回归 AC**，不得留到实现阶段发现 |

---

## 7. 引用

- 拍板链：`docs/handoffs/v0.5_dimos_buyin_decision.md`（D-BI0~3 + r1/r2）→
  `docs/CHANGELOG.md` 2026-07-13 → `docs/handoffs/v0.5_entry.md` §2.5/§3.4
  勘误标注。
- 先例：`docs/handoffs/v0.4_scripted_backend_summary.md`（6 步 schema 链 +
  byte gate）、`docs/handoffs/v0.3_b4_summary.md`（RR/e2e harness）、
  `configs/xlerobot_task29_right_gripper.toml`（gripper-only + 双 robot_id
  拓扑明文）、`scripts/systemd/README.md`（S0 unit runbook）。
- 冻结基线：`/home/loosqk/repo/robot/dimos-deploy` @ `9ece6bd`（只读）+
  dimos 框架 `/home/loosqk/repo/robot/dimos` @ `a035fb315`（只读）。
