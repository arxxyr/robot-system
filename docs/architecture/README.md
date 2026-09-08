# post-E 运行时架构与进度

[打开 Archify 交互架构图](post_e_runtime.html) · [编辑图源](post_e_runtime.architecture.json)

本图是 2026-09-08 的进度快照，依据 `docs/00_design.md` 冻结设计 v0.1.2、
当前源码和 post-E Spec/PLAN。C1–C4 已独立评审通过并分包提交；
C5 完整回归和现场测量仍待完成。图不修改冻结设计。

主路径为操作入口 → RPC 网关 → S1 → S0 → 总线电机；状态快照经 S0 发布线程
回传。EStop 走事件 topic；ClearEStop 与 SetEnabled 走独立 RR。持久化节点
表示 S0 使用的安全状态文件，EStop latch 与 RX 信任授权具有独立生命周期。

用户指定的现场机器为 `loosqk@10.0.1.40`。只读核对确认：Jetson Orin Nano，
Linux 5.15.199-tegra / aarch64；仓库位于 `/home/loosqk/repo/robot/robot-system`，
工作区干净，`master` 停在 demo 收口提交 `fed7549`，尚未同步本地 post-E 修改。
两个 USB 串口可见；机器人服务未运行，旧栈自启保持禁用。现场 H1/H2/H3
测量尚未执行，D15 硬件 fail-safe 保持 OPEN。

## 生成与验证

使用开源 [Archify](https://github.com/tt-a1i/archify)，工具固定于
`10722002bb8777ecb639d93c49586fae4adf3ae4`；HTML 为离线交互产物。
图源可用该版本 CLI 的 `validate architecture`、`deliver architecture` 和
`visual-check` 重建与核对。

```text
diagram_type: architecture
specification_sha256: c9800beead35f66a57b743b819b8d55ccbf1e14d2d78fbd604452d2232d5ae88
artifact_sha256: d395f59c8b107d9c23d5ae949f490ba9551e2f96816958486d0397aeca43bf74
validation: 9/9 showcase, 0 errors, 0 warnings
browser_evidence: passed
visual_review: passed
correction_rounds: 2
```

浏览器证据位于 [验证记录](post_e_runtime.visual-check.json) 和
[浅色／深色截图](post_e_runtime.visual-check.html)：1440×900、1600×1000、
1920×1080、2048×1320 均无页面溢出。最终 HTML 的浅色与深色截图已实际查看，
节点、标签和说明卡无重叠。自动记录保留 `visualReview: pending`，其含义是
自动工具不替代视觉检查；上面的视觉结论来自实际图像检查。
