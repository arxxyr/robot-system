# robot-system

Rust 实现的通用机器人控制架构。首个后端 XLeRobot 已完成 S0 全身硬件抽象层的受监督验收，覆盖 17 个电机和三路 RGB。S1 已通过左右腕的网页单关节往返验收。当前工作是逐关节标定、对齐 URDF，再分侧验证双臂回位。

浏览器通过 WebSocket 连接 Orin 上的一个 Web 服务，服务复用两侧 gRPC 网关。任务由同侧 S1 执行，经 S0 HAL 和硬件适配器到达设备。每条总线由一个 S0 独占串口，相机独立采集。S2 任务编排仍在规划中。

![robot-system 通用机器人控制架构](docs/robot_system_architecture.svg)

| 查什么 | 入口 |
|---|---|
| 接下来做什么 | [简版待办](TODO.md) |
| 已部署什么、实机验到哪里 | [当前状态与验收边界](docs/s0_hal_status.md) |
| 网页控制、校准、回初始位姿 | [操作与控制方案](docs/control_plan.md) |
| 开发规则与最小验证 | [AGENTS.md](AGENTS.md)、[按需开始](docs/02_start_here.md) |
| 当前配置与测试夹具 | [配置导航](configs/README.md) |
| 服务模板与部署边界 | [systemd 导航](scripts/systemd/README.md) |
| 架构与其他文档 | [文档导航](docs/README.md) |

目前的实机验收以现场看护为前提。主机或 S0 失活后的独立硬件断能仍待验证（`D15=OPEN/DEFERRED`，`deployable=false`）。

最新正式标签为 `v0.1.0+bc5603f`。S0 发布包按配置矩阵构建，Web、网关和 S1 独立安装。此后在 Orin 验收的开发功能及部署版本见[当前状态](docs/s0_hal_status.md)。

日常开发同步两个远程的 `dev`。正式发布使用 `master` 和 `v<版本>+<发布提交的7位SHA>` 标签，CI 在推送符合 SemVer 的版本标签后执行构建和测试。Orin 部署单独安排，S0 默认不上扭、故障保持锁存。
