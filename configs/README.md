# 当前配置

同一配置中的 `joints`、`motor_ids` 和逐电机容差按下标对应；轮位只在这里定义。更改配置后重新生成测试计划，不复用旧 GO。

全总线配置在所选 `[[hardware]]` 下使用 `serial_rt = { cpu = 4, fifo_priority = 70 }`（右侧CPU为5）。该段启用严格FIFO、绑核和锁页检查，要求产物启用`rt_guard`；CPU范围0..1023、FIFO范围1..99，实际核可用性由启动设置验证。调度设置失败拒绝就绪；与CLI的`--cpu`/`--serial-fifo`重复指定也拒绝。省略该段的旧配置仍可用原CLI。修改值在下次启动生效；systemd只提供权限和普通线程CPU范围，不复制执行线程的配置值。

| 用途 | 文件 | 边界 |
|---|---|---|
| 全总线硬件配置 | [bus1](xlerobot-bus1.toml)、[bus2](xlerobot-bus2.toml) | bus1管左臂、头部及头/左腕RGB；bus2管右臂、底盘及右腕RGB；默认入口实机验收见[当前状态](../docs/s0_hal_status.md) |
| 双侧 S1 入口 | [左侧服务](../scripts/systemd/robot-s1-bus1.service)、[右侧服务](../scripts/systemd/robot-s1-bus2.service) | 左侧沿 bus1 的八个位置槽和 `scripted` 后端，右侧沿 bus2 的六位置、三轮速槽和 `scripted`、`ik` 后端。每个 S1 读取同侧独立合成的配置。 |
| 左侧单关节运动片段 | [xlerobot-bus1-motion.toml](xlerobot-bus1-motion.toml) | 仅含左侧8关节的`[s1.single_joint]`约束；部署合成时同时读取当前bus1的`[s1.profile]`，不依赖旧S0配置包含S1后端；不是完整设备配置，不单独安装或自动使能 |
| 右侧单关节运动片段 | [xlerobot-bus2-motion.toml](xlerobot-bus2-motion.toml) | 右臂六个位置关节的约束由单关节和五轴 IK 共用。控制安装器与 bus2 的 `scripted`、`ik` 后端配置合成 S1 候选。IK 保持夹爪原位，三轮保持 raw 0 |
| 单关节软件夹具 | [single-joint-smoke.toml](single-joint-smoke.toml) | 两关节mock配置，只用于新载荷、限位、取消和到位测试；其中数值不是XLeRobot实机运动参数 |
| 包配置矩阵 | [package-matrix.toml](package-matrix.toml) | 选择Cargo包、feature、程序及归档文件；当前登记`xlerobot-orin-s0`，只在ARM64 Linux原生构建，不作为设备配置加载 |
| 状态与控制Web（默认只读） | [xlerobot-status.toml](xlerobot-status.toml) | 独立HTTP/WebSocket服务，共享bus1/bus2的同机gRPC连接；`[control].enabled=false`保持只读，无浏览器token；HTTP可指定IPv4私网地址，gRPC始终回环，不启动机器人服务，未纳入S0安装包 |
| 只读部署角色 | [rpc-roles-status.toml](rpc-roles-status.toml) | 候选网关使用空凭据表与viewer/read_state，沿既有GetState免令牌语义；不授予控制权限，不覆盖原rpc-roles.toml |
| 现有 arm-only 部署 | [左臂](xlerobot-left-arm.toml)、[右臂](xlerobot-right-arm.toml)、[RPC 角色](rpc-roles.toml) | 仍被服务、部署工具和受监督 demo 使用，保留路径 |
| 模拟与集成测试 | `xlerobot.toml`、`*-smoke.toml`、`clear-estop-e2e.toml` | 由对应测试明确选择；`xlerobot.toml` 是旧 schema/mock 示例，不是实机入口 |

只按明确文件安装或测试，不使用 `configs/**/*.toml` 批量加载。仍在执行的测试夹具保留稳定路径；退出使用的配置核对引用后直接删除，不另建归档或备份目录。新增配置先注明用途，不按轮次复制现有配置。

## 单关节运动约束

`[s1.single_joint]`是可选动作配置，缺失时拒绝新的单关节请求，原完整向量Scripted不受影响。当前S0使用的bus1/bus2均未添加；控制安装器只在独立候选生成同侧`xlerobot-bus1-s1.toml`或`xlerobot-bus2-s1.toml`供S1使用，不修改两份S0配置。参数为`max_duration_ms`、`settle_timeout_ms`和`stable_window_ms`，均须非零，稳定窗口严格小于到位超时；`[[s1.single_joint.joints]]`按`name`完整覆盖所选hardware的位置关节，明确`min_position_rad`、`max_position_rad`、`max_velocity_rad_s`和`tolerance_rad`，不提供默认运动限值。到位超时从末目标成功发布开始计时。

| 用途 | 配置或来源 | 本机行为 |
|---|---|---|
| S0 校准边界、S1 原位保持范围 | 同一 `hardware.calibration_range_tolerances_raw` | 两侧位置槽为 20 raw，轮槽为 0。非零容差只用于原校准端点派生的保持范围。主动目标仍受运动片段的原始范围限制。 |
| 网页使能前原位对齐 | 每个状态源的 `control.enable_alignment_tolerance_rad` | 对照新鲜反馈与命令镜像。本机候选约 4 raw。 |
| S1 到位与取消保持 | 每个位置关节的 `s1.single_joint.joints[].tolerance_rad` | 运动片段覆盖 ±20 raw，超出 20 raw 不接受。末目标发布后留 800 毫秒到位并连续稳定 200 毫秒。 |

五轴回位复用各关节的 `max_velocity_rad_s` 和 `tolerance_rad`。S1 当前固定五轴电机目标为 0 rad（2047 raw），网页模型偏移只影响显示。完成电机标零后，先核对主动目标范围和图示姿态的实测五轴值。[校准与回位说明](../docs/control_plan.md#单臂回初始位姿)

右侧九槽布局沿 `joints` 与 `motor_ids` 解析。单关节定位、回位及右臂 IK 都保持三轮 raw 0；三轮速度反馈按独立停止规则核验。运动片段和软件夹具仅用于其声明的用途。

## 按矩阵打包

用`uv run --no-project python scripts/build_package.py --list`列出选项；构建时指定`--package <选项>`、`--target-dir <隔离Cargo缓存绝对路径>`和`--output-dir <输出绝对路径>`。已有组件的包组合只改矩阵，新增硬件能力仍须实现和验证相应适配器。

打包要求源码干净，使用根Cargo版本、当前提交和实际nightly身份，核对Cargo输出的生产feature及ARM64 ELF。包包含程序、所选配置和unit模板、helper、VERSION及SHA清单；不包含校准、固定Python环境或保护状态。模板保留现有安装布局，须经候选冻结和现场预检后安装。命名为`robot-system-v<版本>+<7位提交SHA>-<选项>.tar.gz`，同名产物拒绝覆盖；生成包不创建Git标签或启动服务。

## Web控制候选

仓库[网页配置](xlerobot-status.toml)默认关闭运动控制，每个页面共用已有 gRPC、任务、相机和资源快照。安装器在候选目录生成同侧 S1 配置、Web 配置和内部凭据。候选类型决定安装范围：

- 左侧用 `prepare --left-control`，生成 bus1 控制配置，安装左侧 S1、左网关和同一 Web。
- 右侧用 `prepare --right-control --existing-control-candidate <已验收的左侧候选>`，沿用左侧设置，在同一 Web 开放 bus2 六个位置关节。安装范围为右侧 S1、右网关和 Web。正式底盘方向控制仍关闭。

控制候选的 `prepare` 由 unit 的服务用户执行。仅该服务用户可读取权限为 0600 的角色文件和环境文件，凭据留在机器人。文件安装、启停和实机验收是独立步骤。[安装与回滚](../scripts/systemd/README.md#状态服务安装与回滚) · [当前部署](../docs/s0_hal_status.md)
