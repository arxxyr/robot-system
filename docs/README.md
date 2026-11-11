# 文档导航

先读项目 [CLAUDE.md](../CLAUDE.md)，其余只读本次任务相关入口及章节。当前状态只维护在 [S0 HAL 状态](s0_hal_status.md)，历史材料不默认加载。

| 任务 | 入口 |
|---|---|
| 开始工作、最小检查命令 | [按需开始](02_start_here.md) |
| 当前配置与测试夹具 | [配置导航](../configs/README.md) |
| 当前完成边界、阻塞与近期顺序 | [S0 HAL 状态](s0_hal_status.md) |
| 架构与不变量 | [00_design.md](00_design.md)：v0.1.2 冻结设计，权威 |
| 架构概览图 | [通用机器人控制架构](robot_system_architecture.svg)：当前职责、实现与待验边界 |
| 验证契约、指标与既有结果 | [10_verification.md](10_verification.md)：按具体主题检索 |
| 规范变更登记 | [CHANGELOG.md](CHANGELOG.md) |
| 目标平台合同与上游参考 | [targets/README.md](targets/README.md)：仅目标文档 §2 为契约 |
| 候选规格与实现计划 | [team-swe/specs/](../team-swe/specs/)：先确认对应项，不按目录通读 |
| 旧阶段、交接、失败原件与精简前全文 | [历史索引](handoffs/README.md)；[v0.2实施步骤](01_implementation_steps.md)仅历史参考 |

冻结设计不在代码 PR 中改语义；规范变更按 [CHANGELOG](CHANGELOG.md) 与 [目标规范登记规则](targets/README.md) 处理。指南和交接不替代规范，历史 PASS 不替代当前验收。
