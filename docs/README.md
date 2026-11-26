# 文档导航

正式软件版本为 **0.1.0**，版本来源是[根 Cargo.toml](../Cargo.toml)。设计文档的 `v0.1.2` 是冻结契约编号。查看开发进度从[当前状态](s0_hal_status.md)开始，挑选下一项工作看[根待办](../TODO.md)。

| 任务 | 入口 |
|---|---|
| 开始工作、最小检查命令 | [按需开始](02_start_here.md) |
| 当前配置与测试夹具 | [配置导航](../configs/README.md) |
| 完成边界、部署版本与未闭环问题 | [当前状态](s0_hal_status.md) |
| 操作网页、标定电机、对齐模型与回位 | [通讯与控制方案](control_plan.md) |
| 架构与不变量 | [00_design.md](00_design.md)：v0.1.2 冻结设计，权威 |
| 架构概览图 | [通用机器人控制架构](robot_system_architecture.svg)：当前职责、实现与待验边界 |
| 验证契约与工具入口 | [10_verification.md](10_verification.md)：按具体主题检索 |
| 规范变更登记 | [CHANGELOG.md](CHANGELOG.md) |
| 目标平台合同与上游参考 | [targets/README.md](targets/README.md)：仅目标文档 §2 为契约 |
| 安装、回滚与服务边界 | [systemd](../scripts/systemd/README.md) |
| 日志字段、过滤与RT约束 | [日志规范](logging.md) |
| 发布包内容与构建入口 | [配置矩阵](../configs/package-matrix.toml)、[打包脚本](../scripts/build_package.py) |

冻结设计不在代码 PR 中改语义；规范变更按 [CHANGELOG](CHANGELOG.md) 与 [目标规范登记规则](targets/README.md) 处理。Git保留维护所需文档、测试源码与夹具，不收录evidence、perf或测试结果资料；原日志与实机帧留在既有本地或Orin位置，历史内容按需查Git。

正式发布时，包内提交、`master` 发布提交和 `v<版本>+<7位SHA>` 标签指向同一提交。Orin 实际使用的开发候选单独记录在当前状态。
