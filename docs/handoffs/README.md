# 历史记录索引

当前进度只看 [S0 HAL 状态](../s0_hal_status.md)。本目录是历史快照，不是新会话必读清单；其中“当前”“下一步”“已授权”均属于各自记录时点。

## 按问题查找

| 问题 | 入口 |
|---|---|
| 已完成阶段的状态流水账 | [v0.5 状态归档](v0.5_status_archive.md) |
| post-E 通信故障与历史实机失败 | [历史 HANDOFF](../../team-swe/specs/post_e_comm_fault_hardening.HANDOFF.md)，仅查相关标题 |
| v0.2 实施经过 | [历史实施步骤](../01_implementation_steps.md) |
| 冻结后的契约决定 | [CHANGELOG](../CHANGELOG.md) |
| 原始日志、原帧和当轮结果 | 当前状态中的 `.verify/*/summary.json` 链接，再按其中路径定位原件 |

先用 `rg -n '关键词' docs/handoffs team-swe/specs` 定位，或用 `rg --files docs/handoffs` 找日期文件，再读取所需段落；不要全量加载历史目录。

## 本次精简前的全文

固定提交 `36e663fe929cee29a8bef77a065c3baf62be34a8` 保留了此次精简前的已跟踪文档：`CLAUDE.md`、`docs/02_start_here.md`、`docs/s0_hal_status.md`、`docs/README.md` 和 `docs/handoffs/README.md`。它们分别包含旧状态流水账、上手/Rerun 说明、HAL 诊断过程与完整里程碑索引。

按需查看某一文件或片段，无需恢复工作树：

```sh
git show 36e663fe929cee29a8bef77a065c3baf62be34a8:CLAUDE.md | rg -n '关键词'
git show 36e663fe929cee29a8bef77a065c3baf62be34a8:docs/02_start_here.md | sed -n '192,260p'
```

历史正文以 Git 固定版本留存，不再复制大段到新的入口或归档。既有历史文件、`.verify` 日志和实机原帧保持；历史 FAIL/NOT_RUN/DEFERRED 不因后续通过而重写。已删除的 `H_evidence.tar.gz` 不再生成，Git 中的摘要不能代替该包已删除的原件。
