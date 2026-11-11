# 服务模板与部署入口

仓库中的 unit 是模板，不证明 Orin 已安装或已启动。当前软件与实机边界只看 [S0 HAL 状态](../../docs/s0_hal_status.md)；`D15 OPEN/DEFERRED`、`deployable=false` 保持。

| 用途 | 文件 | 配置与入口 |
|---|---|---|
| 现有 arm-only S0 | `xlerobot-s0-left-arm.service`、`xlerobot-s0-right-arm.service` | 经 `xlerobot_deploy guard` 启动；使用 `/etc/robot/xlerobot_{left,right}_arm.toml` |
| 现有 S1、gateway 与交接 | `xlerobot-s1-*.service`、`xlerobot-gateway-*.service`、`xlerobot-handoff.service` | 安装、备份及回滚按[交接操作单](README_dimos_replace.md) |
| 全身 S0 候选 | `xlerobot-s0-bus1.service`、`xlerobot-s0-bus2.service` | 原handoff→guard→S0链，按两份full TOML校验；默认S0实机结论见[当前状态](../../docs/s0_hal_status.md)，服务安装与启动仍待验证 |
| systemd 故障测试 | `xlerobot-*-freeze-test.service` | 仅由对应 smoke 渲染占位符后使用，不直接安装或启动模板 |

## 配置与产物边界

- 全总线 unit 引用随开发变化的配置，**不是冻结的回滚版本**。回滚使用事先备份并验证过的二进制、配置和 unit；不能仅切换 unit 名称。
- arm-only与full-bus的运行ID、控制范围不同；full候选显式复用已验物理总线保护域：bus1→`/var/lib/robot/xlerobot_left`，bus2→`/var/lib/robot/xlerobot_right`。latch和rx-trust目录必须一致，不复制、重置或迁移保护状态。`Conflicts=`不能替代手工进程及串口独占检查。
- 先按[当前配置](../../configs/README.md)选择明确文件，不使用已退出当前流程的试验配置。
- 测试构建使用隔离目录。`run_clear_estop_smoke.sh` 构建与运行同一份隔离产物，不改服务引用的 `target/release`；安装仍走既有部署步骤。

## 启停与保护状态

- 复用[交接操作单](README_dimos_replace.md)的原工具和安装/回滚步骤。guard按显式robot ID严格选择arm-only或full布局；全总线不会绕过原17电机交接和本侧预检。默认S0维护入口通过不代替服务安装与启动验证。
- handoff源模板保留arm-only配置以兼容旧安装；full候选须将handoff两侧参数和两个guard绑定到同批冻结full TOML。handoff只与旧coordinator/web冲突，不能与依赖它的full服务冲突。实际切换前确认旧进程已退出；daemon-reload不会补做已运行进程的启动依赖。
- EStop latch、RX marker、锁文件不是可回滚的普通配置。**不得删除、覆盖为0或复制旧保护文件来解除急停**；解除沿原 `ClearEStop` 权限和验证流程。
- `auto_enable=false` 只禁止自动启用，不代表启动无硬件写入；默认 S0 启动会执行停止收敛。实机启动仍须当次现场条件和动作通知。
- `Type=notify`、worker 存活检查和 systemd watchdog 保留；这些软件机制不替代未完成的 D15 硬件 fail-safe。

## 离线准备

只生成隔离候选，绑定已验S0二进制、新guard工具、TOML、校准、相机helper与固定解释器的SHA；保持`auto_enable=false`。候选沿现有三个unit生成，`systemd-analyze verify`只解析依赖，不安装、不启动。真实`guard`、`preflight-*`和`provision`都有副作用，不能拿来作离线检查；软件测试使用既有假steps和临时目录。

## 历史验证

旧版完整部署、latch 删除试验和 watchdog 验证记录在提交 `39ee87142b79a9b03fc3f5da3515557615dfaa10` 的同路径文件中，可用 `git show <提交>:scripts/systemd/README.md` 查阅。其中旧实机路径和操作命令不作为当前执行指南；fail-closed 等破坏性场景只在现有 smoke 的隔离 mock 环境验证，不删除实际保护文件做试验。

历史测试通过不证明当前模板、配置或安装产物通过；软件与实机验收分别记录。
