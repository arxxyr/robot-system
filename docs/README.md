# 文档导航

先读项目 [AGENTS.md](../AGENTS.md)，其余只读本次任务相关入口及章节。当前状态与 roadmap 只维护在 [S0 HAL 状态](s0_hal_status.md)。

| 任务 | 入口 |
|---|---|
| 开始工作、最小检查命令 | [按需开始](02_start_here.md) |
| 当前配置与测试夹具 | [配置导航](../configs/README.md) |
| 当前完成边界、阻塞与 roadmap | [S0 HAL 状态](s0_hal_status.md) |
| 架构与不变量 | [00_design.md](00_design.md)：v0.1.2 冻结设计，权威 |
| 架构概览图 | [通用机器人控制架构](robot_system_architecture.svg)：当前职责、实现与待验边界 |
| 验证契约与工具入口 | [10_verification.md](10_verification.md)：按具体主题检索 |
| 规范变更登记 | [CHANGELOG.md](CHANGELOG.md) |
| 目标平台合同与上游参考 | [targets/README.md](targets/README.md)：仅目标文档 §2 为契约 |
| 安装、回滚与服务边界 | [systemd](../scripts/systemd/README.md) |

冻结设计不在代码 PR 中改语义；规范变更按 [CHANGELOG](CHANGELOG.md) 与 [目标规范登记规则](targets/README.md) 处理。旧交接和阶段计划已退出工作树，按需查 Git；独有日志与实机原帧保留，历史 PASS 不替代当前验收。
