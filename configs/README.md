# 当前配置

同一配置中的 `joints`、`motor_ids` 和逐电机容差按下标对应；轮位只在这里定义。更改配置后重新生成测试计划，不复用旧 GO。

全总线配置在所选 `[[hardware]]` 下使用 `serial_rt = { cpu = 4, fifo_priority = 70 }`（右侧CPU为5）。该段启用严格FIFO、绑核和锁页检查，要求产物启用`rt_guard`；CPU范围0..1023、FIFO范围1..99，实际核可用性由启动设置验证。调度设置失败拒绝就绪；与CLI的`--cpu`/`--serial-fifo`重复指定也拒绝。省略该段的旧配置仍可用原CLI。修改值在下次启动生效；systemd只提供权限和普通线程CPU范围，不复制执行线程的配置值。

| 用途 | 文件 | 边界 |
|---|---|---|
| 全总线硬件配置 | [bus1](xlerobot-bus1.toml)、[bus2](xlerobot-bus2.toml) | bus1管左臂、头部及头/左腕RGB；bus2管右臂、底盘及右腕RGB；默认入口实机验收见[当前状态](../docs/s0_hal_status.md) |
| 左侧S1软件入口 | [bus1](xlerobot-bus1.toml)、原运动片段与[robot-s1-bus1.service](../scripts/systemd/robot-s1-bus1.service) | 保持8关节顺序和现有scripted profile；显式左侧控制模式生成合成S1配置，同一进程处理多次任务。`Restart=no`，异常后确认断扭并人工恢复；运行状态见[当前状态](../docs/s0_hal_status.md) |
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

`joints[].tolerance_rad`是单关节到位容差的唯一配置，按关节分别填写；正常到位、取消保持及单关节执行走廊使用同一值。用户已授权到位误差在20raw内通过，两侧运动片段使用原Feetech f32换算覆盖±20raw的最大rad差，并严格小于21raw的最小rad差；精确数值以该TOML为准。维护demo已修复丢弃此配置并硬编码4raw的问题，Orin软件验证及左腕单次主动取消的±20raw配置容差实机复验均通过，证据见[当前状态](../docs/s0_hal_status.md#通讯与控制todo)。原运动范围、速率、800毫秒到位超时与200毫秒稳定窗口不变；默认hold及启用前锚点一致性仍沿原检查，历史±4raw自动FAIL不追改。

右侧混合总线的单关节请求只允许选择臂位置关节；运动片段仅覆盖六个位置槽，完整九槽布局沿同份`motor_ids`解析。三轮命令目标始终为raw 0，轮反馈不作为rad位置限位或到位依据，现有三轮停止复核保持。

实际部署仍须核对现有校准、产物和具体计划，S0继续执行原校准限制。Web使能原位、S1保持和单关节反馈复用同份hardware的`calibration_range_tolerances_raw`，缺省为0；非零容差只适用于由原raw校准精确换算的运动端点，按原转换函数派生保持范围，不裁剪实际位置。主动目标仍严格落在运动片段的原范围内，到位仍使用独立`tolerance_rad`；断扭、停速和其他停止判据不变。两份运动片段与测试夹具均不得作为完整生产配置安装。

## 按矩阵打包

用`uv run --no-project python scripts/build_package.py --list`列出选项；构建时指定`--package <选项>`、`--target-dir <隔离Cargo缓存绝对路径>`和`--output-dir <输出绝对路径>`。已有组件的包组合只改矩阵，新增硬件能力仍须实现和验证相应适配器。

打包要求源码干净，使用根Cargo版本、当前提交和实际nightly身份，核对Cargo输出的生产feature及ARM64 ELF。包包含程序、所选配置和unit模板、helper、VERSION及SHA清单；不包含校准、固定Python环境或保护状态。模板保留现有安装布局，须经候选冻结和现场预检后安装。命名为`robot-system-v<版本>+<7位提交SHA>-<选项>.tar.gz`，同名产物拒绝覆盖；生成包不创建Git标签或启动服务。

## Web控制候选

现有状态配置的`[control].enabled`缺省为`false`。`install_status_units.py prepare --left-control`仅开启bus1源的`control.motion_config`和原`control.enable_alignment_tolerance_rad`，引用已有运动片段，不复制限位或±20raw到位容差；bus2仍只读。候选仅管理左S1、左网关和Web三个unit，不改右网关或S0。服务端沿既有`ROBOT_TOKEN`与gRPC角色校验，正式只读网关不会因Web开关获得控制权限。

prepare须以unit的service User执行，在部署候选生成权限0600的`rpc-roles-web-control.toml`和`web-control.env`，确保服务可读；凭据不进Git或浏览器。准备不操作服务，后续check/backup/install/rollback沿[现有流程](../scripts/systemd/README.md#状态服务安装与回滚)，安装不自动启动。当前部署状态见[TODO8.3](../docs/s0_hal_status.md#通讯与控制todo)；候选、脚本和预检完成后再确认现场并通知启动。

控制复用一个Web进程、每源一条gRPC连接和共享任务快照，不按页面创建S1、网关、采样或任务订阅。S1只在Idle、无任务、新鲜确认断扭且无故障时对齐当前保持目标；Web使能前独立核对推进的反馈和命令镜像。对齐容差不借用±20raw到位容差，滑条仍松手单次提交。S1异常后不自动重启；确认断扭并沿原维护接口人工恢复，不靠重启、清错或重发绕过未知结果。

右侧接入使用同一安装器的`--right-control --existing-control-candidate <已验左候选>`，仅管理右S1、右网关和同一个Web。复用左候选凭据及左运动配置，Web保留左侧控制并增加bus2的六位置关节；三轮只保留raw 0速度目标，不提供位置滑条。实际开放和验收状态见[当前状态](../docs/s0_hal_status.md#通讯与控制todo)。
