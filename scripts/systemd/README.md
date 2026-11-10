# 服务模板与部署入口

仓库中的 unit 是模板，不证明 Orin 已安装或已启动。当前软件与实机边界只看 [S0 HAL 状态](../../docs/s0_hal_status.md)；`D15 OPEN/DEFERRED`、`deployable=false` 保持。

| 用途 | 文件 | 配置与入口 |
|---|---|---|
| 现有 arm-only S0 | `xlerobot-s0-left-arm.service`、`xlerobot-s0-right-arm.service` | 经 `xlerobot_deploy guard` 启动；使用 `/etc/robot/xlerobot_{left,right}_arm.toml` |
| 现有 S1、gateway 与交接 | `xlerobot-s1-*.service`、`xlerobot-gateway-*.service`、`xlerobot-handoff.service` | 安装、备份及回滚按[交接操作单](README_dimos_replace.md) |
| 全身 S0 候选 | `xlerobot-s0-bus1.service`、`xlerobot-s0-bus2.service` | 分别引用当前 `configs/xlerobot_bus{1,2}_full.toml`；默认入口实机验收尚未完成 |
| systemd 故障测试 | `xlerobot-*-freeze-test.service` | 仅由对应 smoke 渲染占位符后使用，不直接安装或启动模板 |

## 配置与产物边界

- 全总线 unit 引用随开发变化的配置，**不是冻结的回滚版本**。回滚使用事先备份并验证过的二进制、配置和 unit；不能仅切换 unit 名称。
- arm-only 与 full-bus 的 robot ID、保护文件和控制范围不同，不自动迁移或混用。`Conflicts=` 只是服务互斥的一部分，还须核对手工进程及串口所有权。
- 先按[当前配置](../../configs/README.md)选择明确文件，不使用已退出当前流程的试验配置。
- 测试构建使用隔离目录。`run_clear_estop_smoke.sh` 构建与运行同一份隔离产物，不改服务引用的 `target/release`；安装仍走既有部署步骤。

## 启停与保护状态

- 现有 arm-only 部署、交接、首次 provision 和故障恢复统一走[交接操作单](README_dimos_replace.md)及原工具。全总线候选须先完成自身验收，不能照抄 arm-only 的身份或保护路径。
- EStop latch、RX marker、锁文件不是可回滚的普通配置。**不得删除、覆盖为0或复制旧保护文件来解除急停**；解除沿原 `ClearEStop` 权限和验证流程。
- `auto_enable=false` 只禁止自动启用，不代表启动无硬件写入；默认 S0 启动会执行停止收敛。实机启动仍须当次现场条件和动作通知。
- `Type=notify`、worker 存活检查和 systemd watchdog 保留；这些软件机制不替代未完成的 D15 硬件 fail-safe。

## 历史验证

旧版完整部署、latch 删除试验和 watchdog 验证记录在提交 `39ee87142b79a9b03fc3f5da3515557615dfaa10` 的同路径文件中，可用 `git show <提交>:scripts/systemd/README.md` 查阅。其中旧实机路径和操作命令不作为当前执行指南；fail-closed 等破坏性场景只在现有 smoke 的隔离 mock 环境验证，不删除实际保护文件做试验。

历史测试通过不证明当前模板、配置或安装产物通过；软件与实机验收分别记录。
