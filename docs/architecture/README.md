# post-E 运行时架构与进度

[打开 Archify 交互架构图](post_e_runtime.html) · [编辑图源](post_e_runtime.architecture.json)

依据冻结设计 v0.1.2、当前源码与 post-E PLAN。C1–C4 已评审提交；C5 当前源码
`e6d2b306…2598`，本地与 Orin 原生完整测试各 **1737 PASS / 11 ignored**，
各 **67 个故障场景通过**。两端八套 smoke 与 1000 轮资源回收门通过：15 个本轮
Node ID 在 grace 后与基线完全一致。Orin 的四项 RT guard 与正式无测试钩子产物
恢复全部通过。独立评审批准软件范围：`APPROVED_SOFTWARE_SCOPE`，阶段状态为
`SOFTWARE_ACCEPTED_HARDWARE_PENDING`，`deployable=false`。
最新逐项记录见 [C5 证据](../../.verify/post_e_comm_fault_hardening/C5.json)。

主路径为 robotctl → RPC 网关 → S1 → S0 → Feetech；状态经 S0 发布线程回流。
EStop 使用事件 topic，ClearEStop / SetEnabled 使用独立 RR。安全状态持久化节点
表示 S0 的本地文件职责；EStop latch 与 RX 信任授权具有独立生命周期。图不修改冻结设计。

验证机器为 `loosqk@10.0.1.40`，Orin Nano Super / aarch64，使用原仓库
`/home/loosqk/repo/robot/robot-system`。六项开发服务按授权停止且不自动启动。
H1/H2/H3 保持 NEEDS_HUMAN，D15 OPEN；软件验收不替代现场测量与硬件 fail-safe。

## 生成与视觉检查

[Archify](https://github.com/tt-a1i/archify) 固定在 `10722002bb8777ecb639d93c49586fae4adf3ae4`。
使用该版 CLI 顺序执行 `validate architecture`、`deliver architecture` 与 `visual-check`。
HTML 为离线交互产物。

- 图源 SHA256：`5dc865ec5c9f78991f3e3fad00e242ccf8072ca370a9f7aa768e28ac423447e2`。
- HTML SHA256：`676a85d75014cca719da7e034790ce446e34291eccac103d93c229e47b982d2c`。
- Showcase 9/9，通过，0 errors / 0 warnings。
- 1440×900、1600×1000、1920×1080、2048×1320 的浅深主题均无页面溢出。
- 已实际查看 1440×900 与 2048×1320 的四张浅深截图，节点、标签与卡片无重叠或截断。

[浏览器记录](post_e_runtime.visual-check.json) 中自动生成的 `visualReview: pending`
保持原样；实际图像检查结论保存在 [当前验收包](../../.verify/post_e_comm_fault_hardening/C5_evidence.tar.gz)
的 `target/post_e_verify/c5_archify_final/review.json` 成员中。
[查看截图](post_e_runtime.visual-check.html)。
