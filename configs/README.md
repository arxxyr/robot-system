# 当前配置

同一配置中的 `joints`、`motor_ids` 和逐电机容差按下标对应；轮位只在这里定义。更改配置后重新生成测试计划，不复用旧 GO。

全总线配置在所选 `[[hardware]]` 下使用 `serial_rt = { cpu = 4, fifo_priority = 70 }`（右侧CPU为5）。该段启用严格FIFO、绑核和锁页检查，要求产物启用`rt_guard`；CPU范围0..1023、FIFO范围1..99，实际核可用性由启动设置验证。调度设置失败拒绝就绪；与CLI的`--cpu`/`--serial-fifo`重复指定也拒绝。省略该段的旧配置仍可用原CLI。修改值在下次启动生效；systemd只提供权限和普通线程CPU范围，不复制执行线程的配置值。

| 用途 | 文件 | 边界 |
|---|---|---|
| 全总线硬件配置 | [bus1](xlerobot-bus1.toml)、[bus2](xlerobot-bus2.toml) | bus1管左臂、头部及头/左腕RGB；bus2管右臂、底盘及右腕RGB；默认入口实机验收见[当前状态](../docs/s0_hal_status.md) |
| 左侧S1软件入口 | 同一份[bus1](xlerobot-bus1.toml)与[robot-s1-bus1.service](../scripts/systemd/robot-s1-bus1.service) | S1沿8关节顺序和现有scripted profile运行；临时服务的20秒保持已验收，尚未安装为常驻服务，不自动使能 |
| 单关节软件夹具 | [single-joint-smoke.toml](single-joint-smoke.toml) | 两关节mock配置，只用于新载荷、限位、取消和到位测试；其中数值不是XLeRobot实机运动参数 |
| 包配置矩阵 | [package-matrix.toml](package-matrix.toml) | 选择Cargo包、feature、程序及归档文件；当前登记`xlerobot-orin-s0`，只在ARM64 Linux原生构建，不作为设备配置加载 |
| 只读状态页 | [xlerobot-status.toml](xlerobot-status.toml) | 独立HTTP/WebSocket服务，引用原bus1/bus2配置及同机网关50061/50062；无Web token，HTTP默认回环、部署可指定IPv4私网地址，gRPC始终回环；不启动硬件，未纳入S0安装包 |
| 只读部署角色 | [rpc-roles-status.toml](rpc-roles-status.toml) | 候选网关使用空凭据表与viewer/read_state，沿既有GetState免令牌语义；不授予控制权限，不覆盖原rpc-roles.toml |
| 现有 arm-only 部署 | [左臂](xlerobot-left-arm.toml)、[右臂](xlerobot-right-arm.toml)、[RPC 角色](rpc-roles.toml) | 仍被服务、部署工具和受监督 demo 使用，保留路径 |
| 模拟与集成测试 | `xlerobot.toml`、`*-smoke.toml`、`clear-estop-e2e.toml` | 由对应测试明确选择；`xlerobot.toml` 是旧 schema/mock 示例，不是实机入口 |

只按明确文件安装或测试，不使用 `configs/**/*.toml` 批量加载。仍在执行的测试夹具保留稳定路径；退出使用的配置核对引用后直接删除，不另建归档或备份目录。新增配置先注明用途，不按轮次复制现有配置。

## 单关节运动约束

`[s1.single_joint]`是可选动作配置，缺失时拒绝新的单关节请求，原完整向量Scripted不受影响。当前实机bus1/bus2均未添加，不能直接执行单关节动作。参数为`max_duration_ms`、`settle_timeout_ms`和`stable_window_ms`，均须非零，稳定窗口严格小于到位超时；`[[s1.single_joint.joints]]`按`name`完整覆盖所选hardware的位置关节，明确`min_position_rad`、`max_position_rad`、`max_velocity_rad_s`和`tolerance_rad`，不提供默认运动限值。到位超时从末目标成功发布开始计时。

实际部署须依据现有校准与已验证动作填写这些值，S0继续执行原校准限制。原raw校准边界容差不能当作新的运动范围或到位容差。当前入口拒绝混合轮速总线，测试夹具不得安装为生产配置。

## 按矩阵打包

用`uv run --no-project python scripts/build_package.py --list`列出选项；构建时指定`--package <选项>`、`--target-dir <隔离Cargo缓存绝对路径>`和`--output-dir <输出绝对路径>`。已有组件的包组合只改矩阵，新增硬件能力仍须实现和验证相应适配器。

打包要求源码干净，使用根Cargo版本、当前提交和实际nightly身份，核对Cargo输出的生产feature及ARM64 ELF。包包含程序、所选配置和unit模板、helper、VERSION及SHA清单；不包含校准、固定Python环境或保护状态。模板保留现有安装布局，须经候选冻结和现场预检后安装。命名为`robot-system-v<版本>+<7位提交SHA>-<选项>.tar.gz`，同名产物拒绝覆盖；生成包不创建Git标签或启动服务。
