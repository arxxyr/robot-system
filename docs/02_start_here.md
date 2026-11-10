# 按需开始

先读项目 [CLAUDE.md](../CLAUDE.md)，再按任务选择下面的入口及相关章节。无需每次通读冻结设计、旧实施步骤或历史交接。

| 本次任务 | 入口 |
|---|---|
| 当前完成边界、阻塞和下一步 | [S0 HAL 状态](s0_hal_status.md) |
| 实现或核对架构契约 | [冻结设计](00_design.md) 的相关章节；冲突以此为准 |
| 模块、目标平台与规范 | [文档导航](README.md) |
| 历史失败、验收来源或旧操作说明 | [历史索引](handoffs/README.md)，定位后只读相关段落或原件 |

先确认工作区，再搜索具体符号或任务；保留已有修改。

```bash
git status --short --branch
rg -n '本次任务的符号或关键词' crates configs scripts docs
```

Rust 变更按受影响范围验证；提交前依次执行项目规定的检查：

```bash
cargo fmt --all
cargo clippy --all --all-targets -- -D warnings
```

纯文档核对内容与链接，并执行上述提交检查；不重跑功能测试或硬件流程。源码、软件验证、部署和实机验收分别记录；旧 GO 不证明本次现场就绪。

## 在 htop 中查看进程

Linux 控制与硬件测试入口统一使用 `robot-` 短进程名前缀：`robot-s0`、`robot-s1`、`robot-gateway`、`robot-ctl`、`robot-deploy`、`robot-feetech`、`robot-maint`、`robot-h2` 等；开发测试入口也采用该前缀。名称在启动时设置，不改变 Cargo 二进制名、服务名或控制接口。

htop 的 F2 → Display options 中启用 **Show merged command**（`show_merged_command=1`），然后按 F4 输入 `robot-`；也可运行 `htop -F robot-`。保留现有线程名；需要只看进程时按 H 隐藏用户线程。已安装的旧 Rust 产物须在以后正常更新并启动后生效，本项不会重启控制服务。

当前维护入口给冻结 Feetech 工具使用临时 `robot-feetech` 符号链接作为启动名称，原文件及 SHA 校验保持；日志同时记录实际 `argv` 和原 `requested_argv`，子进程回收后删除链接。显示名不能作为安全身份或批量终止依据。

<a id="rerun-观测工具当前开发入口"></a>

## Rerun 旧操作说明

完整操作说明保留在精简前提交，按需读取相关行：

```bash
git show 36e663fe929cee29a8bef77a065c3baf62be34a8:docs/02_start_here.md | sed -n '192,260p'
```

其中“Rerun 观测工具”小节是当时的用法与边界；使用前核对当前参数与发布者。观测画面不构成控制或物理安全确认，相机接入情况以 [S0 HAL 状态](s0_hal_status.md) 为准。
