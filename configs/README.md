# 配置导航

同一配置中的 `joints`、`motor_ids` 和逐电机容差按下标对应；轮位只在这里定义。更改配置后重新生成测试计划，不复用旧 GO。

| 用途 | 文件 | 边界 |
|---|---|---|
| 当前全身 S0 候选 | [bus1](xlerobot_bus1_full.toml)、[bus2](xlerobot_bus2_full.toml) | 左臂＋头部＋相机、右臂＋底盘；默认入口实机验收见[当前状态](../docs/s0_hal_status.md) |
| 现有 arm-only 部署 | [左臂](xlerobot_left_arm.toml)、[右臂](xlerobot_right_arm.toml)、[RPC 角色](rpc_roles.toml) | 仍被服务、部署工具和受监督 demo 使用，保留路径 |
| 模拟与集成测试 | `xlerobot.toml`、`*_smoke.toml`、`clear_estop_e2e.toml` | 由对应测试明确选择；`xlerobot.toml` 是旧 schema/mock 示例，不是实机入口 |
| 退出当前使用的试验 | [archive/](archive/) | 仅查历史配置，不安装、不直接启动 |

归档文件为 `xlerobot_task28_single_joint.toml`、`xlerobot_task29_single_joint.toml`、`xlerobot_task29_right_gripper.toml`、`xlerobot_bus2_right_arm_full.toml`。它们从本目录根移入 `archive/`，配置值保留；旧交接中的原路径通过 Git 历史查阅。它们省略 `auto_enable`，当前程序会默认启用，因此不能用于当前硬件测试。

只按明确文件安装或测试，不使用 `configs/**/*.toml` 批量加载。仍在执行的测试夹具保留稳定路径；新增配置先注明用途，不按轮次复制现有配置。
