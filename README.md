# robot-system

Rust 机器人运行时。当前目标是在一台 XLeRobot 上完成双臂、头部、底盘和相机的 S0 全身硬件抽象。

分层为 `S1 控制 → S0 HAL → 硬件适配器 → 实际设备`：`robot-core` 定义契约，`robot-s0` 负责硬件组织、执行和生命周期，适配器处理设备协议。执行器命令/状态与相机帧保留独立接口；每条总线一个串口所有者。

| 查什么 | 入口 |
|---|---|
| 当前完成边界和下一步 | [S0 HAL 状态](docs/s0_hal_status.md) |
| 开发规则与最小验证 | [CLAUDE.md](CLAUDE.md)、[按需开始](docs/02_start_here.md) |
| 当前配置与测试夹具 | [配置导航](configs/README.md) |
| 服务模板与部署边界 | [systemd 导航](scripts/systemd/README.md) |
| 架构与其他文档 | [文档导航](docs/README.md) |

软件、部署和实机结果分别验收；当前 `D15 OPEN/DEFERRED`、`deployable=false`，不支持无人值守声明。

CI 仅在推送 `v` 前缀的 SemVer 标签时运行，例如 `v0.1.2`、`v0.2.0-rc.1`；普通分支推送和 PR 不触发。候选标签先校验完整格式，不合法则跳过构建与测试。
