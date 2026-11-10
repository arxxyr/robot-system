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

<a id="rerun-观测工具当前开发入口"></a>

## Rerun 旧操作说明

完整操作说明保留在精简前提交，按需读取相关行：

```bash
git show 36e663fe929cee29a8bef77a065c3baf62be34a8:docs/02_start_here.md | sed -n '192,260p'
```

其中“Rerun 观测工具”小节是当时的用法与边界；使用前核对当前参数与发布者。观测画面不构成控制或物理安全确认，相机接入情况以 [S0 HAL 状态](s0_hal_status.md) 为准。
