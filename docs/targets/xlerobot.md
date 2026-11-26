# 部署目标：XLeRobot 双臂、头部与三轮底盘

XLeRobot 是通用控制架构的首个实机后端。本文件记录硬件对应关系、目标合同和实现入口。现场验收见[当前状态](../s0_hal_status.md)，开发顺序见[项目待办](../../TODO.md)。

实际调用链参考现有 XLeRobot Python 实现及本项目适配器；历史 dimos 路径仅用于追溯，不作为当前安装路径或自动迁移计划。整理本页不改变 §2 的安全与训练／部署合同。

## 1. 当前硬件与配置

| 部分 | 当前配置 |
|---|---|
| 主机 | NVIDIA Jetson Orin，Linux aarch64 |
| 左总线 | 左臂含夹爪 ID1–6，头部 ID7/8，共8电机，位置模式 |
| 右总线 | 右臂含夹爪 ID1–6；机器人自身右轮 ID7、前轮 ID8、左轮 ID9，共9电机，Position/Velocity 混合模式 |
| 相机 | 头部 D435i 的RGB接口；左腕、右腕USB彩色相机。深度和IMU尚未接入 |
| 运行组织 | 两个S0实例各管一条串口总线；相机由所属实例的SoftRT路径独立采集 |
| 调度 | 总线TOML分别指定CPU4/5、FIFO70；默认不上扭，保护保持锁存 |

共17个电机：双臂含夹爪12、头部2、底盘3。轮位以机器人自身左右为准，唯一来源为 [右总线配置](../../configs/xlerobot-bus2.toml) 的 `joints`/`motor_ids` 配对，不在文档或代码中建立第二份配置表。

左总线及头／左腕RGB见 [xlerobot-bus1.toml](../../configs/xlerobot-bus1.toml)，右总线及右腕RGB见 [xlerobot-bus2.toml](../../configs/xlerobot-bus2.toml)。设备路径、外部校准文件、分辨率和帧率直接读取配置；变更须重新生成计划，旧GO失效。

## 2. 训练 / 部署契约（★ target spec · 硬契约 ★）

> **本节每一条都是 spec**：改动必经 [`../CHANGELOG.md`](../CHANGELOG.md) 第 3 类条目（target-contract）登记。
> 下列保留已登记的 target spec；TBD 表示尚未冻结的训练／部署参数，不代表当前 raw 后端未实现。补全或修改须登记 CHANGELOG，不用本次文档整理改变合同。

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
  - 位置模式初始化序列（使能字、PID、max torque 等）；不得假定存在可写的 command watchdog 寄存器
  - 位置读 / 写的帧格式与期待响应
  - 异常帧（超时、overload、过热）的解析与分级
  - 硬件 fail-safe：STS3215 无已文档化、可配置的 command watchdog，历史真机失联仍持续带扭；主机串口 IO 超时不等于电机侧保护
- 安全边界沿已批 D15 / D15-R，原完整条款见[固定提交的候选 E Spec](https://github.com/arxxyr/robot-system/blob/37b6f26ff8d895ed78f9cd352649985c5d7decee/team-swe/specs/v0_5_dimos_replace.md)及 [CHANGELOG](../CHANGELOG.md)；下列为现行边界摘要，不替代原全文：
  - **D15 OPEN/DEFERRED，`deployable=false`**。AC-E25：Enabled 真机 `kill -9 robot-s0` 后实际 torque disable / zero / brake **<500 ms**，仍是生产替换、正式退役与无人值守前的硬门。通信 H2、systemd 重启、同进程 watchdog、启动 preflight 或软件停止不能替代这一证明。
  - **D15-R 仅为受监督 demo 例外**：机械隔离、工作区无人、无负载、操作者持续在场且可立即切总电源；`auto_enable=false`，每次上扭显式授权；先单臂低速/小幅，再双臂，任一侧异常立即取消双侧并双发 EStop。原单臂探针边界为 `|target-anchor|≤0.087 rad`、轨迹最大斜率 `≤0.14 rad/s`；当前 raw 维护动作必须另满足当前状态页的已批合同，不能互相外推授权。
  - 旧栈 unit 保留恢复路径，不得删除、mask 或破坏回滚；已批准关闭旧栈自启仅为 disable，不是正式退役。失联持续带扭或 `kill -9` 后仍带扭只能记为残余风险，不能记为 PASS，不宣称生产替换或硬件 fail-safe 已完成。
  - 独立保护硬件、供电分域与接线方案尚未选定，本项暂缓。后续仅保留文档级集成要求：`arm / heartbeat(kick) / request_safe(disarm) / status+readback`；接入 handoff/guard 与允许 Enable 之间，缺失、失联或 readback 失败必须拒绝 Enabled。不得用 no-op 实现或恒真 `power_ok` 冒充外部保护。
  - 恢复 D15 时先确认型号/固件、电源覆盖、默认态、机械后果和独立测量方案，再按已批方案实施及验收；原始故障起点、真实安全终点、时基与精度必须足以判断 <500 ms。未观测值记 UNKNOWN；本摘要不授权采购、接线、寄存器写入或现场测量。
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
4. §2.4 硬件 fail-safe：不得把主机 IO 超时或失联持位当作电机侧保护；沿 D15 / D15-R 保留 OPEN 与受监督 demo 边界，正式退役前须补齐保护方案及 AC-E25 实测。
5. §2.5 软限位双层：下位机寄存器 + 上位机每帧检查，任一层触发都 → EStop。
6. §2.6 `cmd_vel` clip 训练 / 部署一致。
7. §2.7 视觉外参 / 分辨率 / 帧率训练 / 部署一致；外参改动**必须**走 target-contract 登记。
8. 底盘与手臂共总线时，Feetech 读写 pipeline 顺序训练 / 部署一致（顺序改变 → 时序改变 → 策略看到的状态 stamp 错位）。

---

## 3. 上游与当前入口

实现及排查先核对原版实际调用链、初始化、单位、编码、连接复用与停止顺序，不凭旧阶段规划推定当前能力。参考包括 XLeRobot 和历史 dimos 的 Feetech、校准、机械臂及底盘逻辑；现有 raw 经验合同仅覆盖当前状态页记录的固件、Phase与命令。

| 职责 | 本仓库实际入口 |
|---|---|
| Feetech协议、校准与适配 | [robot-adapters/feetech](../../crates/robot-adapters/feetech/) |
| 总线执行与分组 | [serial_tick.rs](../../crates/robot-s0/src/serial_tick.rs)、[grouped_position.rs](../../crates/robot-s0/src/grouped_position.rs) |
| RGB采集与帧槽 | [camera.rs](../../crates/robot-s0/src/camera.rs)、[camera_capture.py](../../scripts/camera_capture.py)；复用V4L2/OpenCV入口，无独立相机crate |
| 运动学 | [机型运动学库](../../crates/robot-kinematics-xlerobot/)提供手臂 IK 与三轮数学模型。网页可按侧计算腕点预览，右臂执行路径使用 S1 九槽混合总线 |
| 标定与模型 | [网页控制说明](../control_plan.md)区分电机零位和行程、URDF 显示对齐，以及 S1 五轴回位目标 |
| 观测 | [robot-viz](../../crates/robot-viz/)；读取状态与RGB镜像，不参与执行器控制 |
| 维护与部署 | [维护入口](../../scripts/run_hardware_maintenance.py)、[systemd说明](../../scripts/systemd/README.md) |

## 4. 在通用架构中的位置

`S1 → S0 HAL → 硬件适配器 → 设备`。`robot-core`定义契约，`robot-s0`负责设备组织、分组执行和生命周期；协议留在Feetech适配器，相机走独立SoftRT路径。RPC网关属于上层，不是S0启动依赖。

当前每条总线保留唯一串口所有者、SerialRT writer、命令流、控制权与停止域；右臂和底盘共用总线，分组不产生第二个所有者。实际入口及服务名以配置与部署说明为准，不另建 `robot-app-xlerobot`、相机crate或迁移框架。

## 5. 后续范围

双侧 S1 已完成受监督网页单关节控制验收。标定、实际回位和右臂 IK 的实机验证顺序见[项目待办](../../TODO.md)。

## 6. 未闭环边界

- D15仍为`OPEN/DEFERRED`、`deployable=false`；软件停止、systemd重启和受监督测试不替代独立硬件失活保护。
- 已架空核对单轮 −500 raw 转向和四种方向组合。地面行进距离、停车距离及 SI 里程计仍待验证。
- 相机RGB已接入；启动恢复分支的软件通过不能证明USB失流根因或长期稳定性解决。
- 两侧分别验收不代表双总线同时动作通过；无现场确认不追加动作。

## 7. 参考

- [冻结架构设计](../00_design.md)、[当前状态](../s0_hal_status.md)、[目标合同登记规则](README.md)。
- [XLeRobot上游仓库](https://github.com/Vector-Wangel/XLeRobot)、[dimos上游仓库](https://github.com/dimos-robotics/dimos)。本次仅整理已有实现与参考，不声称核验最新上游版本。
- §2中的本地dimos路径保留其历史合同来源含义；当前真实配置和安装路径以本仓库配置及systemd说明为准。
