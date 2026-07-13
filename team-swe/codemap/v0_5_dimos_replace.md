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
| M1 | `get_joint_positions()` → `{joint: rad}`（不含夹爪） | `GetState(hardware_id)` → `RobotState.position_rad`（`robot_control.proto:444`；源 = S0 `JointState.position_rad`，`joint.fbs:103`） | ✅ **覆盖且更全**（本仓 gripper 在 joints 数组内，见 M2） |
| M2 | `get_gripper_position(hw_id)` → `[0,1]`（观测链） | 同 `GetState`——gripper 就是 `position_rad[i]` 的一项（`configs/xlerobot_task29_right_gripper.toml:21-30`），**单位 rad 非 [0,1]** | ✅ 覆盖；🟡 单位差（§3 换算，Q8） |
| M3 | `task_invoke(traj_*_arm, "execute", 2 点 0.1s 轨迹)` ×2 臂，10 Hz 抢占覆盖 | `InvokeTask(Scripted{waypoints})`（`scripted_payload.fbs:37-47`） | 🟡 **可映射但三重语义差**（§1.2） |
| M4 | `set_gripper_position(hw_id, [0,1])`，绕仲裁直写 | **无对应 RPC**（10 RPC 无 gripper 下发） | ❌ **唯一硬缺口** → §2 两方案 |

**核对结论**：10 RPC 覆盖 M1/M2（读面完整）与 M3 的载体（写面存在但语义有差）；
**缺口 = M4 夹爪下发一项**。"10 RPC 够用"在字面调用面上**不成立**（M4），在
"读状态 + 双臂轨迹"子集上成立。

### 1.2 M3 的三重语义差（Spec 必答的真问题）

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
   Cancel+Invoke 或等待终态（Q2）。byte 预算充足：4 KiB gate
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
  - 0 新 method / 0 新 schema / 0 S0 改动 / 0 S1 改动——纯 client 侧表达。
- **权限现状**：`invoke_task = operator+`（`configs/rpc_roles.toml:54-59`）。

### 2.2 方案 (b) 新增 Gripper RPC 的事实基础

- **wire 成本（表层）**：frozen proto 只可 append（entry §5 禁止项 #4 +
  2026-07-13 核对标注）——service +1 method；若带 fbs body 走 **6 步 schema
  注册链 atomic**（`v0.4_scripted_backend_summary.md:26-44`：fbs table →
  build.rs 数组 → schema_id（**下一空闲 = 0x040F**，`schema_id.rs` control 段）
  → BodyMarker → SizeTier → build_helpers）+ rpc_roles capability 新条目。
- **真实成本（深层，B/C/D 量级）**：S0 command 面现状是**全向量位置命令**
  （`JointCommand.target:[double]`，`joint.fbs:119-127`）——"直写单个夹爪"在
  S0 侧**没有现成通路**，需要新 control RR（沿 SetEnabled 第 2 rr_server 先例）
  或部分向量命令语义，外加 gateway rr_io_thread、e2e、idempotency method 注册。
  即 (b) 不是"proto append 一下"，是一个完整 additive 候选。
- **语义收益**：与 dimos 直写并行语义 1:1 对齐（不占 action 槽、不受臂轨迹
  串行化影响）；单位可在 RPC 语义里定为 [0,1] 归一化（与 dimos client 面对齐）。

### 2.3 比较表（事实汇总）

| 维度 | (a) 复用 Scripted | (b) 新增 Gripper RPC |
|---|---|---|
| wire 改动 | 0 | +1 method（append）±1 schema（0x040F）+ capability |
| S0 改动 | 0 | 新 RR server 或部分命令语义（无现成通路） |
| S1 改动 | 0 | 0（若走 S0 direct）或 action 化（另议） |
| 与 dimos 并行语义 | 🟡 同车下发等价；独立下发受单 action 串行限制 | ✅ 1:1（绕 action 槽） |
| 单位 | client 层 [0,1]→rad 换算 | 可在 RPC 定义 [0,1] |
| 与验收环契合 | "双臂动作 + 双夹爪"一条 chunk 载荷即达成 | 需三路调用编排 |
| 工程量 | client/adapter 侧小 | B/C/D 级 additive 候选 |

→ **Spec 拍板 Q3**；本 CodeMap 不给结论。

---

## 3. 勘探项 ③：JointTrajectory → ScriptedPayload adapter 定位

### 3.1 字段映射表（源 §0.2 ↔ 靶 B.4/B.5 事实）

| JointTrajectory（dimos） | ScriptedPayload（本仓） | 转换规则 |
|---|---|---|
| `joint_names` + 子集 positions | 全 hardware 向量按索引 | **name→index 解析** + 缺位填当前位（GetState） |
| `points[i].positions`（rad） | `waypoints[i].positions`（rad） | 单位同为 rad，直传；夹爪若并入需 [0,1]→rad |
| `points[i].time_from_start`（s，绝对） | `segment_duration_ms`（相对，>0） | 差分：`(tfs[i]-tfs[i-1])×1000` |
| `points[0] = 当前位形 @t=0` | 无对应——segment0 的 from = S1 `ctx.anchor_bias`（`scripted.rs:17-21`） | **首点丢弃**（语义由 anchor 承担；差异登记） |
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
向量内的处理 → **Q3/Q5**。

---

## 5. 跨面系统性发现（登记）

1. **`configs/xlerobot.toml` 与冻结基线不一致**：每臂 5 joint（4 臂关节+夹爪，
   `xlerobot.toml:16-49`）vs dimos 12 维（每臂 5 臂关节+1 夹爪）；bus_full
   config（8/9 joint）才与 blueprint 电机表对齐。E 的 config 权威源应从冻结
   blueprint 重新核derive（Spec 期落）。
2. **单位三方约定**：dimos client 面夹爪 [0,1] / 本仓 wire 与 S0 全程 rad /
   电机 raw tick（标定 range）。换算住哪层（§3/Q8）决定"[0,1]"这个词在 E 里
   还存不存在。
3. **`traj_supply_arm` 不存在**（blueprint 仅 4 task）——凡历史文档引用即勘误
   对象。
4. **两份 executor 并存**（独立进程版 + Module 版）语义一致——移植对照以
   `executor.py` 为准即可覆盖两者。
5. 观测链的夹爪读（`get_gripper_position`）与执行链的夹爪写各自独立于
   trajectory——本仓 GetState 一次读全向量天然合并读面（M2 ✅）。

---

## 6. Spec 拍板 Q 表（Pre-Research 出口 → Spec round 1 入口）

| # | 问题 | 选项（事实基础见节） |
|---|---|---|
| Q1 | 双臂拓扑 | (a) 双 robot_id 双栈 + 双 gateway（0 架构改动，先例 task29 + Orin 双 S0 unit；client 侧编排两连接）/ (b) gateway 或 s1 多 hardware 化（架构改动，触 §1.2-3 全链） |
| Q2 | 10 Hz 抢占语义映射 | (a) per-chunk 折叠：一次推理 = 一条 10-waypoint Scripted（1 Hz action 粒度；chunk 间 Cancel+Invoke 或等终态）/ (b) 每步 Cancel+Invoke 抢占对（10 Hz RPC ×2，竞态面）/ (c) 新增抢占/streaming 语义（架构改动，默认不选） |
| Q3 | gripper 方案 | (a) 复用 Scripted 全向量同车（§2.1）/ (b) 新增 Gripper RPC（§2.2，B/C/D 级）——比较表 §2.3 |
| Q4 | adapter 形态/位置 | §3.2 (i)-(iv) |
| Q5 | 非目标关节（head 7,8 + 单独动夹爪时的臂）填充策略 | 填 GetState 当前位（波形不动语义）——确认此策略 + 读写间隙位形漂移的容差归属（Spec AC） |
| Q6 | E workflow e2e 放置 | (a) `robot-rpc/tests/`（CI 自动跑，沿 4 e2e 先例）/ (b) robot-tests smoke bin（required-features，人工）/ (c) 双轨：e2e 进 CI + 真机 smoke bin 复用同 case 库 |
| Q7 | 真机完结 gate 驱动方式 | (a) 专用验收 binary（沿 smoke 先例，脚本化"读状态→双臂→双夹爪"）/ (b) robotctl 补 get-state 等子命令后人工驱动 / (c) a+b（binary 为主 robotctl 为证据面） |
| Q8 | 夹爪 [0,1]↔rad 换算权威层 | (a) client/adapter 层换算，wire 全程 rad（S0 0 改）/ (b) 若 Q3 选新增 RPC，RPC 语义定 [0,1]（S0 侧需换算） |
| Q9 | S1/gateway systemd unit + 新 runbook | E 部署段范围确认（unit ×3~4 + runbook 替代 pre-strict 档案；deploy.sh 是否一并落） |

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
