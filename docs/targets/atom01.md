# 部署目标：ATOM01 人形机器人

> 上游参考实现：https://github.com/Roboparty/atom01_deploy.git （ROS 2 Humble + C++17）
> 本地仓库位置：`~/repo/robot/atom01_deploy`
> 用途：后续构型研究与训练／部署合同参考。当前接入范围为 XLeRobot。ATOM01 本地适配与实机验收待后续构型阶段开展。

---

## 文档骨架说明

本文件按下表组织，冻结范围以[目标合同登记规则](README.md)为准。

| 节 | 性质 | 改动流程 |
|---|---|---|
| §1 硬件清单 | 事实（跟随上游硬件变更） | PR 直接改 |
| §2 训练 / 部署契约 | **★ target spec · 硬契约 ★** | **必经 `docs/CHANGELOG.md`** |
| §3 上游实现参考 | 非 spec，供 Rust 侧实现时回查 | PR 直接改 |
| §4 在 robot-system 中的映射 | 设计映射（跟随框架演进） | PR 直接改 |
| §5 新增 adapter / crate 需求 | 工程规划 | PR 直接改 |
| §6 风险与门槛 | 工程风险清单 | PR 直接改 |
| §7 参考 | 外部链接 | PR 直接改 |

当前工作顺序见[项目待办](../../TODO.md)。
**源码级 walkthrough（commit-pinned 快照）**：见 [`atom01_inference_node_d983373.md`](atom01_inference_node_d983373.md)。

---

## 1. 硬件清单

| 项 | 型号 / 规格 | 接口 |
|----|------------|------|
| 控制器 | Orange Pi 5 Plus（RK3588，5.10 RT 内核）或 RDK X5（6.1.83） | Linux aarch64 |
| 电机 | 达妙 Damiao 系列，23 台 | 4 × SocketCAN（`can0`–`can3`，1 Mbit/s） |
| IMU | HiPNUC HI14 | UART 921600 8N1（`/dev/ttyUSB0`），默认 500 Hz（≥200 Hz 必需） |
| 手柄 | 任意 joy_node 兼容 Xbox 布局 | USB HID |
| 实时内核 | 5.10 RT（OPi 5+ 装 `assets/*.deb`）或出厂 RT（RDK X5） | `rtprio 98` + `memlock unlimited` |

**DoF 布局**（23 关节）：左腿 6 + 右腿 + 腰 7 + 左臂 5 + 右臂 5。其中 4 个（索引 4/5/10/11，CAN 上是 motor_id 5/6/11/12）是**踝关节 4-bar 闭链**，需在上层做解耦。

**CAN 映射**（与 `assets/99-auto-up-devs-*.rules` 一致）：
- `can0` —— 左腿（6 个电机）
- `can1` —— 右腿 + 腰部（7 个电机）
- `can2` —— 左臂（5 个电机）
- `can3` —— 右臂（5 个电机）

**波特率**：CAN 1 Mbit/s + txqueuelen 1000，通过 udev 规则自动拉起。

---

## 2. 训练 / 部署契约（★ target spec · 硬契约 ★）

> **本节每一条都是 spec**：训练侧与部署侧必须字节 / 数值级一致，破坏即机器人抽搐 / 起飞 / 倒地。
> 改动必经 [`docs/CHANGELOG.md`](../CHANGELOG.md) 登记（例如"上游把 `obs_num` 从 78 改成 82"）。
> 所有具体 C++ 源码行号引用去 [`atom01_inference_node_d983373.md`](atom01_inference_node_d983373.md)，本节不写行号（会过期）。

### 2.1 推理 / 控制周期

| 参数 | 值 | 约束 |
|---|---|---|
| `dt` | 0.004 s | 控制线程周期 |
| `decimation` | 5 | 推理 / 控制频率比 |
| 推理周期 | `dt * decimation` = 20 ms → **50 Hz** | 训练侧必须完全相同 |
| 控制周期 | `dt` = 4 ms → **250 Hz** | 同上 |

推理与控制**异频双线程**：推理出一次新 target（50 Hz），控制线程以 250 Hz 反复把最近 target 发给电机。`act_alpha` EMA 平滑（默认 1.0 不平滑）。

### 2.2 Observation 向量拼接（★ 字节级契约 ★）

**顺序由 C++ 硬编码，不可调**。训练侧 replay buffer 必须同顺序，否则 policy 看到错位字段直接发散。

#### 2.2.1 普通模式（`obs_num = 78`，23 DoF，`frame_stack = 10`）

| offset | 字段 | 长度 | scale |
|---|---|---|---|
| 0 | `ang_vel`（IMU → body 系） | 3 | `obs_scales_ang_vel` |
| 3 | `gravity_b`（body 系重力投影） | 3 | `obs_scales_gravity_b` |
| 6 | `cmd_vel`（vx, vy, wz） | 3 | `obs_scales_lin_vel` / `_ang_vel` |
| 9 | `(joint_pos - default)[usd2urdf]` | 23 | `obs_scales_dof_pos` |
| 32 | `joint_vel[usd2urdf]` | 23 | `obs_scales_dof_vel` |
| 55 | `last_output`（上一帧 ONNX 输出，未 scale） | 23 | 1.0 |
| *opt* 78 | `interrupt_flag`（`use_interrupt` 时才加） | 1 | 1.0 |

写完后整个 obs 做 `clamp(-clip_observations, clip_observations)`（默认 ±100）。

**校验公式**：`obs_num == 3 + 3 + 3 + 23 + 23 + 23 [+ 1]`。任何一项改了都破坏契约。

#### 2.2.2 Beyondmimic 模式（`motion_obs_num = 121`，`motion_frame_stack = 1`）

| offset | 字段 | 长度 |
|---|---|---|
| 0 | `motion_pos[t]` | 23 |
| 23 | `motion_vel[t]` | 23 |
| 46 | `ang_vel` | 3 |
| 49 | `gravity_b` | 3 |
| *无 cmd_vel* | | |
| 52 | `(joint_pos - default)[usd2urdf]` | 23 |
| 75 | `joint_vel[usd2urdf]` | 23 |
| 98 | `last_output` | 23 |

#### 2.2.3 Attn-enc 模式（`use_attn_enc`，`perception_obs_num = 187`）

堆叠观测后**拼接**一次完整感知向量（不跟着滑窗堆叠）：

```
input_buffer = [obs_{t-F+1}, ..., obs_{t}, perception_obs_{t}]
               |--- frame_stack * obs_num ---|-- perception_obs_num --|
```

### 2.3 Frame stacking（滑窗）契约

- **最新帧在末尾**（非开头）。训练侧 replay buffer 顺序必须一致。
- 第一帧（`is_first_frame_ == true`）：所有 `frame_stack` 槽位**全部填当前 obs**（冷启动自相关）。
- 后续帧：整体左移 `obs_num`，最末 `obs_num` 槽位写当前 obs。

### 2.4 Gravity projection & 倒地检测

| 契约 | 值 |
|---|---|
| Gravity 定义 | `gravity_b = q_b2w.inverse() * (0, 0, -1)` |
| 四元数顺序 | `w, x, y, z` |
| 直立时值 | `gravity_b ≈ (0, 0, -1)` |
| 倒地阈值 | `gravity_b.z > gravity_z_upper`（默认 **-0.5**）|
| IMU→body 外参 | 通过 `extrinsic_R`（3×3 行优先）从 body 系转 IMU 系；`get_quat()` 已完成 `q_body = q_imu * extrinsic_q_inv` |

**倒地硬行为**：原栈直接 FATAL + `rclcpp::shutdown()`；在 robot-system 里**必须**转化为 `robot-s0::EStop latch` + `robot-app-atom01` 发 EStop command —— **不能靠进程退出**（iceoryx2 + systemd 的行为不完全等价于原栈）。

### 2.5 关节软限位

- 每帧推理开始检查：`joint_pos[i] ∉ [joint_limits[2i], joint_limits[2i+1]]` → FATAL。
- `joint_limits` 是 `[lo0, hi0, lo1, hi1, ...]` 展平（URDF 顺序）。
- **硬关节空间限制**，不是速度 / 力矩。

### 2.6 Action 处理链（ONNX 输出 → 电机命令）

```
output_buffer (23)                              ← ONNX 输出
  ↓ clamp(±clip_actions)                        （默认 ±100）
  ↓ act_[usd2urdf_[i]] = output[i]              （按 URDF 顺序重排）
  ↓ act_[usd2urdf_[i]] *= action_scale          （默认 0.25）
  ↓ act_[usd2urdf_[i]] += joint_default_angle[usd2urdf_[i]]

[若 interrupt 开]
  act_[-10..] = interrupt_action_                （末尾 10 维用 /joint_ref_states 替换，通常双臂）

control_thread 250 Hz:
  last_act_ = act_alpha*act_ + (1-act_alpha)*last_act_
  apply_action(last_act_):
    · 闭链踝关节 → decouple → 纯力矩模式（pos=0, vel=0, kp=0, kd=0, torque=τ）
    · 非闭链电机 → MIT 位置模式（pos=target*sign, vel=0, kp, kd, torque=0）
```

契约：
- **`usd2urdf`**：policy 输出顺序 → URDF 顺序的重排（23 元素 permutation）。训练/部署必须一致。
- **`motor_sign`**：URDF/motor 顺序 → 电机物理方向（±1）。错一个关节直接反向。
- **`motor2urdf`** 由构造时反算：`motor2urdf[urdf2motor[i]] = i`。
- **非闭链电机** → MIT 位置模式（pos + kp/kd）；**闭链电机** → 纯力矩模式。

### 2.7 MIT 模式 kp / kd 契约

- **每关节 kp / kd 不同**，从 `robot.yaml.robot.kp` / `.kd` 读（23 × 2 表）。
- 训练侧仿真必须使用**同一套** kp / kd，否则 policy 不可迁移。

### 2.8 闭链踝解耦（4-bar linkage）契约

- 两组 4-bar（左右踝各一组），每组两个电机 ↔ 两个虚拟关节：
  - **Forward**：`motor_q → joint_q`
  - **Decouple**：`(target, joint_q, joint_vel) → motor_τ`
- 机械参数（连杆长度等，**单位毫米**）硬编码在 `close_chain_mapping.cpp`。
- **硬契约**：Rust 侧**逐行复刻**原 C++ 数值，**不重新推导**。
- 单元测试必须做：给定 `motor_q` → Rust 版 `joint_q` 与 C++ 原栈对拍到 **1e-6** 级别一致。
- 配置来源：`robot.yaml` 的 `close_chain_motor_id: [5, 6, 11, 12]`，通过 `urdf2motor` 反查得 `close_chain_joint_idx`。

### 2.9 Gamepad 按键语义（契约）

上升沿触发（对比 `last_buttonN_`）：

| button | 含义 | 行为 |
|---|---|---|
| 0（A） | reset_motors | 暂停推理 + `robot_->reset_joints(joint_default_angle)` |
| 1（B） | start/pause inference | 翻转 `is_running_` |
| 2（X） | init/deinit motors | 暂停推理 + 翻转 `is_init_` |
| 3（Y） | 切换输入源 | 翻转 `is_joy_control_`（joy ↔ `/cmd_vel`） |
| 4（LB） | 切换策略模式 | interrupt 或 beyondmimic 开关 |
| 5（RB） | 切换 motion 序列 | `current_motion_idx_ = (+1) % N`（非 beyondmimic 时） |

摇杆 → `cmd_vel`：
- `axes[4]` → `cmd_vel[0]`（前后），clip 到 `clip_cmd[0..2]`
- `axes[3]` → `cmd_vel[1]`（左右），clip 到 `clip_cmd[2..4]`
- `axes[2]`（LT <0）/ `axes[5]`（RT <0）→ `cmd_vel[2]`（偏航）

> **模式切换契约**：切换瞬间强制 `is_first_frame_ = true`、清零 `last_act_` / motion_frame，避免状态残留。

### 2.10 配置字段全表（训练-部署对齐）

#### `inference.yaml`（ROS parameters）

| 字段 | 默认 | 约束 |
|---|---|---|
| `model_name` | `policy.onnx` | 相对 `src/inference/models/` |
| `obs_num` | 78 | 必须等于 §2.2.1 校验公式 |
| `frame_stack` | 10 | 滑窗深度 |
| `joint_num` | 23 | = ONNX 输出维度 = 电机数 |
| `decimation` | 5 | 见 §2.1 |
| `dt` | 0.004 | 见 §2.1 |
| `action_scale` | 0.25 | ONNX 输出缩放 |
| `clip_actions` | 100 | action 截断 |
| `clip_observations` | 100 | obs 截断 |
| `obs_scales_*` | 1.0 | 五个 scale，训练侧算 obs 用的相同常数 |
| `usd2urdf` | 23 元素 perm | policy → URDF |
| `clip_cmd` | `[-0.4,0.6, -0.4,0.4, -0.8,0.8]` | vx / vy / wz（m/s, rad/s）|
| `joint_default_angle` | 23 元素 | URDF 顺序 |
| `joint_limits` | 46 元素 | `[lo,hi, ...]` URDF 顺序 |
| `gravity_z_upper` | -0.5 | 倒地阈值（§2.4） |
| `use_interrupt` / `use_beyondmimic` / `use_attn_enc` | false | 特性开关 |

#### `robot.yaml`（硬件拓扑，`RobotInterface` 直接读）

承载：`motor_id / motor_interface / motor_num / motor_model / motor_zero_offset / master_id_offset`、`kp / kd / motor_sign / close_chain_motor_id / urdf2motor / extrinsic_R`、IMU 配置。

> **⚠️ 腰 yaw 偏移 `2.093`**：`motor_zero_offset` 索引 12，机械限位块偏移。3D 打印夹具标定时改 0.0。**`inference/config/robot.yaml` 和 `scripts/config/motion_player.yaml` 两处都有**，改要同改。

### 2.11 关键不变量清单（★ 违反 = 抽搐 / 起飞 ★）

1. §2.2 Obs 拼接顺序字节级对齐。**强烈建议**在 `robot-app-atom01` 里加 `--dry-run` dump obs + 训练 replay 对拍。
2. §2.3 滑窗最新帧在**末尾**。
3. §2.6 `usd2urdf` + `motor_sign` 训练/部署一致。
4. §2.6 `action_scale` + `joint_default_angle` 训练/部署一致。
5. §2.7 MIT 模式 kp/kd 训练/部署一致。
6. §2.8 闭链踝解耦数值**逐行复刻**，不重新推导。
7. §2.4 `extrinsic_R` / IMU→body 外参正确；`gravity_b` 方向错 = 倒地误触发或策略认为自己在飞。
8. §2.1 控制 / 推理周期完全对齐。
9. §2.4 倒地行为走 EStop latch，不靠进程退出。

---

## 3. 上游实现参考（非 spec，供 Rust 实现时回查）

> 本节**不是** spec，是对上游代码结构的总览 + 导航。
> 精确行号 / 源码级 walkthrough 去 [`atom01_inference_node_d983373.md`](atom01_inference_node_d983373.md)。

### 3.1 atom01_deploy 架构速写

三个 ROS 2 包（上游独立 git 子模块）：

```
src/imu       HiPNUC 驱动 + imu_py pybind
src/motors    Damiao 驱动 + motors_py pybind（MIT / POS / SPD 模式）
src/inference 依赖上两者，提供 inference_node（ONNX + 控制循环 + ROS 服务）
```

`RobotInterface`（`src/inference/src/robot_interface.cpp`）把电机 / IMU / 闭链解耦 / ThreadPool 封装，对上提供 `get_joint_q/vel/tau`、`get_quat/ang_vel`、`apply_action(Vec<f32>)`。

### 3.2 线程模型

```
main thread                SCHED_FIFO prio=50
  ROS 2 MultiThreadedExecutor(2)
  回调：/joy / /cmd_vel / /elevation_* / /joint_ref_states + 10 个 Trigger 服务

inference_thread           SCHED_FIFO prio=70，周期 20 ms（50 Hz）
  1. [若 beyondmimic] MotionLoader 取 motion_pos/vel
  2. 读 IMU → gravity_b（§2.4 倒地检测）
  3. 读电机（含闭链解耦）→ joint_pos/vel
  4. 软限位（§2.5）
  5. 拼 obs（§2.2）→ clip → 滑窗入 input_buffer（§2.3）
  6. ORT Session::Run → output_buffer
  7. Action 处理（§2.6）→ act_
  8. [若 interrupt] act_ 末尾 10 维用 /joint_ref_states 替换

control_thread             SCHED_FIFO prio=70，周期 4 ms（250 Hz）
  · last_act_ = act_alpha*act_ + (1-act_alpha)*last_act_
  · robot_->apply_action(last_act_) → MIT 命令到 4 路 CAN
```

### 3.3 启动流程

```
main() → mlockall(MCL_CURRENT|MCL_FUTURE) → SCHED_FIFO prio=50
      → InferenceNode 构造：
          load_config()                         # ROS parameters 加载 inference*.yaml
          RobotInterface(ROOT_DIR "config/robot.yaml")
             ├─ setup_imu()                     # HiPNUC driver
             ├─ setup_motors()                  # 23 Damiao motors，按 can0..can3 分组
             ├─ 解析 extrinsic_R → extrinsic_q_inv_
             ├─ 解析 close_chain_motor_id → idx 映射
             └─ ThreadPool(4)                   # 一路 CAN 一个工作线程
          ort_env = Ort::Env(intra_threads=1)
          setup_model(normal_ctx_, "models/policy.onnx",
                      obs_num*frame_stack [+ perception_obs_num])
          [若 beyondmimic]
            for 每个 motion 模型 setup_model(motion_ctx)
            for 每个 motion npz  MotionLoader(...)
          create subs/pubs/services (Best-Effort QoS, depth=1)
          std::thread inference_thread_
          std::thread control_thread_
      → MultiThreadedExecutor(2).spin()
```

**注意**：构造后电机 `is_init_ = false`。用户按手柄 X 键或调 `/init_motors` 服务后才使能。推理开始前必须先 `is_init_ = true`。

### 3.4 ROS 服务清单（10 个，全部 `std_srvs/Trigger`）

| 服务 | 前置 | 作用 |
|---|---|---|
| `/init_motors` | `is_init == false` | 并行对所有电机发 init |
| `/deinit_motors` | `is_init == true` | 并行 deinit |
| `/start_inference` | `is_running == false` | 置 `is_running = true` |
| `/stop_inference` | `is_running == true` | 置 `is_running = false` |
| `/clear_errors` | `robot_ != nullptr` | 每台电机清错 |
| `/set_zeros` | `is_init && !is_running` | 写当前位置为零点（持久化到电机 EEPROM） |
| `/reset_joints` | `is_init && !is_running` | 电机以 kp/2.5 软过渡到 default_angle，1 s 后恢复 kp |
| `/refresh_joints` | `is_init == true` | 主动请求一次电机状态 |
| `/read_joints` | `is_init == true` | 读一次 + publish `/joint_states` |
| `/read_imu` | 始终 | 读一次 + publish `/imu` |

### 3.5 性能 / 延迟数据（原栈）

- 控制线程 SCHED_FIFO 70，周期 4 ms（250 Hz），超时 FATAL。
- 推理线程 SCHED_FIFO 70，周期 20 ms（50 Hz），同上。
- DDS 发现延迟：原栈目标 < 500 ms（"优秀"）。
- `mlockall(MCL_CURRENT|MCL_FUTURE)` 启动锁页。
- ONNX Runtime 单实例，`intra_threads = 1`（避免抖动）。
- Fast DDS profile（`assets/rt_fastdds_profile.xml`）启用共享内存传输 + 实时 QoS。

---

## 4. 拟议映射（未实施）

以下为早期适配研究，保留供实际接入时核对；当前不新建所列crate、服务或线程组织。

### 4.1 框架提供（robot-system 本体）

| 职责 | 对应 robot-system 组件 | 需要新做什么 |
|---|---|---|
| 四路 SocketCAN 并行驱动 | `robot-adapters/damiao`（**新 crate**） | 移植达妙 MIT 帧协议、错误码、offline 判定 |
| HiPNUC 串口驱动 | `robot-adapters/hipnuc`（**新 crate**） | 移植 HiPNUC 0x91/0xA0 帧解析 |
| 进程间通信 | `robot-transport`（iceoryx2） | 沿用 |
| S0 线程（ipc / rt×M / pub / io） | `robot-s0` | 多 bus 场景 rt 线程 ×M（每 CAN bus 一个；ATOM01 M=4）；见 §5.1 + [`../CHANGELOG.md`](../CHANGELOG.md) 2026-04-24 "`s0_rt_thread` 多 bus 语义澄清" |
| RT 约束执行 | `robot-core::time` + allocation-guard | 沿用 |
| ONNX 推理进程 | `robot-policy-host`（**新 crate**，暂无） | 沿用设计 §4.7；需要 `ort` crate |
| 外部 RPC（init / start / set_zeros ...） | `robot-rpc` | 沿用；把 10 个 ROS 服务映射成 gRPC method |
| 手柄输入 | `robot-adapters/joy`（独立 adapter）或 S2 bridge | 新做；或 `robot-app-atom01` 里起独立进程 |

### 4.2 ATOM01 特定应用层（**新 crate**：`robot-app-atom01`）

业务逻辑，**不进框架**，其他机器人用不到。

```
robot-app-atom01/
├─ src/
│  ├─ main.rs               # 起 policy_host + app controller
│  ├─ controller.rs         # Gamepad 状态机 + 模式切换（原 subs_joy_callback 的 Rust 版）
│  ├─ obs_builder.rs        # §2.2 精确复刻
│  ├─ action_pipeline.rs    # §2.6 clip → scale → default_angle → interrupt
│  ├─ frame_stack.rs        # §2.3 滑窗
│  ├─ motion_loader.rs      # npz 读取（ndarray + npyz）
│  ├─ close_chain.rs        # §2.8 闭链踝解耦
│  ├─ fall_guard.rs         # §2.4 gravity_z 倒地检测 + 联动 EStop
│  └─ config.rs             # TOML 等价的 inference*.toml + robot.toml
├─ configs/
│  ├─ inference.toml
│  ├─ inference_amp.toml
│  ├─ inference_beyondmimic.toml
│  ├─ inference_attn_enc.toml
│  └─ robot.toml
└─ models/                  # policy*.onnx（或独立 release artifact）
```

**clear ownership**：
- `robot-s0` RT 线程只**读取** action target（iceoryx2 lease），产 MIT 命令发 CAN。**不**知道什么是 `joint_default_angle`。
- `robot-s1` 在 ATOM01 业务里基本透传（无 IK、无 WBC）。
- `robot-app-atom01` 负责拼 obs → 调 policy_host → 算 action → 下发 s1 → s0 执行。

### 4.3 延迟预算对比

| 阶段 | 原 `atom01_deploy` | `robot-system` 方案 |
|---|---|---|
| IMU 串口 → 状态缓存 | `imu_driver` 内部线程 | `robot-adapters/hipnuc` → iceoryx2 `state/imu` |
| 电机 CAN 读 → 状态缓存 | `robot_interface` 内存 | `robot-s0 io thread` → iceoryx2 `state/motors` |
| obs 拼接 | inference_thread 本地内存 | `robot-app-atom01` / policy_host 订阅 `state/*` 后拼接 |
| ONNX 推理 | inference_node 内联调用 | policy_host 独立进程，iceoryx2 send/recv command |
| action → MIT 命令 | control_thread 本地 → CAN | `robot-app-atom01` publish `command/action` → `robot-s0 rt_thread` → CAN |

额外跳数：IMU 状态 / 电机状态 / action 各一次 iceoryx2 shm 拷贝。共享内存单次 < 1 µs，总额远小于 20 ms 推理周期。

**policy_host 调度确定性**：atom01 运行环境已是 **5.10 RT 内核（OPi 5+）或 6.1.83 RT（RDK X5）**，`rtprio 98 + memlock unlimited` 可用。policy_host 按 `SCHED_FIFO prio=70`（与原栈 `inference_thread` 同级）+ 绑 A76 大核 + `mlockall(MCL_CURRENT|MCL_FUTURE)` 跑，与原栈 inline 推理在**调度层面等价**。落地见 [`../CHANGELOG.md`](../CHANGELOG.md) 2026-04-24 "`§4.7` policy_host 在 RT 内核下调度等价 inline"。

---

## 5. 后续适配参考（未实施）

### 5.1 `robot-adapters/damiao`

**功能对标**：`atom01_deploy/src/motors`（Damiao 子模块）。

**线程拓扑（方案 B，见 [`../CHANGELOG.md`](../CHANGELOG.md) 2026-04-24 "S0 `s0_rt_thread` 多 bus 语义澄清"）**

ATOM01 4 路独立 CAN → 在 `robot-s0` 内实例化 **4 个 rt_thread，每 bus 一个**。每个 rt_thread 的 tick 闭环：

```
loop {
    clock.sleep_until(deadline); deadline += tick_period;
    bus.drain_rx();                            // non-blocking 到 EAGAIN
    cmd = target_slot.load_latest();           // SampleLease latest-wins
    if fault = safety_check(&state, &cmd) {
        bus.safe_output(); snapshot.publish_fault(fault); continue;
    }
    bus.write_frames_serial(&cmd, &state);     // N ≈ 6 电机 × write()
    snapshot.write_bus_slice(bus.id, &state);  // per-bus seqlock
}
```

**不**照搬上游 `atom01_deploy` 的 "每 bus 2 个 RT I/O 线程（RX select + TX cv+lockfree_queue）+ ThreadPool(4) fan-out" 总 13 线程拓扑，理由：
- 上游 `dm_motor_driver.cpp:135-160` 的 RX callback 在 I/O 线程里 `logger_->error(...)` format + 写非 atomic `float` 成员，**违反 §4.9 RT 铁律 + 有数据竞态**
- 单线程闭环等价：RX 改 tick 开始 `drain_rx()` 非阻塞 drain；TX 直接 `write()`，靠 `SO_SNDBUF ≥ 1 MB` + `txqueuelen ≥ 1000` 保稳态 <10 µs 返回

**必须实现**：
- 4 路独立 SocketCAN（支持 `can0`–`canN`，数量可配）
- MIT 模式帧（position + velocity + kp/kd + torque）；寄存器序列见上游 `src/motors/src/drivers/dm/`
- POS / SPD 模式（工具链用，非 RT 主路径）
- `init_motor` / `deinit_motor`：使能字、清错、确认响应帧
- `set_motor_zero`：写零点到电机 EEPROM（**必须持久化**）
- `refresh_motor_status` / `get_response_count`：离线判定（`offline_threshold = 25`）
- `clear_motor_error`
- 跨线程状态只走 seqlock / atomic / SampleLease；`response_count_` 在 rt_thread 内计数即可，非 atomic

**RT 约束**：
- `motor_mit_cmd` 在 rt_thread 路径上调用，**禁止分配**（预分配 `can_frame` 数组并复用）
- `write()` 返回 `EAGAIN` → 记 `write_eagain_count` metric，跳过该帧，**不在 rt_thread 内 retry**
- 无 spdlog / tracing-format / 任何 format 构造；`AtomicU64::fetch_add` 计数，dump 由 pub_thread 完成
- socket fd 启动时配：`O_NONBLOCK` + `SO_SNDBUF ≥ 1 MB` + 外部 `ip link set canN txqueuelen 1000`

**RT 风险点**：
- 单 CAN bus 稳态 write 时间：6 电机 × ~14 B 帧 × 1 Mbps ≈ 780 µs；1 kHz tick 1 ms 边界紧 → **每 rt_thread 必须独占 CPU 核**（CPU affinity）
- Orange Pi 5+（RK3588 8 核）/ Orin：4 个 CAN rt_thread + ipc + pub + io + s1 + 余 1-2 核 ≈ 8 核正好；**`robot.toml` 里显式指派 affinity**，不让 kernel 自由调度
- 多 bus 并发 snapshot 写：`RtSnapshotWriter` per-bus slice（seqlock），pub_thread 读多 slice 组装全关节快照 —— 具体 API 形状待 v0.4 多 bus 首次落地时定形（CHANGELOG 2026-04-24 条目有登记）

### 5.2 `robot-adapters/hipnuc`

**功能对标**：`atom01_deploy/src/imu`（HiPNUC 子模块）。

**必须实现**：
- 921600 8N1 串口（termios 配死，不依赖 runtime 串口库），能到 500 Hz
- 帧同步 + CRC（HiPNUC 协议 0x91 / 0xA0）
- 输出：四元数 w/x/y/z、角速度 xyz、线加速度 xyz、温度
- 外参应用（`extrinsic_R`）：留给上层（`robot-app-atom01`）做，或 adapter 配置里带
- **非 RT 路径**：IMU 读取放 `robot-s0 io_thread`，不进 rt_thread

**风险点**：
- UART 字节流，driver 做状态机同步不丢帧
- 500 Hz = 2 ms/帧，io_thread 必须跟上（普通优先级足够，但别和日志线程耦合）

### 5.3 `robot-policy-host`

**功能对标**：ONNX Runtime 调用封装（原栈 inline 在 inference_node 内）。

**必须实现**：
- `ort` crate 封装，`intra_threads = 1`
- warmup（预跑 1-2 次丢弃结果）
- tensor 零拷贝绑定（参考 `atom01_inference_node_d983373.md` §16.2）
- 策略多模型切换（normal / amp / beyondmimic / attn_enc）
- 调度：`chrt -f`、CPU affinity

### 5.4 `robot-app-atom01`

见 §4.2 crate 骨架。

---

## 6. 风险与门槛

| 风险 | 影响 | 缓解 |
|---|---|---|
| 闭链解耦数值差异 | 踝关节抖 / 卡滞 | §2.8：给定 `motor_q` → Rust 版 `joint_q` 与 C++ 对拍 1e-6 |
| 观测位错位 | 机器人抽搐 / 直接倒 | §2.2 dry-run dump + 训练 replay 对拍 |
| 零点持久化 | 每次上电都要重标 | §5.1 `set_motor_zero` 必须写 EEPROM |
| 训练侧非 Isaac / 非 `obs_num=78` 布局 | §2.2 不适用 | `robot-app-atom01` 里做可配置的 obs builder（TOML 描述字段顺序） |
| 多策略切换 state 残留 | 模式切换瞬间输出异常 | §2.9：强制 `is_first_frame_ = true`、清零 `last_act` / motion_frame |
| 倒地行为依赖进程退出 | systemd + iceoryx2 行为不等价原栈 | §2.4：走 EStop latch，不走 shutdown |
| SocketCAN `write()` 阻塞 RT | RT 线程 miss deadline | §5.1 方案 B + `SO_SNDBUF ≥ 1 MB` + `txqueuelen ≥ 1000` + `EAGAIN` 不 retry；验收：XLeRobot v0.4 cyclictest + EAGAIN=0（[`../CHANGELOG.md`](../CHANGELOG.md) 2026-04-24 条 1） |

### 从"风险"降级为"验收项"

以下保留2026-04-24的研究目标，不表示已有测量或验收。适用判据以 [验证约定](../10_verification.md) 为准；其中≥50Hz的旧IPC阈值已撤销，不能继续作放行门。

| 项 | 收口条件 | 验收 Step |
|---|---|---|
| policy_host 调度抖动 vs inline | `SCHED_FIFO 70 + A76 pinning + mlockall` 下 p99 推理延迟差 < 1 ms | Step 6 iceoryx2 RT baseline + v0.5 ATOM01 policy_host 落地对拍 |
| iceoryx2 pub/sub RT 行为 | 旧50Hz阈值已撤销；新门槛须另行确定，见验证约定 | 不作为当前放行门|
| ORT 稳态 0 alloc | warmup 2 次后第 3 次及之后 Run 经 `assert-no-alloc` 守卫不触发 | v0.5 `robot-policy-host` 落地 |
| 双 shm round-trip (obs + action) | p99 < 50 µs | v0.5 policy_host 接入 iceoryx2 |

---

## 7. 参考

| 资源 | 位置 |
|---|---|
| 原上游仓库 | https://github.com/Roboparty/atom01_deploy.git |
| 本地仓库位置 | `~/repo/robot/atom01_deploy` |
| 源码快照文档（commit `d983373`） | [`atom01_inference_node_d983373.md`](atom01_inference_node_d983373.md) |
| 平台验收范围 | [当前状态](../s0_hal_status.md) |
| inference_node 主逻辑 | `atom01_deploy/src/inference/src/inference_node.{hpp,cpp}` |
| 启动脚本 / Fast DDS profile | `atom01_deploy/tools/start_robot.sh`、`assets/rt_fastdds_profile.xml` |
| `RobotInterface`（硬件封装） | `atom01_deploy/src/inference/src/robot_interface.{cpp,hpp}` |
| 闭链解耦 | `atom01_deploy/src/inference/src/utils/close_chain_mapping.{hpp,cpp}` |
| 达妙驱动 | `atom01_deploy/src/motors/` |
| HiPNUC 驱动 | `atom01_deploy/src/imu/` |
| 原栈 CLAUDE.md | `atom01_deploy/CLAUDE.md` |
| 达妙电机 ID 标定 | [RoboParty 文档](https://roboparty.feishu.cn/wiki/Sh5vw7QFZimO5Skxzficro2Pnkf) |
| HiPNUC 协议手册 | [HiPNUC 官网](https://www.hipnuc.com/resource_hi14.html) |
