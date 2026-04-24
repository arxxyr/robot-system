# 部署目标：XLeRobot 双臂 + 全向底盘

> 上游参考实现：https://github.com/dimos-robotics/dimos.git （Python + dimos 框架）
> 本地仓库位置：`~/repo/robot/dimos`
> 框架阶段：v0.4（`robot-system` 首个 adapter 落地目标；完成后 dimos 项目切换成调用 robot-system 的二进制 + RPC / iceoryx2 客户端）
> 状态：**骨架**（2026-04-24 建立；§2 契约大量 TBD，v0.4 开工时逐条填实并走 [`../CHANGELOG.md`](../CHANGELOG.md) 登记）

---

## 文档骨架说明

本文件按 [`targets/README.md`](README.md) 约定的 6 节骨架组织：

| 节 | 性质 | 改动流程 |
|---|---|---|
| §1 硬件清单 | 事实（跟随上游硬件变更） | PR 直接改 |
| §2 训练 / 部署契约 | **★ target spec · 硬契约 ★** | **必经 [`../CHANGELOG.md`](../CHANGELOG.md)**（第 3 类：target-contract） |
| §3 上游实现参考 | 非 spec，供 Rust 侧实现时回查 | PR 直接改 |
| §4 在 robot-system 中的映射 | 设计映射（跟随框架演进） | PR 直接改 |
| §5 新增 adapter / crate 需求 | 工程规划 | PR 直接改 |
| §6 风险与门槛 | 工程风险清单 | PR 直接改 |
| §7 参考 | 外部链接 | PR 直接改 |

**迁移路线图**：待 v0.4 开工前按需建立 `xlerobot_migration.md`。
**源码级 walkthrough**：按需建立 `xlerobot_<subsystem>_<commit7>.md`（dimos 是 Python，优先读代码而非建快照文档）。

---

## 1. 硬件清单

| 项 | 型号 / 规格 | 接口 | 备注 |
|---|---|---|---|
| 控制器 | NVIDIA Jetson Orin（dimos 项目默认；兼容 Orange Pi / 其他 aarch64 Linux） | Linux aarch64 | v0.4 部署目标机 |
| 手臂电机 | Feetech STS3215 × 5 × 2 = 10 | 2 × USB-RS485（每臂一条总线） | 位置模式 + PID；硬件层 command timeout（典型 100 ms） |
| 底盘电机 | Feetech STS3215 × 3（三轮全向） | USB-RS485（可与一条手臂共总线或独立，TBD） | 速度模式 |
| 夹爪 / Gripper | TBD（取决于具体装机，dimos 支持可选 2 × Feetech） | 同手臂总线 | |
| 视觉传感器 | Intel RealSense（具体型号 TBD，典型 D435i / D435）× 1 | USB-3.0 | RGB + depth + IMU |
| 辅助相机 | USB Webcam × N（可选） | USB-2.0 | |
| 实时内核 | Linux 主线内核（非必须 RT，因 STS3215 自带 command timeout 作硬件 fail-safe） | — | Orin 默认即可 |

**DoF 布局（v0.4 起以此为准；具体编号 TBD）**：
- 左臂 5 DoF（肩 3 + 肘 1 + 腕 1，或类似，TBD 从 URDF 导出）
- 右臂 5 DoF
- 底盘 3 个轮子（各自独立位置 / 速度控制，运动学合成为 (vx, vy, wz)）
- 可选 gripper × 2

合计最小 13 DoF（10 臂 + 3 轮），含 gripper 时 15 DoF。

**总线拓扑**：见 `~/repo/robot/dimos/src/xlerobot_dimos/common/feetech_io/`。Feetech 通信单总线串行；多电机读写要 pipeline，不能每电机独立 RTT。

---

## 2. 训练 / 部署契约（★ target spec · 硬契约 ★）

> **本节每一条都是 spec**：改动必经 [`../CHANGELOG.md`](../CHANGELOG.md) 第 3 类条目（target-contract）登记。
> **当前大量标 TBD**：v0.4 开工时从 dimos 源码逐条摘取，每填一条走一次 CHANGELOG。

### 2.1 关节角度编码契约

**弧度 ↔ raw 整数**（STS3215 原生单位是 raw int16 / int32 分寄存器）。

- 上游定义：`~/repo/robot/dimos/src/xlerobot_dimos/common/units.py`
- 契约项：每个关节的 `raw_per_rad`（每弧度多少 raw 单位）+ `raw_zero`（raw = 0 对应的物理角度）
- TBD：逐关节常数表（10 或 12 个关节，每个一对 (raw_per_rad, raw_zero)）
- **训练 / 部署必须一致**：仿真侧（IsaacSim / MuJoCo）用 rad，实机用 raw，这两端的转换常数必须对齐

### 2.2 关节校准 offset 持久化

- 每次上电，零点相对机械起点有漂移（STS3215 EEPROM 默认不记绝对编码）。
- 上游做法：每台机器人一份 JSON 校准文件（`~/repo/robot/dimos/calibration/<robot_id>.json`），内含每关节 `zero_offset`。
- 契约：
  - **校准文件是 source of truth**；机器人上电后 driver 必须加载并应用
  - 校准文件格式：每关节 `{motor_id, zero_offset_raw, soft_limit_lo_raw, soft_limit_hi_raw}`（字段名 TBD，核对 dimos 实际字段）
  - 无校准文件 → 拒绝进 READY（类比 ATOM01 的 EStop latch 硬拒）
- **必须 1:1 复刻到 Rust 侧**：`robot-adapters/feetech` 要实现同名字段的 JSON 解析
- TBD：校准文件完整 schema（从 dimos 源码导出）

### 2.3 三轮全向底盘运动学契约

- 上游定义：`~/repo/robot/dimos/src/xlerobot_dimos/common/units.py`（三轮→twist 变换矩阵）
- 契约项：
  - 三个轮子在底盘坐标系中的位置（r, θ）
  - `(vx, vy, wz)` body frame → 三轮转速 的 3×3 变换矩阵
  - 反向（三轮测速 → (vx, vy, wz) odom）
  - 轮子半径 + 减速比
- **训练 / 部署必须一致**：任何 RL / VLA 策略如果输出 `cmd_vel`，必须与仿真侧使用同一套矩阵
- TBD：具体矩阵 + 轮半径 + 减速比

### 2.4 Feetech STS3215 寄存器序列契约

- 上游定义：dimos `docs/06_lerobot_replace_spec.md`
- 契约项（高层）：
  - 位置模式初始化序列（使能字、PID、max torque、command timeout 等）
  - 位置读 / 写的帧格式与期待响应
  - 异常帧（超时、overload、过热）的解析与分级
  - 硬件 fail-safe：`command timeout` 默认 100 ms，超时电机自动停转 / 断力
- **v0.2 Step 10 已引用此 spec**（见 `docs/01_implementation_steps.md` Step 10），XLeRobot v0.4 继续沿用
- `robot-adapters/feetech` 是 Rust 侧实现，**逐寄存器复刻**原 spec
- TBD：是否存在 XLeRobot 特有的 STS3215 自定义参数（扭矩上限 / PID / deadband）

### 2.5 关节软限位

- 契约项：每关节 `[lo, hi]` 硬角度范围（弧度 或 raw，优先 raw 与 EEPROM 对齐）
- 来源：校准 JSON（见 §2.2）+ URDF（`~/repo/robot/dimos/assets/xlerobot/`）
- 行为契约：
  - 下位机：Feetech 硬件 soft limit 寄存器作第一层防线
  - 上位机：每帧读关节角，越界 → `fault` + EStop（与 ATOM01 §2.5 对齐）
- TBD：10 个或 12 个关节的具体 limit 表（v0.4 填）

### 2.6 底盘 `cmd_vel` clip 契约

- 契约项：`(vx_max, vy_max, wz_max)` 安全速度上限
- 来源：dimos 配置（路径 TBD）
- **训练 / 部署必须一致**：若策略输出未 clip 的 `cmd_vel`，robot-system 侧 app crate 必须复刻相同 clip
- TBD：具体数值

### 2.7 视觉传感器契约（若策略使用视觉输入）

- RealSense 固定帧率（典型 30 fps）+ 固定分辨率
- 外参（相机 → base_link 变换）必须在训练 / 部署间完全一致
- 内参（`fx, fy, cx, cy`）从 RealSense EEPROM 读，但 **宽高 / 分辨率 必须在训练 / 部署间固定**
- TBD：XLeRobot 现行外参（从 `~/repo/robot/dimos/assets/xlerobot/` URDF 读）+ 训练侧对应 Isaac / RLBench 配置

### 2.8 VLA / RL 策略契约（仅当使用 Phase 6 OpenPI 栈时）

- 上游参考：`~/repo/robot/dimos/docs/07_phase6_openpi_rl.md`
- 契约项（高层）：
  - OpenPI π0.5 action chunk 长度
  - Observation 组成（RGB / depth / joint_pos / joint_vel / cmd_vel / language）
  - Action 组成（joint target / cmd_vel）及对应关节 / 轴顺序
- v0.5 实际接入 π0.5 时再展开；v0.4 XLeRobot 可先跑手工控制 / 经典策略
- TBD：完整向量布局 + 频率

### 2.9 关键不变量清单（★ 违反 = 机器人抽搐或漂移 ★）

1. §2.1 关节角度编码：`raw_per_rad` / `raw_zero` 训练 / 部署一致。
2. §2.2 校准 offset：必须从校准 JSON 加载；**禁止上电即零位**。无校准 → 拒进 READY。
3. §2.3 三轮全向运动学矩阵：训练 / 部署一致；反向 odom 用于 RL 观测时尤为关键。
4. §2.4 Feetech command timeout：Rust adapter 必须设置（典型 100 ms），作为硬件 fail-safe。
5. §2.5 软限位双层：下位机寄存器 + 上位机每帧检查，任一层触发都 → EStop。
6. §2.6 `cmd_vel` clip 训练 / 部署一致。
7. §2.7 视觉外参 / 分辨率 / 帧率训练 / 部署一致；外参改动**必须**走 target-contract 登记。
8. 底盘与手臂共总线时，Feetech 读写 pipeline 顺序训练 / 部署一致（顺序改变 → 时序改变 → 策略看到的状态 stamp 错位）。

---

## 3. 上游实现参考（非 spec，供 Rust 侧实现时回查）

> 本节**不是** spec，是 dimos 源码的导航图。

### 3.1 dimos 项目目录速写

```
~/repo/robot/dimos/
├─ src/xlerobot_dimos/
│  ├─ common/
│  │  ├─ feetech_io/            # Python STS3215 驱动（Rust 移植源）
│  │  └─ units.py               # 弧度↔raw + 三轮全向运动学矩阵
│  ├─ <其他 xlerobot 子模块>     # 控制器 / 任务 / 策略等
├─ assets/xlerobot/              # URDF + STL（v0.4 后 IK / 3D 渲染用）
├─ calibration/                  # 每机器人一份 JSON（§2.2 契约来源）
├─ docs/
│  ├─ 06_lerobot_replace_spec.md # Feetech STS3215 寄存器序列（§2.4 契约来源）
│  ├─ 07_phase6_openpi_rl.md     # OpenPI π0.5 + RL 集成（§2.8 契约来源）
│  └─ 09_robot_framework.md      # robot-system 设计文档的**源版本**
```

> **dimos docs/09_robot_framework.md** 是本仓库 `docs/00_design.md` 的源版本。本仓库只读快照（v0.1.2 冻结）；改动回写流程见 `00_design.md` 附录 E + `docs/CHANGELOG.md`。

### 3.2 dimos 控制流（高层）

- Python 进程，主循环 ~50 Hz（TBD 具体数值）
- `ControlCoordinator` → `ManipulatorAdapter` / `TwistBaseAdapter` → Feetech IO
- LCM 作进程间通信（robot-system 侧用 iceoryx2 取代；映射见 `00_design.md` 附录 B）
- 策略接入通过独立 Python 模块，同进程调用

### 3.3 RealSense + webcam 接入

- dimos 侧走 `pyrealsense2` + OpenCV
- v0.4 起由 `robot-system` 侧 `robot-adapters/realsense` + `robot-adapters/webcam` 接管
- SoftRT class，挂 `s0_io_thread`（见 `00_design.md` §4.6）

### 3.4 性能 / 延迟数据（dimos 现栈）

- TBD：dimos 侧实测 p99 控制延迟（需要录一次现役运行数据）
- 初步目标（`robot-system` 侧）：手臂控制 ≤ 50 Hz，底盘 ≤ 50 Hz，RealSense 30 fps（SoftRT）

---

## 4. 在 robot-system 中的映射

### 4.1 框架提供（robot-system 本体）

| 职责 | 对应 robot-system 组件 | 需要新做什么 |
|---|---|---|
| Feetech STS3215 驱动（手臂 + 底盘） | `robot-adapters/feetech`（v0.2 Step 10 已规划） | v0.4 扩展到多总线 + 底盘速度模式 |
| 三轮全向运动学 | `robot-kinematics-xlerobot`（`00_design.md` §4.1 / §11.3 定） | `BaseKinematics` trait 的 `XLeRobotBase` 实现；数值从 dimos `units.py` 复刻 |
| IK（双臂） | `robot-kinematics-xlerobot`（`ArmIkSolver` trait） | v0.4 落地；从 dimos kinematics 模块移植 |
| RealSense | `robot-adapters/realsense`（**新 crate**） | v0.4 新做，SoftRT 挂 s0_io_thread |
| Webcam | `robot-adapters/webcam`（**新 crate**） | v0.4 新做 |
| 视觉桥 | `robot-viz`（Rerun） | v0.4 对齐 dimos `rerun_bridge` 能力 |
| 外部 RPC | `robot-rpc` | 映射 dimos 的 RPC 接口到 gRPC |

### 4.2 XLeRobot 特定应用层

- v0.4 起：`robot-app-xlerobot`（**新 crate**，暂无）承载业务逻辑
  - 策略切换 / 任务分派（对齐 dimos 的 task manager）
  - 校准 JSON 加载（§2.2）
  - 校准持久化（上电读、下电或 `/set_zeros` 服务写）
- S0 / S1 不感知"XLeRobot"，只跑通用 trait

### 4.3 和 dimos 的关系

- v0.4 完成 = dimos 项目切换成调用 `robot-system` 二进制 + RPC / iceoryx2 客户端
- dimos 保留 Python 策略 / 任务层；`robot-system` 接管运行时 + 硬件 adapter
- 迁移是**可逆的 A/B**：每个硬件 / 功能切换可通过 systemd unit 或 RPC 端点开关回退
- 映射关系见 `docs/00_design.md` 附录 B

---

## 5. 新增 adapter / crate 需求

### 5.1 `robot-adapters/feetech`（v0.2 Step 10 起规划，v0.4 扩展）

**线程拓扑（方案 B，见 [`../CHANGELOG.md`](../CHANGELOG.md) 2026-04-24 "S0 `s0_rt_thread` 多 bus 语义澄清"）**

XLeRobot 有 2–3 路 USB-RS485 → 在 `robot-s0` 内实例化 **M 个 rt_thread，每 bus 一个**（M=2 或 3 看装机）。每个 rt_thread 的 tick 闭环与 ATOM01 CAN 方向一致：

```
loop {
    clock.sleep_until(deadline); deadline += tick_period;
    bus.drain_rx();                            // non-blocking 到 EAGAIN
    cmd = target_slot.load_latest();
    if fault = safety_check(&state, &cmd) { bus.safe_output(); continue; }
    bus.write_frames_serial(&cmd, &state);     // 每 bus N 电机 pipeline
    snapshot.write_bus_slice(bus.id, &state);
}
```

RS485 是半双工，单 bus 不能同时 read+write，所以 "pipeline 读写" 指一次发 N 请求后连续 drain 回应（而非并行 I/O）。这与方案 B 的 tick 闭环天然吻合。

**必须实现**：
- v0.2 Step 10：单总线单关节验证（M=1）
- v0.4 扩展到 M=2–3：
  - 每 USB-RS485 独立 `termios` / file descriptor
  - 底盘速度模式（位置模式之外的模式枚举）
  - Pipeline 读写（总线内多电机请求 → 连续响应 drain）
  - CPU affinity：每 rt_thread 独占一核

**RT 约束**：
- 位置 / 速度命令在 rt_thread 路径调用，禁止分配（预分配帧缓冲复用）
- 无 spdlog / format / log；`AtomicU64::fetch_add` 计数，dump 由 pub_thread 完成
- 跨线程状态只走 seqlock / atomic / SampleLease

**参考**：
- 寄存器序列：dimos `~/repo/robot/dimos/docs/06_lerobot_replace_spec.md`
- 硬件 fail-safe：STS3215 `command timeout`（典型 100 ms）—— `robot-adapters/feetech` 必须设置

### 5.2 `robot-adapters/realsense`（**新 crate**，v0.4）

- 依赖 `realsense-rust` crate
- 发布：RGB + depth + IMU（若型号支持）
- SoftRT，挂 s0_io_thread
- 需要考虑：启动时 enumeration 可能慢 / 断电重连策略

### 5.3 `robot-adapters/webcam`（**新 crate**，v0.4）

- V4L2 backend（Linux）
- 发布：RGB
- SoftRT

### 5.4 `robot-kinematics-xlerobot`（v0.4）

- 实现：
  - `BaseKinematics::XLeRobotBase`（三轮全向）
  - `ArmIkSolver` × 2（左右臂，5 DoF 每臂，简单解析解或数值解 TBD）
- 数值从 dimos `units.py` + URDF 复刻
- 零 runtime 依赖（`00_design.md` §4.1）

### 5.5 `robot-app-xlerobot`（v0.4）

- 业务层 crate，组织 app 启动 / 任务 / 策略切换
- 校准 JSON 加载（§2.2）
- 不进框架（与 ATOM01 一致）

---

## 6. 风险与门槛

| 风险 | 影响 | 缓解 |
|---|---|---|
| Feetech 多总线 pipeline 复杂度 | 总线时序错 → 状态读取延迟 / 漏读 | 先在 bench 上单总线跑稳，再扩两路；每 bus 一个 rt_thread（[`../CHANGELOG.md`](../CHANGELOG.md) 2026-04-24 条 1）|
| 校准 offset 文件路径 / schema 漂移 | 不同机器人间校准文件不兼容 | §2.2 schema 冻结；所有字段走 target-contract 登记 |
| 三轮全向运动学数值差异 | 机器人漂移 / 方向错 | §2.3 从 dimos `units.py` 逐行复刻；单元测试对拍 |
| RealSense 启动时 enumeration 慢 | 启动期 s0 超 `sd_notify` 超时 | adapter 内部独立线程 + 状态机，initial 阶段不阻塞 ready |
| 底盘 + 手臂共总线时带宽不够 | 控制延迟抖动 | 默认分离总线；共总线仅作降级选项 |
| 视觉外参漂移 | 策略看到的相对位置错 | §2.7 外参走 target-contract 登记；改装前必须重新标定 |
| dimos 侧策略继续演进 | robot-system 侧 adapter 契约追不上 | v0.4 先切换 adapter 层；策略切换滞后到 v0.5（OpenPI 接入时） |
| v0.4 开工时 §2 大量 TBD 未填 | adapter 写出来可能和 dimos 不对齐 | 第一件事：从 dimos 源码批量导出 §2 TBD 项，补完整走 CHANGELOG 登记 |
| **policy_host 调度抖动** —— XLeRobot 默认 Jetson Orin **非 RT 内核** | `SCHED_OTHER` 下 policy_host 推理延迟 p99 可能飘到 30 ms+ | v0.4 立项时决定是否装 RT 内核；若不装，用 `InProcessPolicyHost`（设计 §4.7 分支）绑进 s1；装 RT 则按 ATOM01 同套配置（`SCHED_FIFO 70` + CPU pinning + `mlockall`）—— 见 [`../CHANGELOG.md`](../CHANGELOG.md) 2026-04-24 条 2 |

---

## 7. 参考

| 资源 | 位置 |
|---|---|
| 上游仓库（dimos） | https://github.com/dimos-robotics/dimos.git |
| 本地仓库位置 | `~/repo/robot/dimos` |
| robot-system 设计文档源版本 | `~/repo/robot/dimos/docs/09_robot_framework.md`（对应本仓库 `docs/00_design.md`，冻结版） |
| Feetech STS3215 寄存器序列 spec | `~/repo/robot/dimos/docs/06_lerobot_replace_spec.md` |
| Phase 6 OpenPI + RL 集成 | `~/repo/robot/dimos/docs/07_phase6_openpi_rl.md` |
| 自研 Python STS3215 驱动（Rust 移植源） | `~/repo/robot/dimos/src/xlerobot_dimos/common/feetech_io/` |
| 弧度 ↔ raw / 三轮全向运动学（§2.1 / §2.3 契约源） | `~/repo/robot/dimos/src/xlerobot_dimos/common/units.py` |
| 校准 JSON 示例（§2.2 契约源） | `~/repo/robot/dimos/calibration/` |
| URDF + STL（IK / 可视化） | `~/repo/robot/dimos/assets/xlerobot/` |
| dimos 全局工作约定 | `~/repo/robot/dimos/CLAUDE.md` |
| robot-system ↔ dimos 术语对应 | 本仓库 `docs/00_design.md` 附录 B |

---

**最后更新**：2026-04-24（建立骨架；§2 契约大量 TBD，v0.4 开工时逐条填实并走 CHANGELOG 第 3 类登记）。
