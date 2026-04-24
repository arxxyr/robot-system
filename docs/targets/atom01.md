# 部署目标：ATOM01 人形机器人

> 上游参考实现：https://github.com/Roboparty/atom01_deploy.git （ROS 2 Humble + C++17）
> 本地仓库位置：`~/repo/robot/atom01_deploy`
> 目的：验证 `robot-system` 能承载**端到端 RL 策略（joint-space）** 这类部署场景，并作为首个人形机器人目标。

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

## 2. 上游 `atom01_deploy` 架构速写

三个 ROS 2 包（在上游是独立 git 子模块）：

```
src/imu      —— HiPNUC 驱动 + imu_py pybind
src/motors   —— Damiao 驱动 + motors_py pybind（MIT / POS / SPD 模式）
src/inference —— 依赖上两者，提供 inference_node（ONNX + 控制循环 + ROS 服务）
```

`RobotInterface`（`src/inference/src/robot_interface.cpp`）把电机/IMU/闭链解耦/ThreadPool 封装在一起，对上提供 `get_joint_q/vel/tau`、`get_quat/ang_vel`、`apply_action(Vec<f32>)`。

`inference_node`（`src/inference/src/inference_node.{hpp,cpp}`）是核心逻辑，**下一节详细展开**。

---

## 3. inference_node 逻辑详解（★ 精确复刻契约 ★）

### 3.1 线程模型

```
┌────────────────────────────────────────────────────────────────┐
│  main thread  (SCHED_FIFO prio=50)                              │
│    ROS 2 MultiThreadedExecutor(2)，回调：                        │
│      · /joy           → Gamepad 状态机 + cmd_vel 写入             │
│      · /cmd_vel       → cmd_vel 写入（仅非 joy 控制时）           │
│      · /elevation_*   → perception_obs（attn_enc 模式）           │
│      · /joint_ref_states → interrupt_action（interrupt 模式）     │
│      · 10 个 /std_srvs/Trigger 服务（init/start/set_zeros/...）   │
└────────────────────────────────────────────────────────────────┘

┌────────────────────────────────────────────────────────────────┐
│  inference_thread  (SCHED_FIFO prio=70)                         │
│    周期 = dt * decimation  （默认 0.004s × 5 = 20ms → 50 Hz）    │
│    1. 若 beyondmimic：从 MotionLoader 取 motion_pos/vel          │
│    2. 读 IMU → gravity_b （倒地检测：gravity_b.z > -0.5 → 紧急停） │
│    3. 读电机关节 (含闭链解耦) → joint_pos / vel                    │
│    4. 关节软限位检查 → 越界 FATAL                                  │
│    5. 拼接 obs → clip → 滑窗入 input_buffer                      │
│    6. ORT Session::Run → output_buffer                           │
│    7. output clip → * action_scale + joint_default_angle → act_  │
│    8. 若 interrupt 开：把 act_ 末尾 10 维用 /joint_ref_states 替换│
└────────────────────────────────────────────────────────────────┘

┌────────────────────────────────────────────────────────────────┐
│  control_thread  (SCHED_FIFO prio=70)                           │
│    周期 = dt （默认 4ms → 250 Hz）                                │
│    · last_act_ = act_alpha * act_ + (1-act_alpha) * last_act_   │
│    · robot_->apply_action(last_act_)  → MIT 命令到 4 路 CAN      │
└────────────────────────────────────────────────────────────────┘
```

**关键点**：推理和控制是**异频双线程**，推理出一次新 target（50 Hz），控制线程以 250 Hz 反复把最近的 target 发给电机。`act_alpha` 可做 EMA 平滑（默认 1.0，即不平滑）。

### 3.2 启动流程

```
main() → mlockall(MCL_CURRENT|MCL_FUTURE)
       → SCHED_FIFO prio=50
       → InferenceNode 构造：
           load_config()                   # 从 ROS parameters 加载 inference*.yaml
           RobotInterface(ROOT_DIR "config/robot.yaml")
              ├─ setup_imu()               # HiPNUC driver
              ├─ setup_motors()            # 23 Damiao motors，按 can0..can3 分组
              ├─ 解析 extrinsic_R → extrinsic_q_inv_
              ├─ 解析 close_chain_motor_id → idx 映射
              └─ ThreadPool(4)             # 一路 CAN 一个工作线程
           ort_env = Ort::Env(intra_threads=1)
           setup_model(normal_ctx_, "models/policy.onnx",
                       obs_num*frame_stack [+ perception_obs_num])
           如果 beyondmimic：
             for 每个 motion 模型 setup_model(motion_ctx)
             for 每个 motion npz  MotionLoader(...)
           create subscribers / publishers / services (Best-Effort QoS, depth=1)
           std::thread inference_thread_ = [inference()]
           std::thread control_thread_   = [control()]
       → MultiThreadedExecutor(2).spin()
```

**注意**：`RobotInterface` 构造后电机**没有初始化**（`is_init_ = false`）。用户按手柄 X 键或调 `/init_motors` 服务后才真正使能。推理开始前必须先 `is_init_=true`。

### 3.3 Observation 向量拼接（★ 训练-部署契约 ★）

一帧 obs 按 **固定顺序** 写入 `obs_[]`（`inference_node.cpp` L154-222）。**顺序不可调，代码硬编码。**

**普通模式（non-beyondmimic，`obs_num=78`，23 DoF，`frame_stack=10`）**：

| offset | 字段 | 长度 | scale |
|--------|------|------|-------|
| 0 | `ang_vel` （IMU → body 系）| 3 | `obs_scales_ang_vel` |
| 3 | `gravity_b` （body 系重力投影）| 3 | `obs_scales_gravity_b` |
| 6 | `cmd_vel` （vx, vy, wz）| 3 | `obs_scales_lin_vel / _ang_vel` |
| 9 | `(joint_pos - default)[usd2urdf]` | 23 | `obs_scales_dof_pos` |
| 32 | `joint_vel[usd2urdf]` | 23 | `obs_scales_dof_vel` |
| 55 | `last_output` （上一帧 ONNX 输出，未 scale）| 23 | 1.0 |
| *opt* 78 | `interrupt_flag` （`use_interrupt` 时才加）| 1 | 1.0 |

写完后整个 obs 做 `clamp(-clip_observations, clip_observations)`（默认 ±100）。

**Beyondmimic 模式（`motion_obs_num=121`，`motion_frame_stack=1`）**：

| offset | 字段 | 长度 |
|--------|------|------|
| 0 | `motion_pos[t]` | 23 |
| 23 | `motion_vel[t]` | 23 |
| 46 | `ang_vel` | 3 |
| 49 | `gravity_b` | 3 |
| *no cmd_vel* | | |
| 52 | `(joint_pos - default)[usd2urdf]` | 23 |
| 75 | `joint_vel[usd2urdf]` | 23 |
| 98 | `last_output` | 23 |

**Attn-enc 模式（`use_attn_enc`，`perception_obs_num=187`）**：在堆叠观测后**拼接**一次完整感知向量（不跟着滑窗堆叠）：

```
input_buffer = [obs_{t-F+1}, ..., obs_{t}, perception_obs_{t}]
               |--- frame_stack * obs_num ---|-- perception_obs_num --|
```

### 3.4 Frame stacking（滑窗）

第一帧（`is_first_frame_=true`）：所有 `frame_stack` 槽位**全部填当前 obs**（冷启动自相关）。

后续帧：
```
input_buffer[0 .. (F-1)*obs_num]   ← 原本 input_buffer[obs_num .. F*obs_num]  （左移）
input_buffer[(F-1)*obs_num .. F*obs_num] ← obs_         （最新帧放末尾）
```

⚠️ **注意**：是"最新帧在末尾"，**不是**"最新帧在开头"。训练侧 replay buffer 必须同样的顺序，否则 policy 会看到反向时序。

### 3.5 Gravity & 倒地检测

```cpp
Eigen::Quaternionf q_b2w(quat_[0], quat_[1], quat_[2], quat_[3]);  // w,x,y,z
Eigen::Vector3f gravity_w(0.f, 0.f, -1.f);
Eigen::Vector3f gravity_b = q_b2w.inverse() * gravity_w;
if (gravity_b.z() > gravity_z_upper_ /* 默认 -0.5 */) {
    FATAL("Robot fell down!");
    rclcpp::shutdown();
}
```

- 直立时 `gravity_b ≈ (0, 0, -1)`，跌倒时 z 翻正。
- IMU 外参用 `extrinsic_R`（3×3 行优先）从 body 系转到 IMU 系，`RobotInterface::get_quat()` 里通过 `q_body = q_imu * extrinsic_q_inv` 已经做完了这层换算。

### 3.6 关节软限位

每帧推理开始时检查：
```cpp
for (i in 0..23):
    if joint_pos_[i] < joint_limits_[2*i] || joint_pos_[i] > joint_limits_[2*i+1]:
        FATAL("Joint i out of limit")
```
`joint_limits` 是 `[lo0,hi0, lo1,hi1, ...]` 展平。**这是硬关节空间限制，不是速度/力矩**。

### 3.7 Action 处理链（ONNX 输出 → 电机命令）

```
output_buffer (23)                                ← ONNX 输出
  ↓ clamp(±clip_actions)                          （默认 ±100）
  ↓ act_[usd2urdf_[i]] = output[i]                （按 URDF 顺序放）
  ↓ act_[usd2urdf_[i]] = act_ * action_scale      （缩放，默认 0.25）
                     + joint_default_angle[usd2urdf_[i]]   （叠加中位角）

[若 interrupt 开]
  act_[-10..] = interrupt_action_                 （用 /joint_ref_states 替换末尾 10 维，通常是双臂）

control_thread 200–250 Hz:
  last_act_ = act_alpha*act_ + (1-act_alpha)*last_act_
  robot_->apply_action(last_act_):
    · 并行读每路 CAN 上所有电机状态（motor_sign 应用）
    · 闭链踝关节：forward decouple（motor 角 → 虚拟关节角）
    · 闭链踝关节：再 decouple（target → 电机力矩命令）
    · 非闭链电机：motor_mit_cmd(pos=target*sign, vel=0, kp, kd, torque=0)
    · 闭链电机：motor_mit_cmd(pos=0, vel=0, kp=0, kd=0, torque=computed)
```

关键点：
- **`usd2urdf` 是 policy 输出顺序 → URDF 顺序的重排**（23 元素 permutation），训练和部署必须一致。
- **`motor_sign` 是 URDF/motor 顺序 → 电机物理方向**（±1 翻转），从 `robot.yaml.robot.motor_sign` 读。
- `motor2urdf` 在构造时从 `urdf2motor` 反算：`motor2urdf_[urdf2motor_[i]] = i`。
- 非闭链电机走**位置模式 MIT**（pos + kp/kd），闭链电机走**纯力矩模式**（计算出 τ 直接发）。

### 3.8 闭链踝解耦（4-bar linkage）

两组 4-bar（左右踝各一组），每组两个电机 ↔ 两个虚拟关节：

```
Forward（电机角 → 虚拟关节角）:   motor_q → joint_q
Decouple（目标 → 电机力矩）:      (target, joint_q, joint_vel) → motor_τ
```

实现见 `src/inference/src/utils/close_chain_mapping.{hpp,cpp}`，用 `Decouple::get_forwardQVT()` / `get_decoupleQVT()`。`ankle_decouple_` 对左右踝分别调用，参数 `true`/`false` 区分左右。

**从 `robot.yaml`**：
```yaml
close_chain_motor_id: [5, 6, 11, 12]  # motor_id，4 个电机
```
对应 URDF 索引通过 `urdf2motor` 反查得到 `close_chain_joint_idx_`（4 个关节索引）。

### 3.9 Gamepad 状态机（`subs_joy_callback`）

所有按键是**上升沿触发**（对比 `last_buttonN_`）：

| button index | 含义 | 行为 |
|--------------|------|------|
| 0 （A） | reset_motors | 暂停推理 + `robot_->reset_joints(joint_default_angle)` |
| 1 （B） | start/pause inference | 翻转 `is_running_` |
| 2 （X） | init/deinit motors | 暂停推理 + 翻转 `is_init_` |
| 3 （Y） | 切换输入源 | 翻转 `is_joy_control_`（joy ↔ `/cmd_vel`） |
| 4 （LB） | 切换策略模式 | interrupt 或 beyondmimic 开关 |
| 5 （RB） | 切换 motion 序列 | `current_motion_idx_ = (+1) % motions.size`（仅非 beyondmimic 时允许） |

摇杆：
- `axes[4]` → `cmd_vel[0]`（前后），clip 到 `clip_cmd[0..2]`
- `axes[3]` → `cmd_vel[1]`（左右），clip 到 `clip_cmd[2..4]`
- `axes[2]`（LT，<0 时）/ `axes[5]`（RT，<0 时）→ `cmd_vel[2]`（偏航）

### 3.10 ROS 服务（10 个，全部 `std_srvs/Trigger`）

| 服务 | 前置条件 | 作用 |
|------|----------|------|
| `/init_motors` | `is_init==false` | 并行对所有电机发 init |
| `/deinit_motors` | `is_init==true` | 并行 deinit |
| `/start_inference` | `is_running==false` | 置 `is_running=true` |
| `/stop_inference` | `is_running==true` | 置 `is_running=false` |
| `/clear_errors` | `robot_ != nullptr` | 每台电机清错 |
| `/set_zeros` | `is_init==true` && `!is_running` | 每台电机写当前位置为零点（持久化到电机 EEPROM） |
| `/reset_joints` | `is_init==true` && `!is_running` | 电机以 kp/2.5 软过渡到 default_angle，1s 后恢复 kp |
| `/refresh_joints` | `is_init==true` | 主动请求一次电机状态（非周期读） |
| `/read_joints` | `is_init==true` | 读一次 + publish `/joint_states` |
| `/read_imu` | 始终 | 读一次 + publish `/imu` |

### 3.11 配置契约（训练-部署必须对齐的字段）

**`src/inference/config/inference.yaml`**（ROS parameters）：

| 字段 | 默认 | 含义 / 约束 |
|------|------|----------|
| `model_name` | `policy.onnx` | 相对 `src/inference/models/` |
| `obs_num` | 78 | 单帧 obs 向量长度，**必须等于 3+3+3+23+23+23 [+1]** |
| `frame_stack` | 10 | 滑窗深度 |
| `joint_num` | 23 | = ONNX 输出维度 = 电机数 |
| `decimation` | 5 | 控制/推理频率比，`inference_period = dt*decimation` |
| `dt` | 0.004 | 控制线程周期（s） |
| `action_scale` | 0.25 | ONNX 输出缩放 |
| `clip_actions` | 100 | action 截断 |
| `clip_observations` | 100 | obs 截断 |
| `obs_scales_*` | 1.0 | 五个 scale，训练侧算 obs 时用的相同常数 |
| `usd2urdf` | 23 元素 perm | policy 输出顺序 → URDF 顺序 |
| `clip_cmd` | `[-0.4,0.6, -0.4,0.4, -0.8,0.8]` | vx / vy / wz 范围（m/s, rad/s） |
| `joint_default_angle` | 23 元素 | 中位角（URDF 顺序） |
| `joint_limits` | 46 元素 | `[lo,hi, lo,hi, ...]`（URDF 顺序） |
| `gravity_z_upper` | -0.5 | 倒地阈值 |
| `use_interrupt` / `use_beyondmimic` / `use_attn_enc` | false | 特性开关 |

**`src/inference/config/robot.yaml`**（`RobotInterface` 直接读，不走 ROS params）：

承载硬件拓扑：`motor_id / motor_interface / motor_num / motor_model / motor_zero_offset / master_id_offset` + `kp / kd / motor_sign / close_chain_motor_id / urdf2motor / extrinsic_R` + IMU 配置。

**⚠️ 腰 yaw 偏移 `2.093`**：`motor_zero_offset` 索引 12，是机械限位块的偏移。用 3D 打印夹具标定时改成 0.0。这个字段在 `inference/config/robot.yaml` 和 `scripts/config/motion_player.yaml` **两处**都有。

### 3.12 性能 / 延迟数据（原栈）

- 控制线程 SCHED_FIFO 70，周期 4ms（250 Hz），若单次跑超 FATAL 警告。
- 推理线程 SCHED_FIFO 70，周期 20ms（50 Hz），同上。
- DDS 发现延迟：原栈目标 <500ms（"优秀"）。
- `mlockall(MCL_CURRENT|MCL_FUTURE)` 启动时锁页。
- ONNX Runtime 单实例，`intra_threads=1`（避免抖动）。
- Fast DDS profile（`assets/rt_fastdds_profile.xml`）启用共享内存传输 + 实时 QoS。

---

## 4. 在 robot-system 中的映射

### 4.1 框架提供（robot-system 本体）

| 职责 | 对应 robot-system 组件 | 需要新做什么 |
|------|---------------------|-------------|
| 四路 SocketCAN 并行驱动 | `robot-adapters/damiao`（**新 crate**） | 移植达妙 MIT 帧协议、错误码、offline 判定 |
| HiPNUC 串口驱动 | `robot-adapters/hipnuc`（**新 crate**） | 移植 HiPNUC 0x91/0xA0 帧解析 |
| 进程间通信 | `robot-transport`（iceoryx2） | 沿用 |
| S0 四线程（ipc/rt/pub/io） | `robot-s0` | 沿用；RT 线程跑 MIT 命令发送 + 电机状态读取 |
| RT 约束执行 | `robot-core::time` + allocation-guard | 沿用 |
| ONNX 推理进程 | `robot-policy-host`（**新 crate**，暂无） | 沿用设计文档 §4.7；需要 onnxruntime-rs 或 ort |
| 外部 RPC（init / start / set_zeros ...） | `robot-rpc` | 沿用；把 10 个 ROS 服务映射成 gRPC method |
| 手柄输入 | 可以作为 `robot-adapters/joy`（独立 adapter 或走 S2 bridge） | 新做；或从 `robot-app-atom01` 里起一个独立进程 |

### 4.2 ATOM01 特定应用层（**新 crate**：`robot-app-atom01`）

这部分**不进框架**，因为属于业务逻辑，别的机器人用不到。建议做法：

```
robot-app-atom01/
├─ src/
│  ├─ main.rs               # 起 policy_host + app controller
│  ├─ controller.rs         # Gamepad 状态机 + 模式切换（原 subs_joy_callback 的 Rust 版）
│  ├─ obs_builder.rs        # §3.3 精确复刻的观测拼接
│  ├─ action_pipeline.rs    # §3.7 的 clip → scale → default_angle → interrupt 覆盖
│  ├─ frame_stack.rs        # §3.4 滑窗
│  ├─ motion_loader.rs      # npz 读取（ndarray + npyz crate）
│  ├─ close_chain.rs        # 闭链踝解耦（移植 close_chain_mapping.cpp）
│  ├─ fall_guard.rs         # gravity_z 倒地检测 + 联动 EStop
│  └─ config.rs             # TOML 等价的 inference*.toml + robot.toml
├─ configs/
│  ├─ inference.toml        # §3.11 字段
│  ├─ inference_amp.toml
│  ├─ inference_beyondmimic.toml
│  ├─ inference_attn_enc.toml
│  └─ robot.toml            # 硬件拓扑（取代 YAML）
└─ models/                  # policy*.onnx（或独立 release artifact）
```

**clear ownership**：
- `robot-s0` 里的 RT 线程只**读取** `action target`（iceoryx2 lease），产出 MIT 命令发 CAN。它不知道什么是 `joint_default_angle`。
- `robot-s1` 可能作为 coordinator 转发命令，但原子 ATOM01 的业务中 S1 基本透传（无 IK、无 WBC）。
- `robot-app-atom01` 负责拼 obs → 调 policy_host → 算 action → 下发到 s1 → s0 执行。

### 4.3 延迟预算对比

| 阶段 | 原 `atom01_deploy` | `robot-system` 方案 |
|------|-------------------|---------------------|
| IMU 串口 → 状态缓存 | 直接在 `imu_driver` 内部线程 | `robot-adapters/hipnuc` → iceoryx2 `state/imu` |
| 电机 CAN 读 → 状态缓存 | 同上，`robot_interface` 内存 | `robot-s0 io thread` → iceoryx2 `state/motors` |
| obs 拼接 | inference_thread 本地内存访问 | `robot-app-atom01` / policy_host 订阅 `state/*` 后拼接 |
| ONNX 推理 | inference_node 内联调用 | policy_host 独立进程，iceoryx2 send/recv command |
| action → MIT 命令 | control_thread 本地 → CAN | `robot-app-atom01` publish `command/action` → `robot-s0 rt_thread` → CAN |

额外跳数：IMU 状态 / 电机状态 / action 各一次 iceoryx2 shm 拷贝。共享内存单次 <1µs，总额增量远小于 20ms 推理周期，**理论上 OK**。但**必须实测**：policy_host 进程调度抖动可能比 inline 调用更差，要用 `chrt -f` 保证 SCHED_FIFO。

---

## 5. 新增 adapter 需求

### 5.1 `robot-adapters/damiao`

**功能对标**：`atom01_deploy/src/motors`（Damiao 子模块）。

**必须实现的能力**：
- 4 路独立 SocketCAN 实例（支持 `can0`–`canN`，数量可配）
- MIT 模式帧（position + velocity + kp/kd + torque），寄存器序列见上游 `src/motors/src/drivers/dm/`
- POS / SPD 模式（工具链用，非 RT 主路径）
- `init_motor` / `deinit_motor`：设置使能字、清错、确认响应帧
- `set_motor_zero`：写零点到电机 EEPROM（要持久化到硬件）
- `refresh_motor_status` / `get_response_count`：离线判定（`offline_threshold=25`）
- `clear_motor_error`
- **线程模型**：每路 CAN 一个工作线程并行下发（上游用 ThreadPool(4)）。在 robot-system 里走 `robot-s0` 的 rt_thread + io_thread 分工。
- **RT 约束**：`motor_mit_cmd` 在 RT 路径上调用，**禁止分配**（预分配 `can_frame` + `write()` syscall）。

**RT 风险点**：
- SocketCAN `write()` 是阻塞 syscall，RT 线程里要么用非阻塞 + 轮询，要么保证 kernel 层不 block（可能需要调整 `txqueuelen` + `SO_SNDTIMEO`）。
- 4 路 CAN 并行 = 4 个线程，在 Orange Pi 8 核上 OK，但要预约 CPU affinity 避免和内核 RT 线程抢。

### 5.2 `robot-adapters/hipnuc`

**功能对标**：`atom01_deploy/src/imu`（HiPNUC 子模块）。

**必须实现的能力**：
- 921600 8N1 串口（termios 配好不要靠 runtime 串口库），能到 500 Hz
- 帧同步 + CRC 校验（HiPNUC 协议 0x91 / 0xA0）
- 输出：四元数 w/x/y/z、角速度 xyz、线加速度 xyz、温度
- 外参应用（`extrinsic_R`）：可以留给上层（`robot-app-atom01`）做，或者让 adapter 配置里带
- **非 RT 路径**：IMU 读取放 `robot-s0 io_thread` 即可，不进 rt_thread

**风险点**：
- UART 是字节流，driver 侧要做状态机做包同步，不能丢帧
- 500 Hz = 2ms/帧，io_thread 必须能跟上（普通优先级足够，但不要和日志线程耦合）

---

## 6. 关键不变量（★ 违反 = 机器人抽搐或起飞 ★）

1. **Obs 拼接顺序**必须和 §3.3 字节级对齐。训练侧任何一个 scale、任何一个位置的字段错位，策略输出全废。**强烈建议 `robot-app-atom01` 里加一个 `--dry-run` 模式 dump obs 向量，和训练 replay buffer 比特比较**。
2. **滑窗顺序**：最新帧在**末尾**（§3.4）。多数 IsaacLab 代码都是这个约定，但要确认。
3. **`usd2urdf` 和 `motor_sign`** 训练/部署一致。`motor_sign` 错一个关节直接反向。
4. **`action_scale` + `joint_default_angle`** 训练/部署一致。训练时通常 `target = a*0.25 + default_angle`，部署侧必须一样。
5. **闭链踝解耦的数学**：`close_chain_mapping.cpp` 里的 `get_forwardQVT` / `get_decoupleQVT` 包含机械常数（连杆长度等），必须逐行移植。**不要重新推导**，从原 C++ 复刻。
6. **`extrinsic_R` / IMU 到 body 变换**：`gravity_b` 方向错 = 倒地检测误触发 / policy 认为自己在飞。
7. **控制 / 推理周期**：训练侧 `dt=0.004, decimation=5`，部署侧**必须**完全相同。
8. **MIT 模式 kp/kd**：原栈每个关节不同（见 `robot.yaml.robot.kp/kd`），训练侧如果不是用同一套 kp/kd，策略不可迁移。
9. **紧急停机优先级**：`gravity_b.z > -0.5` → shutdown。在 robot-system 里应转化为 `robot-s0::EStop latch` + `robot-app-atom01` 发 EStop command。**不要依赖进程退出**（iceoryx2 + systemd 的行为不完全等价）。

---

## 7. 风险与门槛

| 风险 | 影响 | 缓解 |
|------|------|------|
| policy_host 调度抖动 > inline 调用 | 推理 20ms 抖到 30ms+ → 策略不稳定 | `chrt -f`、CPU affinity、预热 ORT session |
| SocketCAN write() 阻塞 | RT 线程 miss deadline | 非阻塞 fd + queue；或调 txqueuelen |
| 闭链解耦数值差异 | 踝关节抖 / 卡滞 | 移植前做单元测试：给定 motor_q → 期望 joint_q 与原 C++ 一致到 1e-6 |
| 观测位错位 | 机器人抽搐 / 直接倒 | dry-run dump + 和训练 replay 对拍 |
| 零点持久化 | 每次上电都要重标 | Damiao adapter 必须实现 `set_motor_zero`（写 EEPROM） |
| 训练侧非 Isaac / 非 `obs_num=78` 布局 | §3.3 不适用 | `robot-app-atom01` 里做可配置的 obs builder（YAML/TOML 描述字段顺序） |
| 多策略切换时 state 残留 | 模式切换瞬间输出异常 | 切换时强制 `is_first_frame_=true`、清零 last_act / motion_frame（原栈已这么做） |

---

## 8. 实施顺序（在 robot-system v0.4–v0.5 框架就位后）

> 这里**不是**新的 v0.x 步骤，只是"框架 ready 后这台机器人还差什么"。

```
1. robot-adapters/hipnuc   （先做 IMU，验证 io_thread + iceoryx2 状态流）
2. robot-adapters/damiao   （4 路 SocketCAN + MIT 模式；先单路在 bench 上跑通）
3. robot-policy-host       （onnxruntime-rs；不依赖任何特定 robot）
4. robot-app-atom01 骨架    （config 解析 + obs builder + dry-run dump）
5. 训练侧拉一条 replay，和 dry-run obs 对拍至比特一致
6. 闭链解耦 port + 单元测试（和 C++ 原栈对拍）
7. bench 测：单电机 → 6 电机 → 23 电机逐步加
8. 真机小位移实验：kp 减半、clip_actions 减半
9. 全策略跑通（normal / amp / attn_enc / beyondmimic / interrupt）
10. 替换原 atom01_deploy 作为正式部署栈
```

**退路**：任何一步失败，原 `atom01_deploy` 仍在 `~/repo/robot/atom01_deploy` 可跑。迁移是**可逆的 A/B**，不是一次性切换。

---

## 9. 参考

| 资源 | 位置 |
|------|------|
| 原上游仓库 | https://github.com/Roboparty/atom01_deploy.git |
| 本地仓库位置 | `~/repo/robot/atom01_deploy` |
| inference_node 主逻辑 | `atom01_deploy/src/inference/src/inference_node.{hpp,cpp}` |
| Obs / action pipeline 源码 | `inference_node.cpp` L136-286 |
| 启动脚本 / Fast DDS profile | `atom01_deploy/tools/start_robot.sh`、`assets/rt_fastdds_profile.xml` |
| `RobotInterface`（硬件封装） | `atom01_deploy/src/inference/src/robot_interface.{cpp,hpp}` |
| 闭链解耦 | `atom01_deploy/src/inference/src/utils/close_chain_mapping.{hpp,cpp}` |
| 达妙驱动 | `atom01_deploy/src/motors/` |
| HiPNUC 驱动 | `atom01_deploy/src/imu/` |
| 原栈 CLAUDE.md | `atom01_deploy/CLAUDE.md` |
| 达妙电机 ID 标定 | [RoboParty 文档](https://roboparty.feishu.cn/wiki/Sh5vw7QFZimO5Skxzficro2Pnkf) |
| HiPNUC 协议手册 | [HiPNUC 官网](https://www.hipnuc.com/resource_hi14.html) |

---

**最后更新**：2026-04-23（初稿，基于 `atom01_deploy` commit `d983373` 的代码状态）。
