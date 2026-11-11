# 当前配置

同一配置中的 `joints`、`motor_ids` 和逐电机容差按下标对应；轮位只在这里定义。更改配置后重新生成测试计划，不复用旧 GO。

| 用途 | 文件 | 边界 |
|---|---|---|
| 当前全身 S0 候选 | [bus1](xlerobot-bus1.toml)、[bus2](xlerobot-bus2.toml) | bus1管左臂、头部及头/左腕RGB；bus2管右臂、底盘及右腕RGB；默认入口实机验收见[当前状态](../docs/s0_hal_status.md) |
| 包配置矩阵 | [package-matrix.toml](package-matrix.toml) | 选择Cargo包、feature、程序及归档文件；当前登记`xlerobot-orin-s0`，只在ARM64 Linux原生构建，不作为设备配置加载 |
| 现有 arm-only 部署 | [左臂](xlerobot-left-arm.toml)、[右臂](xlerobot-right-arm.toml)、[RPC 角色](rpc-roles.toml) | 仍被服务、部署工具和受监督 demo 使用，保留路径 |
| 模拟与集成测试 | `xlerobot.toml`、`*-smoke.toml`、`clear-estop-e2e.toml` | 由对应测试明确选择；`xlerobot.toml` 是旧 schema/mock 示例，不是实机入口 |

只按明确文件安装或测试，不使用 `configs/**/*.toml` 批量加载。仍在执行的测试夹具保留稳定路径；退出使用的配置核对引用后直接删除，不另建归档或备份目录。新增配置先注明用途，不按轮次复制现有配置。

## 按矩阵打包

用`uv run --no-project python scripts/build_package.py --list`列出选项；构建时指定`--package <选项>`、`--target-dir <隔离Cargo缓存绝对路径>`和`--output-dir <输出绝对路径>`。已有组件的包组合只改矩阵，新增硬件能力仍须实现和验证相应适配器。

打包要求源码干净，使用根Cargo版本、当前提交和实际nightly身份，核对Cargo输出的生产feature及ARM64 ELF。包包含程序、所选配置和unit模板、helper、VERSION及SHA清单；不包含校准、固定Python环境或保护状态。模板保留现有安装布局，须经候选冻结和现场预检后安装。命名为`robot-system-v<版本>+<7位提交SHA>-<选项>.tar.gz`，同名产物拒绝覆盖；生成包不创建Git标签或启动服务。
