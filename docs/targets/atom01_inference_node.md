# ATOM01 `inference_node` 完整逻辑拆解

> 配套文档：[`atom01.md`](atom01.md)
> 目的：给迁移到 `robot-system` 的 `robot-app-atom01` 提供一份**可精确复刻的源码解读**。
> 上游代码位置：`/home/loosqk/repo/robot/atom01_deploy/src/inference/`（commit `d983373`）。

---

## 0. 代码位置速查

| 文件 | 作用 | 行数 |
|------|------|------|
| `src/inference/src/inference_node.hpp` | 类定义、成员、构造函数 | 257 |
| `src/inference/src/inference_node.cpp` | 推理/控制线程 + main | 324 |
| `src/inference/src/ros_interface.cpp` | ROS 回调 + 服务 + 配置加载 | 449 |
| `src/inference/src/robot_interface.cpp` | 硬件层封装（IMU + 电机 + 闭链） | 294 |

下文所有行号引用 = 这些文件的行号。

---

## 1. 线程模型（四个角色）

```
┌────────────────────────────────────────────────────────────────┐
│ main thread            SCHED_FIFO prio=50                       │
│   mlockall(MCL_CURRENT | MCL_FUTURE)                            │
│   MultiThreadedExecutor(2) 跑所有 ROS 回调：                    │
│     · sub /joy (状态机 + cmd_vel 写)                            │
│     · sub /cmd_vel                                              │
│     · sub /elevation_data  (attn_enc)                           │
│     · sub /joint_ref_states (interrupt)                         │
│     · 10 个 /std_srvs/Trigger 服务                              │
├────────────────────────────────────────────────────────────────┤
│ inference_thread       SCHED_FIFO prio=70                       │
│   周期 = dt * decimation  （0.004 × 5 = 20 ms → 50 Hz）         │
│   读 IMU → 读电机 → 拼 obs → ORT Run → 更新 act_                │
├────────────────────────────────────────────────────────────────┤
│ control_thread         SCHED_FIFO prio=70                       │
│   周期 = dt         （4 ms → 250 Hz）                           │
│   last_act_ = act_alpha * act_ + (1-act_alpha) * last_act_      │
│   robot_->apply_action(last_act_)                               │
├────────────────────────────────────────────────────────────────┤
│ ThreadPool(4)          普通优先级                               │
│   每路 CAN 一个 worker，并行读电机状态 / 发 MIT 命令            │
└────────────────────────────────────────────────────────────────┘
```

`main()` 里 `mlockall`（L290）+ `SCHED_FIFO prio=50`（L295）确保页表锁在内存、主线程调度优先。两个工作线程 prio=70 比 main 高（因为它们是硬实时路径）。

---

## 2. 启动构造流程（`InferenceNode::InferenceNode`，`.hpp` L31–L131）

```
1. load_config()                                   # ros_interface.cpp L3
2. robot_ = make_shared<RobotInterface>(ROOT_DIR + "config/robot.yaml")
     └─ setup_imu()        （HiPNUC 串口）
     └─ setup_motors()     （遍历 can0..canN 创建 23 个 MotorDriver）
     └─ 解析 extrinsic_R → extrinsic_q_inv_
     └─ 解析 close_chain_motor_id → close_chain_motor_idx_ / close_chain_joint_idx_
     └─ ThreadPool(4)
     └─ 预分配 joint_q_ / joint_vel_ / joint_tau_ / motor_target_ （全 0）
3. 创建 Ort::Env（线程配置：intra_threads_ 个 session 内线程）
4. setup_model(normal_ctx_, "models/policy.onnx",
               obs_num*frame_stack [+ perception_obs_num if attn_enc])
5. 若 use_beyondmimic_：
     for 每个 motion 模型：setup_model(motion_ctx)
     for 每个 motion npz ：MotionLoader(path)
6. 把 active_ctx_ 指向 normal_ctx_
7. 预分配所有 obs/act/joint 缓冲 + JointState 消息 name 字段
8. 调 reset() 初始化状态（act_ 置为 joint_default_angle_）
9. 创建 subscribers / publishers（Best-Effort + KeepLast(1) + volatile）
10. 启动 inference_thread_ + control_thread_
11. 注册 10 个 ROS 服务
```

⚠️ **电机此时还没 init**（`is_init_ = false`）。要等用户按手柄 X 或调 `/init_motors`。

---

## 3. `setup_model()` 干了什么（`inference_node.cpp` L3–L55）

每个 `ModelContext` 持有一个 ORT session + 预分配的 input/output 张量：

```cpp
Ort::SessionOptions opt;
opt.DisablePerSessionThreads();           // 共享全局线程
opt.EnableCpuMemArena();                  // 内存 arena，减少分配
opt.EnableMemPattern();                   // 记录 tensor 分配模式
opt.SetGraphOptimizationLevel(ORT_ENABLE_ALL);

ctx->session = make_unique<Ort::Session>(*env_, model_path, opt);
ctx->input_buffer.resize(input_size);     // 连续 float 数组
ctx->output_buffer.resize(joint_num_);    // 23

// 把 buffer 地址直接当成张量 → ORT 运行时零拷贝
ctx->input_tensor  = Value::CreateTensor(memory_info, input_buffer.data(), ...);
ctx->output_tensor = Value::CreateTensor(memory_info, output_buffer.data(), ...);
```

**关键**：`input_buffer` / `output_buffer` 是 `vector<float>`，一旦 `resize` 就不再动地址，ORT 每次 `Run()` 直接读写这段内存。所以推理热路径上**不分配内存**。

**Rust 侧移植注意**：`ort` crate 的 `SessionBuilder` + `Value::from_array` 也支持类似的绑定写法，但要手动管理 lifetime（input 张量持有 `&mut [f32]` 的生命周期），建议封一个 `PolicyContext` 结构体把 session + buffer + tensor view 放一起。

---

## 4. 推理线程主循环（`inference_node.cpp` L136–L286）

### 4.1 进入节奏

```cpp
auto period = microseconds(dt_ * 1000 * 1000 * decimation_);  // 20 ms
while (rclcpp::ok()) {
    auto loop_start = steady_clock::now();
    if (!is_running_.load()) { sleep_for(period); continue; }
    // ... 下面逐步展开 ...
    auto elapsed = now() - loop_start;
    if (elapsed < period) sleep_for(period - elapsed);
    else RCLCPP_WARN("Inference loop overran! %lld us > %lld us", ...);
}
```

`is_running_` 是原子 bool，由手柄 B 键 / `/start_inference` 服务翻转。**暂停时策略不跑、不发命令，但 control 线程还在按 250 Hz 发"最近的 act_"**（即 default_angle，因为 reset 时 act_ 被清成 default）。

### 4.2 Beyondmimic 分支（L156–L169）

```cpp
if (is_beyondmimic_.load()) {
    int idx = current_motion_idx_;
    motion_pos_ = motion_loaders_[idx]->get_pos(motion_frame_);
    motion_vel_ = motion_loaders_[idx]->get_vel(motion_frame_);
    motion_frame_ += 1;
    if (motion_frame_ >= motion_loaders_[idx]->get_num_frames()) {
        motion_frame_ = motion_loaders_[idx]->get_num_frames() - 1;  // 卡住末帧
    }
    for (int i = 0; i < joint_num_; i++) {
        obs_[i + offset]              = motion_pos_[i];
        obs_[i + joint_num_ + offset] = motion_vel_[i];
    }
    offset += joint_num_ * 2;       // 23*2 = 46
}
```

从 `.npz` 里取当前帧的参考 pos/vel（已预转 USD→URDF 顺序），写到 obs 前 46 维。**末帧卡住**而不是循环，意味着"一次动作跑完就停在最后一帧"。

### 4.3 IMU 读 + 重力投影 + 倒地检测（L171–L189）

```cpp
read_imu();   // ang_vel_ = robot_->get_ang_vel();   quat_ = robot_->get_quat();

// 角速度直接入 obs
for (int i = 0; i < 3; i++) obs_[i + offset] = ang_vel_[i] * obs_scales_ang_vel_;
offset += 3;

// 重力从世界系旋到 body 系
Eigen::Quaternionf q_b2w(quat_[0], quat_[1], quat_[2], quat_[3]);  // w,x,y,z
Eigen::Vector3f gravity_w(0, 0, -1);
Eigen::Vector3f gravity_b = q_b2w.inverse() * gravity_w;

if (gravity_b.z() > gravity_z_upper_) {     // 默认 -0.5
    RCLCPP_FATAL("Robot fell down!");
    rclcpp::shutdown();
    return;                                  // 线程退出，control 也会跟着停
}

obs_[offset + 0] = gravity_b.x() * obs_scales_gravity_b_;
obs_[offset + 1] = gravity_b.y() * obs_scales_gravity_b_;
obs_[offset + 2] = gravity_b.z() * obs_scales_gravity_b_;
offset += 3;
publish_imu();
```

**关键点**：
- `quat_` 已经是 **body→world**（`RobotInterface::get_quat()` 里已经用 `extrinsic_q_inv` 把 IMU frame 转到 body frame 了，见 `robot_interface.hpp` L75–L87）。
- `ang_vel_` 同样已经是 body frame（`robot_interface.hpp` L88–L96 里 `extrinsic_R_mat_ * omega_imu`）。
- 直立时 gravity_b ≈ (0,0,-1)，z = -1。机器人躺下时 z 向上翻正 → 触发 FATAL。

### 4.4 cmd_vel（仅非 beyondmimic，L191–L197）

```cpp
if (!is_beyondmimic_.load()) {
    unique_lock lock(cmd_mutex_);
    obs_[offset + 0] = cmd_vel_[0] * obs_scales_lin_vel_;
    obs_[offset + 1] = cmd_vel_[1] * obs_scales_lin_vel_;
    obs_[offset + 2] = cmd_vel_[2] * obs_scales_ang_vel_;  // ← 注意 yaw 用 ang_vel_scale
    offset += 3;
}
```

`cmd_vel_` 由手柄回调（axes[3/4] + LT/RT）或 `/cmd_vel` topic 写入，取决于 `is_joy_control_`。

### 4.5 关节状态读 + 软限位（L199–L212）

```cpp
read_joints();  // joint_pos/vel/tau 从 robot_ 里拉一次（内部 mutex）

for (int i = 0; i < joint_num_; i++) {
    obs_[offset + i]              = (joint_pos_[usd2urdf_[i]] - joint_default_angle_[usd2urdf_[i]]) * obs_scales_dof_pos_;
    obs_[offset + joint_num_ + i] = joint_vel_[usd2urdf_[i]] * obs_scales_dof_vel_;
}

// 软限位（URDF 顺序）
for (size_t i = 0; i < joint_limits_.size() / 2; i++) {
    if (joint_pos_[i] < joint_limits_[2*i] || joint_pos_[i] > joint_limits_[2*i+1]) {
        FATAL("Joint i out of limit"); shutdown(); return;
    }
}
offset += joint_num_ * 2;
publish_joint_states();
```

**两件事容易混**：
- **写 obs 时用 `usd2urdf[i]` 索引** → 这里是把 URDF 顺序的关节值**重排成 USD 顺序**（policy 期望的输入顺序）。
- **软限位检查用 `i` 直接索引 joint_pos_** → 因为 `joint_pos_` 和 `joint_limits_` 都是 URDF 顺序。

这个顺序切换很容易写反，上游这里是对的，但移植时要小心。

### 4.6 Last action（L214–L217）

```cpp
for (int i = 0; i < joint_num_; i++) {
    obs_[offset + i] = active_ctx_->output_buffer[i];  // ← 上一帧的 ONNX 原始输出（未 scale）
}
offset += joint_num_;
```

这里**用 `output_buffer`，不是 `act_`**。意思是：feed 给 policy 的"上一帧动作"是 policy 自己原始输出，**没有经过 action_scale / default_angle / usd2urdf 任何变换**。训练侧也必须这样记录。

### 4.7 Interrupt 标志位（L219–L222，可选）

```cpp
if (use_interrupt_) {
    obs_[offset] = is_interrupt_.load() ? 1.0 : 0.0;
    offset += 1;
}
```

Policy 能感知"现在上肢是否被外部接管"。

### 4.8 整体 clip（L224–L226）

```cpp
transform(obs_.begin(), obs_.end(), obs_.begin(),
          [this](float v) { return clamp(v, -clip_observations_, clip_observations_); });
```

全 obs 截到 ±100（默认）。

### 4.9 滑窗装填 input_buffer（L228–L247）

```cpp
bool bm = is_beyondmimic_.load();
int obs_num     = bm ? motion_obs_num_     : obs_num_;       // 121 vs 78
int frame_stack = bm ? motion_frame_stack_ : frame_stack_;   // 1 vs 10

if (is_first_frame_) {
    // 冷启动：把当前 obs 复制到所有 F 个槽位
    for (int i = 0; i < frame_stack; i++)
        copy(obs_.begin(), obs_.end(),
             active_ctx_->input_buffer.begin() + i * obs_num);
    if (use_attn_enc_) {
        lock perception_mutex_;
        copy(perception_obs_.begin(), perception_obs_.end(),
             active_ctx_->input_buffer.begin() + frame_stack * obs_num);
    }
    is_first_frame_ = false;
} else {
    // 正常帧：左移一格 + 最新帧放末尾
    //   buf[0 .. (F-1)*obs]  ← buf[obs .. F*obs]
    //   buf[(F-1)*obs .. F*obs]  ← obs_
    copy(input_buffer.begin() + obs_num,
         input_buffer.begin() + frame_stack * obs_num,
         input_buffer.begin());
    copy(obs_.begin(), obs_.end(),
         input_buffer.begin() + (frame_stack - 1) * obs_num);
    if (use_attn_enc_) { ... 同上 ...  }
}
```

**⚠️ 训练-部署契约**：
- 最新帧**在末尾**（高地址），最老帧在开头。
- `perception_obs` 不滑窗，每帧覆盖写到堆叠观测**之后**的区域（`[F*obs_num .. F*obs_num + perception_obs_num]`）。

### 4.10 ONNX 推理（L249–L251）

```cpp
active_ctx_->session->Run(
    RunOptions{nullptr},
    active_ctx_->input_names_raw.data(),  active_ctx_->input_tensor.get(),  active_ctx_->num_inputs,
    active_ctx_->output_names_raw.data(), active_ctx_->output_tensor.get(), active_ctx_->num_outputs);
```

单输入单输出假设（names_raw 是预构造的 `const char*` 数组）。推理结果直接落到 `output_buffer`。

### 4.11 Action 后处理（L253–L267）

```cpp
{
    unique_lock lock(act_mutex_);
    for (int i = 0; i < active_ctx_->output_buffer.size(); i++) {
        // 1. clip output
        active_ctx_->output_buffer[i] = clamp(output_buffer[i], -clip_actions_, clip_actions_);
        // 2. USD 顺序 → URDF 顺序
        act_[usd2urdf_[i]] = active_ctx_->output_buffer[i];
        // 3. scale + 加中位角（URDF 顺序下做）
        act_[usd2urdf_[i]] = act_[usd2urdf_[i]] * action_scale_ + joint_default_angle_[usd2urdf_[i]];
    }
    // 4. 可选：interrupt 覆盖 act_ 末尾 10 维
    if (use_interrupt_ && is_interrupt_.load()) {
        unique_lock lock2(interrupt_mutex_);
        for (size_t i = 0; i < interrupt_action_.size(); i++) {
            act_[act_.size() - interrupt_action_.size() + i] = interrupt_action_[i];
        }
    }
    publish_action();
}
```

**两步 scale 关键点**：
- `act_[usd2urdf_[i]] = output[i]` → 这一步相当于 `act_in_urdf = permute(output_in_usd)`。
- `act_ = act_ * action_scale + joint_default_angle` → 缩放 + 叠加中位角。

**这两步在同一行做容易读错**：代码写成 `act_[usd2urdf_[i]] = act_[usd2urdf_[i]] * action_scale_ + ...`，其实是先赋值再就地缩放。移植时要拆开写更安全。

### 4.12 超时警告（L279–L283）

```cpp
if (sleep_time > 0us) sleep_for(sleep_time);
else RCLCPP_WARN("Inference loop overran! Took %lld us, period %lld us", elapsed, period);
```

超时不退出、不降频，**继续下一帧**。意味着 20 ms 周期是软实时边界。

---

## 5. 控制线程（`inference_node.cpp` L109–L134）

```cpp
void control() {
    pthread_setname_np(pthread_self(), "control");
    SCHED_FIFO prio=70;
    auto period = microseconds(dt_ * 1e6);  // 4 ms

    while (rclcpp::ok()) {
        auto start = now();
        apply_action();                     // ← 下面看
        auto elapsed = now() - start;
        if (elapsed < period) sleep_for(period - elapsed);
    }
}

void apply_action() {
    if (!is_running_.load() || !robot_->is_init_.load()) return;
    {
        unique_lock lock(act_mutex_);
        for (size_t i = 0; i < act_.size(); i++) {
            last_act_[i] = act_alpha_ * act_[i] + (1 - act_alpha_) * last_act_[i];
        }
    }
    robot_->apply_action(last_act_);
}
```

**三个点**：
- **EMA 平滑**：`act_alpha=1.0` 即无平滑（`last_act = act_`）；减小可以防抖。
- **推理 / 控制解耦**：控制线程每 4ms 跑一次，但 act_ 每 20ms 才更新，所以一个新 act 会被重复发 5 次（decimation=5）。这相当于 zero-order hold，是 RL 里标配。
- **未跑推理时行为**：暂停或电机未 init → 直接 return，**不发任何 CAN 命令**。但电机收不到命令超 timeout 会自己进 damping / 失能（达妙固件行为），这是软件安全的第二层。

---

## 6. `robot_->apply_action(last_act_)` 内部（`robot_interface.cpp` L97–L167）

这是每 4ms 调一次的硬件层，要仔细看。

### 6.1 读所有电机状态（并行）

```cpp
exec_motors_parallel([this](MotorDriver& motor, int idx) {
    joint_q_[motor2urdf_[idx]]   = motor.get_motor_pos() * motor_sign_[idx];
    joint_vel_[motor2urdf_[idx]] = motor.get_motor_spd() * motor_sign_[idx];
    joint_tau_[motor2urdf_[idx]] = motor.get_motor_current() * motor_sign_[idx];
    if (motor.get_response_count() > offline_threshold_)   // 25
        throw runtime_error("Motor offline");
});
```

**4 路 CAN → 4 个线程并行**（ThreadPool）。`motor2urdf_` 把 motor 物理顺序（按 CAN 插入顺序）重排到 URDF 顺序。`motor_sign` 是 ±1，用来翻转安装方向。

### 6.2 闭链踝解耦（左右踝各一次）

```cpp
if (!close_chain_joint_idx_.empty()) {
    // ---- 左踝 ----
    VectorXd q(2), vel(2), tau(2);
    int idx1 = close_chain_joint_idx_[0], idx2 = close_chain_joint_idx_[1];
    q << joint_q_[idx1], joint_q_[idx2];
    vel << joint_vel_[idx1], joint_vel_[idx2];
    tau << joint_tau_[idx1], joint_tau_[idx2];
    ankle_decouple_->get_forwardQVT(q, vel, tau, /*is_left=*/true);
    // 覆盖 joint_q/vel/tau 为虚拟关节空间的值
    joint_q_[idx1] = q[0];   joint_q_[idx2] = q[1];
    joint_vel_[idx1] = vel[0]; joint_vel_[idx2] = vel[1];
    joint_tau_[idx1] = tau[0]; joint_tau_[idx2] = tau[1];

    // 计算踝关节虚拟空间下的 PD 力矩
    tau << kp[motor_idx0] * (action[idx1] - q[0]) + kd[motor_idx0] * (0 - vel[0]),
           kp[motor_idx1] * (action[idx2] - q[1]) + kd[motor_idx1] * (0 - vel[1]);

    // decouple 回电机空间力矩
    ankle_decouple_->get_decoupleQVT(q, vel, tau, true);
    action[idx1] = tau[0];  // 注意：覆盖 action 向量的该位置为"要发的电机力矩"
    action[idx2] = tau[1];

    // ---- 右踝 ---- (idx 2,3 + is_left=false，逻辑完全对称)
}
```

**这段很绕**，要理清三件事：
1. **`get_forwardQVT`**：电机空间（两个电机的 q/vel/tau）→ 虚拟关节空间（踝 pitch / roll）。读状态时用。
2. **`get_decoupleQVT`**：虚拟空间目标力矩 → 电机空间力矩。写命令时用。
3. **PD 在虚拟空间做**：`τ_virtual = kp*(target_virtual - q_virtual) + kd*(0 - vel_virtual)`，然后再 decouple 回电机空间。这比在电机空间直接做 PD 更符合机器人控制直觉（目标和 kp/kd 都是针对"关节"而非"电机"）。

### 6.3 生成 `motor_target_`（URDF → motor 顺序）

```cpp
{
    unique_lock lock(motors_mutex_);
    for (size_t i = 0; i < motor_target_.size(); i++) {
        motor_target_[i] = action[motor2urdf_[i]];
    }
}
```

把 URDF 顺序的 action 重排成 motor 物理顺序。闭链电机在这里拿到的是**力矩**（前面覆盖过），非闭链电机拿到的是**位置目标**。

### 6.4 并行发 MIT 命令

```cpp
exec_motors_parallel([this](MotorDriver& motor, int idx) {
    if (idx 不是闭链电机) {
        // 位置模式 MIT：给 target pos + kp/kd
        motor.motor_mit_cmd(motor_target_[idx] * motor_sign_[idx],
                            0.0f,                   // vel
                            kp_[idx], kd_[idx],
                            0.0f);                  // τ
    } else {
        // 纯力矩模式：给 τ（已经是 decouple 后的电机空间力矩）
        motor.motor_mit_cmd(0.0f, 0.0f, 0.0f, 0.0f,
                            motor_target_[idx] * motor_sign_[idx]);
    }
});
```

**MIT 协议参数**：`τ_out = kp*(pos_target - pos_now) + kd*(vel_target - vel_now) + τ_ff`，所以非闭链电机靠 kp/kd 做位置跟随，闭链电机靠前馈力矩。

---

## 7. Gamepad 回调（`ros_interface.cpp` L108–L219）

### 7.1 摇杆 → cmd_vel（仅 is_joy_control_ 时）

```cpp
if (is_joy_control_) {
    cmd_vel_[0] = clamp(axes[4] * clip_cmd_[1], clip_cmd_[0], clip_cmd_[1]);  // 前后
    cmd_vel_[1] = clamp(axes[3] * clip_cmd_[3], clip_cmd_[2], clip_cmd_[3]);  // 左右
    if (axes[2] < 0)      cmd_vel_[2] = clamp(-axes[2] * clip_cmd_[5], ...);  // LT 左转
    else if (axes[5] < 0) cmd_vel_[2] = clamp(axes[5] * clip_cmd_[5], ...);   // RT 右转
    else                  cmd_vel_[2] = 0;
}
```

注意摇杆映射：**axes[4] 是前后，axes[3] 是左右**（很多 joy_node 配置是 [0]=左右 [1]=前后，这里用了 axes[3]/[4]，应该是右摇杆的 x/y）。LT / RT 是半轴（-1→1），只有按下（<0）才触发。

### 7.2 按键（全部上升沿触发）

| button idx | 按键 | 前置 | 动作 |
|-----------|------|------|------|
| 0 | A | 电机已 init | 暂停推理 + reset_joints（软过渡到 default_angle） |
| 1 | B | — | 翻转 `is_running_` |
| 2 | X | — | 暂停推理 + 翻转电机 init/deinit |
| 3 | Y | — | 翻转 is_joy_control_（joy ↔ /cmd_vel） |
| 4 | LB | use_interrupt \|\| use_beyondmimic | 翻转 is_interrupt_ 或 is_beyondmimic_（切换时清 buffer + 暂停推理 50ms 再恢复） |
| 5 | RB | use_beyondmimic 且不在 bm 模式 | `current_motion_idx_ = (+1) % motions.size` |

**LB 切换的细节（L156–L203）**：
```cpp
// 1. 记住推理是否在跑
bool restore = is_running_.load();
// 2. 暂停推理
is_running_.store(false);
// 3. 等 50ms 让推理线程退出当前循环
sleep_for(50ms);
// 4. 翻转状态
is_beyondmimic_.store(!is_beyondmimic_.load());
// 5. 切换 active_ctx_ / resize obs_ / 清所有 buffer / 置 is_first_frame_
active_ctx_ = is_bm ? motion_ctxs_[idx].get() : normal_ctx_.get();
obs_.resize(is_bm ? motion_obs_num_ : obs_num_);
fill all buffers with 0;
is_first_frame_ = true; motion_frame_ = 0;
// 6. 再等 50ms
sleep_for(50ms);
// 7. 如果之前在跑就恢复
if (restore) is_running_.store(true);
```

这段里 `sleep_for(50ms)` 在 ROS 回调线程里 —— 会阻塞 executor 50ms，但只是按键触发一次，可以接受。

### 7.3 `/cmd_vel` 回调（L221–L228）

```cpp
if (!is_joy_control_) {
    cmd_vel_[0] = clamp(msg->linear.x, clip_cmd_[0], clip_cmd_[1]);
    cmd_vel_[1] = clamp(msg->linear.y, clip_cmd_[2], clip_cmd_[3]);
    cmd_vel_[2] = clamp(msg->angular.z, clip_cmd_[4], clip_cmd_[5]);
}
```

直接从 Twist 消息写 cmd_vel_，clip 到配置范围。只有 `!is_joy_control_` 时生效。

### 7.4 `/joint_ref_states` 回调（interrupt，L239–L246）

```cpp
if (use_interrupt_ && is_interrupt_.load()) {
    unique_lock lock(interrupt_mutex_);
    for (size_t i = 0; i < interrupt_action_.size(); i++) {  // 默认 10
        interrupt_action_[i] = msg->position[i];
    }
}
```

外部节点（比如遥操作软件）发 10 个关节的目标，interrupt 模式开时覆盖 act_ 末尾 10 维（通常是双臂）。

---

## 8. ROS 服务（`ros_interface.cpp` L248–L418）

全是 `std_srvs/Trigger`（没有参数，只有 `success` + `message` 返回）：

| 服务 | 前置 | 实现 |
|------|------|------|
| `/init_motors` | `!is_init` | 并行 `motor.init_motor()` |
| `/deinit_motors` | `is_init` | 并行 `motor.deinit_motor()` |
| `/start_inference` | `!is_running` | 翻 `is_running_=true` |
| `/stop_inference` | `is_running` | 翻 `is_running_=false` |
| `/reset_joints` | `is_init && !is_running` | 2 步软过渡：先 kp/2.5 → 等 1s → 恢复 kp（`robot_interface.cpp` L169–L201） |
| `/set_zeros` | `is_init && !is_running` | 并行 `motor.set_motor_zero()`（写电机 EEPROM） |
| `/refresh_joints` | `is_init` | 并行 `refresh_motor_status()` + 等 1s + 再读状态 |
| `/read_joints` | `is_init` | 读一次 + publish `/joint_states` |
| `/read_imu` | — | 读一次 + publish `/imu` |
| `/clear_errors` | — | 并行 `motor.clear_motor_error()` |

**`/reset_joints` 的软过渡值得注意（`robot_interface.cpp` L187–L200）**：
```cpp
// 1. 用 kp/2.5 的低刚度缓慢到目标位置
exec_motors_parallel([&](motor, idx) {
    motor.motor_mit_cmd(target * sign, 0, kp/2.5, kd, 0);
});
// 2. 等 1 秒让机器人到位
sleep_for(1000ms);
// 3. 切回正常 kp
exec_motors_parallel([&](motor, idx) {
    motor.motor_mit_cmd(target * sign, 0, kp, kd, 0);
});
```

低刚度到位 → 高刚度锁住。粗暴但实用。

---

## 9. `reset()`（`inference_node.cpp` L57–L94）

按手柄 A 或暂停推理时调用：

```cpp
is_running_ = false;
is_interrupt_ = false;
is_beyondmimic_ = false;
obs_.resize(obs_num_);     // 切回普通模式的 size
active_ctx_ = normal_ctx_;
// 清 obs/joint/motion/cmd_vel/quat/ang_vel/input_buffer/output_buffer 为 0
// 把 act_ 和 last_act_ 置为 joint_default_angle_
is_first_frame_ = true;
motion_frame_ = 0;
// 清 interrupt_action / perception_obs
```

**这里 act_ 置成 default_angle 很关键**：暂停后 control 线程还在 250Hz 发 act_，这时发的就是"中位姿"，相当于机器人会自动回到默认姿态（低刚度下就是"软瘫"，高刚度下是"站住不动"）。

---

## 10. 配置字段全表（`ros_interface.cpp` L3–L105）

### 10.1 ROS parameters（`inference*.yaml`）

| 字段 | 类型 | 默认 | 含义 |
|------|------|------|------|
| `model_name` | string | `1.onnx` | `models/` 下文件名 |
| `motion_names` | `vector<string>` | — | beyondmimic 的 .npz 列表 |
| `motion_model_names` | `vector<string>` | — | beyondmimic 的 .onnx 列表 |
| `act_alpha` | float | 0.9 | EMA 平滑系数（1.0=不平滑） |
| `gyro_alpha` / `angle_alpha` | float | 0.9 | 似乎声明了但代码里没用到 |
| `intra_threads` | int | -1 | ORT 单 session 内线程数 |
| `use_interrupt` / `use_beyondmimic` / `use_attn_enc` | bool | false | 三个互斥特性 |
| `obs_num` / `motion_obs_num` / `perception_obs_num` | int | 78 / 121 / 187 | obs 维度 |
| `perception_obs_topic` | string | `elevation_data` | attn_enc 订阅的 topic |
| `frame_stack` / `motion_frame_stack` | int | 15 / 1 | 滑窗深度 |
| `joint_num` | int | 23 | ONNX 输出维度 |
| `decimation` | int | 10 | 控制/推理频率比（inference.yaml 里改成 5） |
| `dt` | float | 0.001 | 控制周期（inference.yaml 里是 0.004） |
| `obs_scales_*` | float | 1.0 | 五个 scale（lin_vel / ang_vel / dof_pos / dof_vel / gravity_b） |
| `clip_observations` / `clip_actions` | float | 100 / 18 | 截断 |
| `action_scale` | float | 0.3 | ONNX 输出缩放（inference.yaml 是 0.25） |
| `usd2urdf` | `vector<long>` | — | 23 元素 permutation |
| `clip_cmd` | `vector<double>` | — | 6 元素 [vx_lo,vx_hi, vy_lo,vy_hi, wz_lo,wz_hi] |
| `joint_default_angle` | `vector<double>` | — | 23 元素 |
| `joint_limits` | `vector<double>` | — | 46 元素 [lo,hi]×23 |
| `gravity_z_upper` | float | -0.5 | 倒地阈值 |

### 10.2 YAML 配置（`robot.yaml`，直接被 RobotInterface 解析）

- `imu.imu_id / baudrate / imu_type / imu_interface_type / imu_interface`
- `motors.motor_id / motor_interface / motor_num / motor_type / motor_model / motor_zero_offset / master_id_offset`
- `robot.kp / kd / motor_sign / close_chain_motor_id / urdf2motor / extrinsic_R`

---

## 11. 精华总结（移植时 **必须对齐** 的契约）

| # | 项 | 要点 |
|---|---|------|
| 1 | **Obs 拼接顺序** | 普通模式：ang_vel(3) + gravity_b(3) + cmd_vel(3) + Δq(23) + dq(23) + last_out(23) [+ interrupt(1)] = 78 [+1] |
| 2 | **Obs scale** | 5 个 scale 分别应用到对应字段 |
| 3 | **Clip 顺序** | 先拼 obs → clip_observations → 滑窗；ONNX 输出 → clip_actions → usd2urdf → scale → +default |
| 4 | **滑窗方向** | 最新帧在末尾，左移老帧 |
| 5 | **last_out 是未 scale 的原始 ONNX 输出** | 不是 act_ |
| 6 | **usd2urdf 两次应用** | 写 obs 时 (URDF→USD 反查)，接收 output 时 (USD→URDF 正查) |
| 7 | **Action = output * scale + default** | 在 URDF 顺序下做 |
| 8 | **Interrupt 覆盖末尾 N 维** | 默认 10 维 |
| 9 | **MIT 模式区分闭链 / 非闭链** | 前者纯力矩，后者位置+kp/kd |
| 10 | **PD 在虚拟关节空间做再 decouple** | 不要在电机空间直接 PD |
| 11 | **推理 20ms 控制 4ms** | decimation=5，zero-order hold |
| 12 | **倒地检测靠 gravity_b.z** | -1 直立，>-0.5 触发 |
| 13 | **暂停时 act_ = default_angle** | control 线程继续发，机器人保持中位 |
| 14 | **电机离线 = response_count > 25** | 硬抛异常 |

---

## 12. 迁移到 `robot-app-atom01` 的伪代码骨架

```rust
// robot-app-atom01/src/obs_builder.rs
pub struct ObsBuilder {
    obs_scales: ObsScales,           // 5 个 scale
    usd2urdf: [usize; 23],
    joint_default_angle: [f32; 23],
    joint_limits: [(f32, f32); 23],
    clip_observations: f32,
    use_interrupt: bool,
}

impl ObsBuilder {
    pub fn build(&self, input: &StateInputs) -> Result<ArrayVec<f32, 79>, ObsError> {
        let mut obs = ArrayVec::new();
        // 3 + 3 + 3 + 23 + 23 + 23 [+ 1]
        push_scaled(&mut obs, &input.ang_vel, self.obs_scales.ang_vel);
        push_scaled(&mut obs, &input.gravity_b, self.obs_scales.gravity_b);
        push_scaled(&mut obs, &input.cmd_vel, /* 前两 lin, 第三 ang */);
        for i in 0..23 {
            let j = self.usd2urdf[i];
            obs.push((input.joint_pos[j] - self.joint_default_angle[j]) * self.obs_scales.dof_pos);
        }
        for i in 0..23 {
            obs.push(input.joint_vel[self.usd2urdf[i]] * self.obs_scales.dof_vel);
        }
        obs.extend_from_slice(&input.last_output);   // 上一帧 ORT 输出
        if self.use_interrupt {
            obs.push(if input.is_interrupt { 1.0 } else { 0.0 });
        }
        clip_in_place(&mut obs, self.clip_observations);
        Ok(obs)
    }
}

// robot-app-atom01/src/frame_stack.rs
pub struct FrameStack<const F: usize, const O: usize> {
    buf: [[f32; O]; F],
    is_first: bool,
}

impl<const F: usize, const O: usize> FrameStack<F, O> {
    pub fn push(&mut self, obs: &[f32; O]) -> &[f32] {
        if self.is_first {
            self.buf.fill(*obs);
            self.is_first = false;
        } else {
            // 左移 + 末尾放新
            self.buf.rotate_left(1);
            self.buf[F - 1] = *obs;
        }
        unsafe { std::slice::from_raw_parts(self.buf.as_ptr() as *const f32, F * O) }
    }
}

// robot-app-atom01/src/action_pipeline.rs
pub fn postprocess(
    output: &[f32; 23],
    cfg: &PolicyCfg,
    interrupt: Option<&[f32; 10]>,
) -> [f32; 23] {
    let mut act = [0.0; 23];
    for i in 0..23 {
        let clipped = output[i].clamp(-cfg.clip_actions, cfg.clip_actions);
        act[cfg.usd2urdf[i]] = clipped * cfg.action_scale + cfg.joint_default_angle[cfg.usd2urdf[i]];
    }
    if let Some(intr) = interrupt {
        act[13..23].copy_from_slice(intr);
    }
    act
}
```

---

## 13. 移植期建议的对拍检查

| 检查项 | 方法 | 通过标准 |
|--------|------|----------|
| Obs 拼接 | `--dry-run` 模式 dump obs 向量，和训练 replay buffer 逐字段比较 | 所有位 1e-6 内一致 |
| Frame stack | feed 确定性的 obs 序列 10 次，dump input_buffer | 最末尾 F-1 个槽位 = 最近 F-1 次 obs，首槽 = 第 1 次 obs |
| Action scale | 把 output_buffer 人为设成全 1，检查 act_ 输出 | `act[usd2urdf[i]] == 1.0 * scale + default[usd2urdf[i]]` |
| 闭链 decouple | 给定典型 motor_q（比如机械零位 + 小扰动），调用 forward + decouple，和 C++ 原版对拍 | 1e-6 内一致（机械常数必须完全相同） |
| gravity_b 极性 | 手动摆机器人到直立、侧躺、倒立，dump gravity_b | 直立 ~(0,0,-1)，倒立 ~(0,0,+1) |
| 倒地阈值 | 在仿真里发人为 quat，看 FATAL 时机 | 与 C++ 原版触发点同 |

---

## 14. 闭链踝解耦数学详解（`close_chain_mapping.{hpp,cpp}`）

ATOM01 的踝关节是典型的**并联 4-bar 连杆**：两个电机（motor_id 5/6 左踝、11/12 右踝）共同驱动一个两自由度的虚拟关节（ankle pitch + ankle roll）。策略在"虚拟关节空间"输出目标，硬件层需要做两次坐标变换：

```
电机空间 (θ1, θ2)  ←──── 正运动学 FK ────  虚拟关节空间 (pitch, roll)
   (上三角 serial)   ────  逆运动学 IK  ──→    (并联 parallel)
```

### 14.1 机械参数（所有数值单位是 **毫米**）

```
l_bar      = 20           // 短连杆（motor 端）长度
l_rod[0]   = 180          // 长连杆 1
l_rod[1]   = 110          // 长连杆 2
l_spacing  = ±42.35       // 左踝 +42.35，右踝 -42.35（leftLegFlag 切换正负）
short_link_angle_0 = π    // 短杆初始角（rad）
long_link_angle_0  = 0    // 长杆初始角
```

**关键点 3D 坐标（零位时）**：

| 点 | 含义 | 坐标（mm） |
|----|------|-----------|
| `r_A1_0` | 第 1 短杆枢轴 | `(0, ±42.35, 180)` |
| `r_B1_0` | 第 1 短杆末端 | `(−20 cos 0, ±42.35, 180 − 20 sin 0) = (−20, ±42.35, 180)` |
| `r_C1_0` | 第 1 长杆末端（连到踝） | `(−20, ±42.35, 0)` |
| `r_A2_0` | 第 2 短杆枢轴 | `(0, ±42.35, 110)` |
| `r_B2_0` | 第 2 短杆末端 | `(20, ±42.35, 110)` |
| `r_C2_0` | 第 2 长杆末端 | `(20, ±42.35, 0)` |

这些数值来自机械设计，**移植时必须原样搬**，不要推导。

### 14.2 逆运动学（`inverse_kinematics`，L59–L160）

给定虚拟空间 `(q_pitch, q_roll)`，求两个电机角 `THETA = (θ1, θ2)`。

算法：
1. 对 `r_C`（长杆末端点）施加姿态变换 `R_y(pitch) * R_x(roll)`，得到旋转后位置 `r_C_i`。
2. 对每根短杆（i = 0, 1），解方程：
   ```
   |r_B_i - r_C_i|² = l_rod[i]²         // 长杆长度约束
   r_B_i = r_A_i + R_y(θ_i) * (r_B_0 - r_A_0)   // 短杆绕 A 点旋转
   ```
3. 展开后变成二次方程 `aθ² + bθ + c = 0` 的变形：
   ```
   a = r_C_i.x - r_A_i.x
   b = r_A_i.z - r_C_i.z
   c = (l_rod² - l_bar² - |r_C_i - r_A_i|²) / (2 * l_bar)
   discriminant = b²c² - (a²+b²)(c² - a²)
   θ_i = asin((bc + √discriminant) / (a²+b²))
   θ_i = a < 0 ? θ_i : -θ_i               // 分支选择
   ```
4. discriminant < 0（超出可达域）时打 warning 并令 `discriminant = 0`。

返回 `InsKinematicsResult`，包含所有中间点和 `THETA`。

**Rust 侧移植**：Eigen 的 `Vector3d` / `Matrix3d` → `nalgebra::Vector3<f64>` / `Matrix3<f64>`，或者直接用 `[f64; 3]` 加手写乘加。`asin` 前确保 `arg ∈ [-1, 1]`（加 clamp 防浮点误差）。

### 14.3 雅可比（`jacobian`，L164–L199）

计算电机空间 → 虚拟空间的雅可比 `J_ankle[0]` 和虚拟 → 电机的 `J_ankle[1]`。

公式推导（来自虚位移原理）：

```
s_unit = (0, 1, 0)                 // 踝 pitch 轴方向（body 系 Y）
J_x = [ r_rod[0]ᵀ,  (r_C[0] × r_rod[0])ᵀ ]     ∈ ℝ²ˣ⁶
      [ r_rod[1]ᵀ,  (r_C[1] × r_rod[1])ᵀ ]
J_theta = diag( s_unit · (r_bar[0] × r_rod[0]),     ∈ ℝ²ˣ²
                s_unit · (r_bar[1] × r_rod[1]) )
J_q = [ 0, 0;      0, 0;      0, 0;
        0, cos(q_pitch);
        1, 0;
        0, -sin(q_pitch) ]                           ∈ ℝ⁶ˣ²

J_Temp = J_x · J_q                                  ∈ ℝ²ˣ²

J_ankle[0] = J_Temp⁻¹ · J_theta     // motor → joint (用于速度变换)
J_ankle[1] = J_theta⁻¹ · J_Temp     // joint → motor (用于力矩变换)
```

注意 Eigen 代码里用 `PartialPivLU<Matrix2d>` 做 2x2 逆，Rust 侧可以直接硬写 2x2 解析逆（行列式不为零时 `1/det * [[d,-b],[-c,a]]`，2x2 维度下比 LU 更快且无分支）。

### 14.4 正运动学（`forward_kinematics`，L213–L269）—— 迭代求解

**没有闭式解**，用 Newton 迭代：

```
x_c_k = last_solution[leftLegFlag]    // 上次成功的 (pitch, roll)，或 (0, 0)
for count in 0..MAX_ITERATIONS (100):
    ik = inverse_kinematics(x_c_k[1], x_c_k[0], leftLegFlag)  // 注意 x_c_k[0]=pitch, x_c_k[1]=roll
    Jac = jacobian(ik.r_C, ik.r_bar, ik.r_rod, x_c_k[0])
    f_error = thetaRef - ik.THETA                       // 目标 θ - 当前 θ
    x_c_k += ALPHA(0.5) * Jac[0] * f_error              // damped Newton
    if |f_error| < TOLERANCE(1e-3): break
last_solution[leftLegFlag] = x_c_k                      // 缓存下次初值
```

**这里有几个坑**：

1. **状态机**：`last_solution_` 是一个 `std::map<bool, Vector2d>`，左右腿各存一份最近一次收敛解作为下次迭代的初始值。这使得**连续调用比冷启动快得多**（一般 3–5 次迭代收敛）。Rust 侧用 `[Vector2<f64>; 2]` 按索引 `leftLegFlag as usize` 访问即可。
2. **容差和步长**：`TOLERANCE = 1e-3`、`ALPHA = 0.5`、`MAX_ITERATIONS = 100`。步长 0.5 是 damped Newton，保证稳定；容差 1e-3 rad ≈ 0.057°。
3. **Jacobian NaN 检查**：当机械构型接近奇异点（例如过零位）时 `J_motor2Joint.hasNaN()` → 直接返回 `count=-1` 失败标记。RobotInterface 不检查这个返回值（潜在 bug），移植时应该检查并回退到上一帧解。
4. **符号约定**：`x_c_k[0]` 是 pitch，`x_c_k[1]` 是 roll。`inverse_kinematics(roll, pitch)` 参数顺序**恰好相反**，容易写反。

### 14.5 面向外部的 4 个方法

```cpp
get_forwardQVT(q, vel, tau, isLeft)
   // q 输入: (motor_θ1, motor_θ2)   → 输出: (pitch, roll)
   // vel: J_ankle[0] * vel
   // tau: J_ankle[1]ᵀ * tau          （注意 J_ankle[1] 不是 [0]！）

get_decoupleQVT(q, vel, tau, isLeft)
   // q 输入: (pitch, roll)          → 输出: (motor_θ1, motor_θ2)
   // vel: J_ankle[1] * vel
   // tau: J_ankle[0]ᵀ * tau
```

`vel` 和 `tau` 的变换矩阵是**互易**的（雅可比的对偶），这一点 Eigen 代码里是写对的。

### 14.6 调用序列（硬件层每帧的完整流程）

`RobotInterface::apply_action` 每 4ms 调一次（左右踝各一次，逻辑对称，这里只写左踝）：

```
1. 读电机状态 (motor_q, motor_vel, motor_tau)
2. get_forwardQVT(q, vel, tau, true):
     q   = forward_kinematics(motor_q)       → 虚拟空间 (pitch, roll)
     vel = Jac[0] * motor_vel
     tau = Jac[1]ᵀ * motor_tau
3. 在虚拟关节空间算 PD 力矩：
     τ_virtual_i = kp_i * (target_joint_i - q_i) + kd_i * (0 - vel_i)
4. get_decoupleQVT(q, vel, tau, true):
     q   = inverse_kinematics(pitch, roll)   → motor_θ
     vel = Jac[1] * vel                       (通常不用)
     tau = Jac[0]ᵀ * τ_virtual                → motor_τ
5. 把 motor_τ 写回 action 向量的踝关节槽位
6. 对这四个闭链电机，用**纯力矩模式** motor_mit_cmd(0,0,0,0, τ)
```

**这意味着非闭链电机和闭链电机的 MIT 命令参数语义完全不同**：
- 非闭链电机：`(pos_target, 0, kp, kd, 0)` → 电机固件算 `τ = kp*(pos_target - pos) + kd*(0 - vel) + 0`
- 闭链电机：`(0, 0, 0, 0, τ_ff)` → 电机直接输出 τ_ff

### 14.7 `reset_joints` 的特殊处理（`robot_interface.cpp` L170–L185）

复位时 target 是 default_angle（虚拟空间的），需要**先 decouple 到电机空间**：

```cpp
q << joint_default_angle[idx1], joint_default_angle[idx2];
ankle_decouple_->get_decoupleQVT(q, vel, tau, true);  // vel/tau 未用
joint_default_angle[idx1] = q[0];
joint_default_angle[idx2] = q[1];
```

然后**用位置模式**发（不是力矩模式），因为 reset 不需要动态力矩控制，只是缓慢到位。这是闭链电机**唯一用位置模式**的场景。

### 14.8 Rust 侧移植建议

- 一个 `Decouple` 结构体，持有 `last_solution: [Vector2<f64>; 2]`。
- 机械常数做成 `const`（不要读配置），避免和 C++ 原版差异。
- 单元测试：给定 10 组典型 `(pitch, roll)` → IK → FK → 检查回到原 `(pitch, roll)` 误差 < 1e-4。
- 对拍测试：生成 100 组 `(motor_q, motor_vel, motor_tau)`，调 `get_forwardQVT`，和 C++ 原版对比所有 24 个输出值（6 个向量 × 4 个点？不，是 `q[0], q[1], vel[0], vel[1], tau[0], tau[1]`），每个值 1e-6 内一致。
- **`last_solution` 初值**：如果机器人启动时电机已经不在零位（比如上电自检时被人扶正），冷启动第一次 FK 可能不收敛。建议 `reset_joints` 后强制清一次 `last_solution`，让下次 FK 从更准确的 `(0,0)` 附近开始。

---

## 15. MotionLoader .npz 格式详解（`motion_loader.{hpp,cpp}`）

### 15.1 .npz 文件结构

一个 beyondmimic motion 文件（如 `motions/dance.npz`）是 NumPy 的标准 `.npz` 压缩归档，内含三个数组：

| key | dtype | shape | 含义 |
|-----|-------|-------|------|
| `fps` | `int` | 标量 / `(1,)` | 动作采样帧率 |
| `joint_pos` | `float32` | `(num_frames, num_joints)` | 每帧 23 关节目标位置 |
| `joint_vel` | `float32` | `(num_frames, num_joints)` | 每帧 23 关节目标速度 |

C++ 侧用 `cnpy::npz_load()` 加载（vendor 在 `thirdparty/cnpy-*`），按行优先逐元素拷贝到 `vector<vector<float>>`。

### 15.2 关节顺序（重要）

上游 `.npz` 来自 IsaacSim/USD 训练侧，**默认是 USD 关节顺序**。但 C++ `MotionLoader` 直接 `memcpy`，不做顺序转换 —— 这意味着：

- `atom01_deploy` 的 C++ 假设 **`.npz` 已经是 USD 顺序**，且在 `inference_node.cpp` L164-167 直接按 i 索引拿出来拼到 obs 的前 46 维（USD 顺序）。
- Python 脚本 `scripts/motion_player.py`（本仓库不在其中，是辅助工具）可以用参数 `usd2urdf=True` 额外转成 URDF 顺序：
  ```python
  joint_map = [0, 6, 12, 1, 7, 13, 18, 2, 8, 14, 19, 3, 9, 15, 20,
               4, 10, 16, 21, 5, 11, 17, 22]
  self.joint_pos[:, joint_map] = pos_usd   # USD → URDF 转换
  ```
- 在 C++ inference_node 里 motion 拼进 obs 是**不做转换**的，因为 policy 本来就期望 USD 顺序。

**Rust 侧选择**：保持 `.npz` 原样（USD 顺序），移植时写入 obs 的前 46 维时也不做转换。只有在"播放 motion 到真机（动作回放工具）"场景才需要做 `usd2urdf` 转换。

### 15.3 用法时序

```cpp
// 构造：加载全部帧到内存（num_frames * 23 * 2 * 4 bytes，几秒动作 < 10MB）
MotionLoader(motion_file);

// 推理线程每 20ms 取一帧
motion_pos_ = motion_loaders_[idx]->get_pos(motion_frame_);
motion_vel_ = motion_loaders_[idx]->get_vel(motion_frame_);
motion_frame_ += 1;
// 末帧卡住（见 inference_node.cpp L161-163）
```

**播放速率 = inference 周期 = 50 Hz**（默认 dt=0.004, decimation=5）。如果 `.npz` 的 `fps` 不是 50，实际会变速播放。上游代码里**没有做时间插值**，说明训练侧约定 motion `fps == 50`。

### 15.4 `get_pos` / `get_vel` 的语义

这两个方法返回**策略参考值**，不是直接的电机命令。拼进 obs 后，policy 会根据 `motion_pos - joint_pos` 的差异（策略隐式计算）来输出真正的 action。

### 15.5 Rust 侧移植

```rust
// Cargo.toml
// npyz = "0.8"   // 或 ndarray-npy

use npyz::npz::NpzArchive;

pub struct Motion {
    pub fps: u32,
    pub joint_pos: Vec<[f32; 23]>,   // 行优先连续存储
    pub joint_vel: Vec<[f32; 23]>,
}

impl Motion {
    pub fn load(path: &Path) -> Result<Self, MotionError> {
        let mut npz = NpzArchive::open(path)?;
        let fps = npz.by_name("fps")?
            .ok_or(MotionError::MissingKey("fps"))?
            .into_vec::<i32>()?[0] as u32;
        let pos_flat: Vec<f32> = npz.by_name("joint_pos")?.unwrap().into_vec()?;
        let vel_flat: Vec<f32> = npz.by_name("joint_vel")?.unwrap().into_vec()?;
        let n = pos_flat.len() / 23;
        // 切成 Vec<[f32; 23]>
        let joint_pos = pos_flat.chunks_exact(23).map(|c| {
            let mut a = [0f32; 23]; a.copy_from_slice(c); a
        }).collect();
        let joint_vel = vel_flat.chunks_exact(23).map(|c| {
            let mut a = [0f32; 23]; a.copy_from_slice(c); a
        }).collect();
        Ok(Self { fps, joint_pos, joint_vel })
    }
}
```

**注意**：`npyz` 或 `ndarray-npy` 解码 `.npz` 时要确认字节序（NumPy 默认小端，x86/aarch64 原生匹配，无需转换）。

### 15.6 容量评估

以 `dance.npz` 为例（假设 30s 动作 × 50fps = 1500 帧）：

```
1500 帧 × 23 关节 × 2 (pos+vel) × 4 字节 = 276 KB
```

可以全量常驻内存，不需要流式加载。`robot-app-atom01` 启动时一次性读所有 motions，后续播放零 I/O。

---

## 16. ONNX Runtime Session 细节（`setup_model` / `Session::Run`）

### 16.1 SessionOptions 每项的含义

```cpp
Ort::SessionOptions opt;
opt.DisablePerSessionThreads();           // (A)
opt.EnableCpuMemArena();                  // (B)
opt.EnableMemPattern();                   // (C)
opt.SetGraphOptimizationLevel(ORT_ENABLE_ALL);  // (D)
```

| 选项 | 效果 | 为什么用 |
|------|------|---------|
| (A) `DisablePerSessionThreads` | 所有 session 共用 `Ort::Env` 里的全局线程池 | ATOM01 可能有多个 session（normal + N 个 motion），如果每个都开一套线程池会爆炸。共用 + `Env` 里设 `intra_threads=1` → 单线程推理，延迟确定性更好 |
| (B) `EnableCpuMemArena` | 启用 ORT 自带的 CPU 内存 arena 分配器 | 推理中间张量的分配在 arena 里，避免反复 malloc/free；arena 复用内存 |
| (C) `EnableMemPattern` | 首次 Run 时记录各中间张量的内存布局，后续 Run 直接复用 | 前提：**输入 shape 必须固定**。对 ATOM01 是成立的（`[1, 78*10]` 或 `[1, 121]` 这种固定维度） |
| (D) `ORT_ENABLE_ALL` | 启用所有图优化（常量折叠、融合、dead-code elim 等） | 推理阶段不需要保留调试图结构，全开即可 |

**Rust `ort` crate 对应写法**：

```rust
use ort::{
    session::{builder::SessionBuilder, Session},
    environment::Environment,
    value::Value,
    tensor::OrtOwnedTensor,
    ExecutionProvider, GraphOptimizationLevel,
};

let env = Environment::builder()
    .with_name("atom01_policy")
    .with_intra_threads(1)
    .build()?
    .into_arc();

let session = SessionBuilder::new(&env)?
    .with_optimization_level(GraphOptimizationLevel::Level3)?
    .with_memory_pattern(true)?              // = EnableMemPattern
    .with_allocator(AllocatorType::Arena)?   // = EnableCpuMemArena
    .with_model_from_file(model_path)?;
```

### 16.2 Tensor 绑定机制（零拷贝）

C++ 原栈的关键技巧：

```cpp
ctx->input_buffer.resize(input_size);   // 预分配，地址固定
ctx->input_tensor = std::make_unique<Ort::Value>(
    Ort::Value::CreateTensor<float>(
        *ctx->memory_info,                     // OrtMemTypeCPU
        ctx->input_buffer.data(),              // 指向 vector 内部数据
        ctx->input_buffer.size(),
        ctx->input_shape.data(),               // [1, obs_num * frame_stack]
        ctx->input_shape.size()));             // rank = 2
```

意义：
- `Ort::Value` 只是一个"指向外部 buffer 的 view"，**不拥有数据**。
- 后续 `Run()` 时 ORT 直接读这个 view 指向的内存，**不拷贝**。
- 每帧只需要 `memcpy` 新 obs 到 `input_buffer`，不需要重建 `Value`。

**output 同理**：
```cpp
ctx->output_buffer.resize(joint_num_);  // 23
ctx->output_tensor = CreateTensor<float>(memory_info, output_buffer.data(), ...);
// Run 之后直接读 output_buffer[0..23]
```

**Rust `ort` 对应**：`Value::from_array` 会拷贝；零拷贝要用 `Value::from_raw` 或 `Input::bind_tensor`（视 ort crate 版本）。建议做法：

```rust
pub struct PolicyContext {
    session: Session,
    input_buffer: Vec<f32>,         // 预分配
    output_buffer: Vec<f32>,
    // tensor view 要在 Run 时重建（ort 0.x 不支持完全 persistent binding）
}

impl PolicyContext {
    pub fn run(&mut self, obs_flat: &[f32]) -> Result<&[f32], ort::Error> {
        self.input_buffer.copy_from_slice(obs_flat);
        let input = Value::from_array(
            self.session.allocator(),
            &Array2::from_shape_vec((1, self.input_buffer.len()),
                                     self.input_buffer.clone())?
        )?;
        let outputs = self.session.run(vec![input])?;
        let out: OrtOwnedTensor<f32, _> = outputs[0].try_extract()?;
        self.output_buffer.copy_from_slice(out.view().as_slice().unwrap());
        Ok(&self.output_buffer)
    }
}
```

**Rust 侧 ort 的现状**：
- `ort` 0.x 支持零拷贝输入（`CowArray` + `Value::from_array`），但用法比 C++ 冗长。
- `tract` 是纯 Rust 替代，零拷贝更自然，但对复杂模型（transformer）支持不如 ort。
- 对 ATOM01 这种 MLP / 轻量模型，**两者都可以**；建议先用 `ort`（能直接加载任何 IsaacLab 导出的 ONNX），性能不够再换。

### 16.3 `Run()` 函数签名的含义

```cpp
session->Run(
    Ort::RunOptions{nullptr},       // 运行时选项（超时、log 级别等）
    input_names_raw.data(),         // const char* const*  输入名字数组
    input_tensor.get(),             // const Ort::Value*  输入张量数组
    num_inputs,                     // size_t  输入数量
    output_names_raw.data(),        // const char* const*  输出名字数组
    output_tensor.get(),            // Ort::Value*  输出张量数组（预分配）
    num_outputs);                   // size_t  输出数量
```

**ATOM01 的简化假设**：
- 单输入单输出（`num_inputs == num_outputs == 1`），但代码写了通用循环。
- 输入名字从模型里 `GetInputNameAllocated(0)` 读出来，不硬编码。对 IsaacLab 导出的模型通常是 `"obs"` 或 `"input"`；输出通常是 `"actions"` 或 `"output"`。

### 16.4 性能 / 延迟调优建议

1. **`intra_threads=1` 是关键**：多线程会引入抖动，对 20ms 周期不友好。ATOM01 默认就是 1。
2. **模型规模**：ATOM01 的 `policy.onnx` 是几层 MLP（典型 [512, 256, 128] 或类似），单次推理在 aarch64 上 < 1ms。
3. **避免每帧重建 Value**：如 16.2 所述，C++ 原栈只建一次。Rust 侧如果做不到完全零拷贝，至少保证 `input_buffer` 不 resize。
4. **CPU 亲和性**：推理线程 + 控制线程绑到同一个物理核（不同超线程），CAN IO 线程绑到另一个物理核，避免缓存竞争。可以用 `sched_setaffinity` / Linux `taskset`。
5. **Warmup**：构造完 `Session` 后立即跑 1-2 次 `Run()` 并丢弃结果，触发 MemPattern 记录 + JIT 预热。`atom01_deploy` 没显式 warmup（`is_first_frame_` 的填充相当于隐式 warmup）；Rust 侧推荐显式做一次。
6. **模型量化**（未来）：FP16 或 INT8 量化可以把 MLP 推理降到 <100µs。ATOM01 原栈没做，因为 FP32 已经够快。

### 16.5 Session 生命周期

```
env_ = unique_ptr<Ort::Env>(...);   // 构造时一次
for each model:
    setup_model(ctx, path, size):
        ctx->session = make_unique<Ort::Session>(*env_, path, opt);
        // ctx->input_tensor/output_tensor 绑到 buffer
every 20ms:
    session->Run(..., input_tensor, ..., output_tensor)
析构时：
    ctx->session / env_ 自动释放（unique_ptr）
```

**注意 `env_` 必须比所有 `session` 活得久**（Ort::Session 持有对 Env 的引用），C++ 原栈用 `unique_ptr + 按声明顺序析构` 保证了这一点。Rust 用 `Arc<Environment>` 也可以（`ort` crate 就这样做）。

### 16.6 错误处理

C++ 原栈里 `Run()` 抛 `Ort::Exception`，被 `inference()` 的 `try/catch` 捕获后打 FATAL 并 `rclcpp::shutdown()`。Rust 侧应该：

- 第一次启动时（`setup_model`）若加载失败 → 不要启动进程（main 直接返回错误）。
- 运行时 `Run()` 失败 → 发 EStop command → 切到 SAFE 状态（沿用 `robot-s0` 的 EStop latch 机制）。

---

## 17. ThreadPool 的实时设置（`thread_pool.hpp`）

闭链解耦和电机读写都走 `exec_motors_parallel` → `ThreadPool::run_parallel`。这个线程池有几个细节值得搬到 Rust：

```cpp
ThreadPool(threads /*=4*/):
    for i in 0..threads:
        worker = std::thread([this] {
            pthread_setname_np("threadpool");
            struct sched_param sp{.sched_priority = 70};
            pthread_setschedparam(self, SCHED_FIFO, &sp);   // ★
            for(;;) wait + dequeue + run
        });
```

**关键点**：
- 所有 worker 线程都是 `SCHED_FIFO prio=70`，和 inference/control 线程**同优先级**。
- 这意味着 4 路 CAN 读写和主控制线程抢 CPU 时是公平的。
- **对 RT 核心的影响**：如果 ThreadPool 正好在 CAN 阻塞，会让 control 线程等 —— 这是为什么 `apply_action` 的 250Hz 偶尔会超时（`RCLCPP_WARN("overran!")`）。

**Rust 侧建议**：
- `robot-adapters/damiao` 里每路 CAN 开一个 `std::thread::Builder::spawn_with_sched_fifo(70)`（或用 `thread-priority` crate）。
- 或者把 4 路 CAN 统一到 `robot-s0` 的 `io_thread` + `rt_thread` 分工里，不再用独立 ThreadPool：
  - `io_thread`（普通优先级）：读 CAN，更新 `state/motors`
  - `rt_thread`（SCHED_FIFO 80+，比 worker 还高）：写 MIT 命令
- 这样就**不需要**一个专门的"同优先级 worker pool"，符合 robot-system 的 S0 线程分工。

### 17.1 `run_parallel` 的语义

```cpp
void run_parallel(const vector<function<void()>>& tasks) {
    vector<future<void>> futures;
    for (auto& task : tasks) futures.push_back(enqueue(task));
    for (auto& fut : futures) fut.wait();   // 等所有完成
    for (auto& fut : futures) fut.get();    // 收集异常（如有）
}
```

"扇出 + 全部收回"的 fork-join 模式。如果其中一个 task 抛异常（如"Motor offline"），`fut.get()` 重新抛出，传播到 `inference()` 的 try/catch。

Rust 侧可以用 `std::thread::scope` + `crossbeam::scope` 或直接 `rayon::scope`，更简洁且无分配。

---

## 18. 参考

| 资源 | 位置 |
|------|------|
| 父文档 | [`atom01.md`](atom01.md) |
| inference_node 主逻辑 | `atom01_deploy/src/inference/src/inference_node.{hpp,cpp}` |
| ROS 回调 / 配置加载 | `atom01_deploy/src/inference/src/ros_interface.cpp` |
| 硬件封装 | `atom01_deploy/src/inference/src/robot_interface.{cpp,hpp}` |
| 闭链解耦 | `atom01_deploy/src/inference/src/utils/close_chain_mapping.{hpp,cpp}` |
| Motion 加载 | `atom01_deploy/src/inference/src/utils/motion_loader.{hpp,cpp}` |
| Motion 播放脚本 | `atom01_deploy/scripts/motion_player.py`（含 USD↔URDF 转换示例） |
| 线程池 | `atom01_deploy/src/inference/src/utils/thread_pool.hpp` |
| ONNX Runtime C++ API | [`onnxruntime.ai/docs/api/c`](https://onnxruntime.ai/docs/api/c/) |
| Rust ort crate | [`docs.rs/ort`](https://docs.rs/ort) |
| cnpy (.npz 读取) | `atom01_deploy/src/inference/thirdparty/cnpy-*` |
| 原栈 CLAUDE.md | `atom01_deploy/CLAUDE.md` |

---

**最后更新**：2026-04-23（初稿，基于 `atom01_deploy` commit `d983373`）。
