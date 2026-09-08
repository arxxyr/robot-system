# 交接 · post-E 通信故障硬化（2026-09-08 持续推进）

## 本轮状态（2026-09-08，优先于下方历史交接）

- 用户授权：对齐本地文档，固定专职开发 / 独立评审两个子 agent，持续推进。
- Spec 的 2026-09-05 round 21 批准保持有效；独立 reviewer 已于 2026-09-08 明确
  **Plan Approved**。developer 按 C1–C5 分包实施，主 agent 负责整合、文档与检查。
  批准文件为 [正式 Plan](post_e_comm_fault_hardening.PLAN.md)，916 行，SHA256
  `3a8a394a74764ad2277bdd5f3aa0901627538f635d4b6208c55cde99d6f2dd30`。
- 开发分支已创建：`feat/post-e-comm-fault-hardening`，基础提交 `bd081cb`。
- C1 公共契约已实现并获独立 reviewer **C1 Approved**；core 79、S0 298、schema
  compat 16 项通过，S0 含主 agent 独立验收 14 项。Cargo manifest 提示已最小修正，
  fresh fmt / workspace clippy / all-targets build 均零警告，timings 9.55 s；证据见
  `.verify/post_e_comm_fault_hardening/C1.json`。
- C2 有界传输 / adapter 已实现并获 **C2 Approved**；诊断缺 motor 的 P2 已修复并
  经 reviewer ACK 关闭。Feetech 145 库测试（含独立 11）+ 12 独立 integration、
  workspace 全 feature lib / bins 1389 项通过。默认 build 暴露仅 real-hw 使用的
  tracing 依赖声明未门控，随包按实际使用域修正；最终门与日志见 C2.json。
  接续 C3 serial 收敛，整候选尚未完成。
- 旧“不得实施 / 不 commit”的授权前提只描述 9 月 5 日；本轮按用户持续推进授权
  与内部质量门执行，提交前仍必须 fmt + clippy 零警告。不因本地开发执行真机动作。
- 文档所有者：主 agent（Spec / 本交接 / 导航）；开发所有者：developer（正式计划 /
  生产代码）；独立评审：inspect_current_specs（只读代码 / QA）。
- D15 保持 OPEN；RX 重同步、串口 reopen、跨臂联锁不在本候选内。
- Plan 前置独立评审完成：启动发布门、事务 RAII 执行权、RR 三类 pending 责任、
  SerialRT 40 位 / HardRT 48 位分派、integration / smoke 活清单五项纳入正式计划。
- AC-1 消歧已获 reviewer 同意：按实现前 Cargo 清单保留全部覆盖，历史 12/12 溯源
  为 workflow 主体 + 公共 subprocess 测试；补已批准 abort additive default 例外。
- Plan round 1 的三项阻断已逐项回修：S-39 未提交请求超时、周期事务入口被 EStop
  拒绝的返回语义、完整 ignored / smoke / 当前二进制验收命令。Spec D3 / D4 /
  D13 / AC 及 CHANGELOG、10_verification 已 Reverse Sync，最终复核已通过。
- 基线检查（实现前）：fmt / 默认 clippy 返回 0；2026-09-08 复核完整日志勘误：
  Rust / clippy 诊断为 0，但 Cargo 另有未用 workspace dependency 与既有二进制
  snake_case 命名的 manifest 提示，不能称整个输出零警告。全 feature clippy 另报
  既有 feetech 可选 `robot-logging` 未用警告，C2 修正；
  `cargo test --workspace --all-features --lib --bins` 为 1345 PASS / 0 FAIL。
  RPC 五组 integration 实跑 45 PASS（workflow 12、estop 7、set_enabled 11、
  state 8、task 7），显式指定本次 debug 构建的三个子进程。其余 integration 仅
  冻结清单，smoke 与真机尚未执行；日志位于 `target/post_e_verify/`。

## 历史交接（2026-09-05）

## 当前状态（一句话）

reviewer round 21 已明确 **Spec Approved（2026-09-05）**，8 项拍板全部 ACK；
AC-9(o7) 已收准为“原语成功 CAS 后零写”，允许 CAS 前的 latch / pending drain /
诊断计数，随收口同步，**不开新 Spec 回修轮**。

当前交付是 Spec 末尾的 **Plan outline（待审）**。本次不是 Plan Approved，
**不授权实施**。代码基线 `bd081cb`，0 代码改动，文档仍 untracked / 未 commit。

## 工作流（RIPER）

1. 用户粘贴 reviewer 一轮意见（P0 / P1 / 非阻断，附 `file:line`）。reviewer 默认
   只读；Spec 已批准，后续围绕末尾 Plan outline 细化文件 / 函数级计划，
   不重新选择已批准方案。发现真实语义偏离才走 Reverse Sync。
2. **逐条核码**：每个引用行号实读后确认事实；检索优先 `rg`，区间用
   `sed -n 'a,bp'` / `nl -ba`。新代码事实登记 **S-xx**（当前到 S-38），规范自身的
   时序错误写 amend，不冒充代码已实现。
3. 改 Spec：amend 记录置顶（本轮全文；历史轮只留摘要）；正文必须**自包含**，
   规范、phase / 调度表、AC、hard boundaries 和拍板状态同步。
4. 每次改完跑下方自检。仅文档改动不运行生产代码测试，不将新增 AC 写成已通过。
5. 更新本交接文件；记忆文件追加本轮摘要：
   `~/.claude/projects/-home-loosqk-repo-robot-robot-system/memory/candidate_e_execute_progress.md`。
   本机已核验该记忆文件不受 chezmoi 管理；若迁移机器，写入前重新核对。
6. 回复以「爸爸」开头，简要说明核码结论、回修点和验证；待复核状态写清，不自行
   视为 reviewer ACK。当前 Spec 拍板清单 8 项全部 ACK；Plan 审批独立记录。

**不要做的事**：不 commit（reviewer ACK 后才提交，Conventional Commits + emoji，
**不加 Co-Authored-By**；commit 前 fmt + clippy；`git fetch` 后再 push）；不改
`docs/00_design.md`（偏离走 CHANGELOG + `10_verification.md`）；不动并行 session
的文件；不宣称超出“机器人控制执行面受监督功能 demo 跑通”的口径；候选 E 的
D15 硬件 fail-safe 保持 OPEN。用户本轮只批准准备 Plan outline，未批准 Plan 或实施。

## 写文件的坑（本机实测）

- 单次 heredoc 输出有上限：整份 1500 行一次写会截断/失败。做法：**Python 定点
  替换（`assert count == 1`，行内换行用宽松正则 `\s*`）+ 只重写变动大的段**（round 19 曾
  把 `### D13（` 起截断后重写 D13–D15，再追加 AC–末尾）。
- 若 heredoc 先 append、Python 后失败，会出现**重复段**（round 19 出现过两个 `### D13`
  标题）：用 `s.index('### D13（')` 两次定位去重。自检脚本打印 D13 标题行数，应恰 1。
- 表格单元格内**不能含 `|`**（如 `{gen | code | …}` 要写成 `{gen, code, …}`），
  否则管道自检报错。

## 自检脚本（复制即用）

```python
from pathlib import Path
import re
p = Path('team-swe/specs/post_e_comm_fault_hardening.md')
s = p.read_text()
lines = s.splitlines()
bad = []
i = 0
while i < len(lines):
    if lines[i].lstrip().startswith('|'):
        hdr = lines[i].count('|')
        j = i
        while j < len(lines) and lines[j].lstrip().startswith('|'):
            if lines[j].count('|') != hdr:
                bad.append((j + 1, lines[j].count('|'), hdr))
            j += 1
        i = j
    else:
        i += 1
body = s.split('## What\n', 1)[1]
refs = re.findall(r'同 round|同前轮', body)
old_names = re.findall(
    r'estop_seq|last_seen|N_max = 13|external_estop|compare_exchange_weak|EStopPending',
    body,
)
d13 = [k for k, line in enumerate(lines, 1) if line.startswith('### D13（')]
trailing = [k for k, line in enumerate(lines, 1) if line.rstrip() != line]
print(len(lines), trailing, bad, refs, old_names, d13)
assert not (trailing or bad or refs or old_names)
assert len(d13) == 1
```

## round 20 回修要点（round 21 已闭合，保留设计理由）

- **P0**：`ConvergeRound.completed_safety` 是 serial 自有完成记录，初始 / 重开为
  None，Confirm / D3 豁免 / 启动 deadline 出口写 Some；publisher 改写 word 不影响旧证据。
  D4(d) 优先于启动合并：AwaitAck 可信 off → 保留原 ctx / gen / inhibit / BOOT /
  原绝对截止，仅重开 `StopIssue → Confirm`；再次完成仍 AwaitAck(n)，ACK 早到不
  跳过物理阶段。不可信 → 零 IO 升级 SSF 新 gen。AC-19(iv) + AC-23 同步每轮预算。
- **P1-1**：撤 serial claim 通用尾写。`StopIssue ∧ estop_bound` 是 serial 唯一
  policy 写点：`tighten_estop_policy()` strong CAS 仅将四种可输出 policy 收紧为
  StopTorque，Disabled / StopTorque 原样保留；IPC Fault commit + Disabled 任一
  顺序都不被覆盖。纯 Fault 收敛零 policy 写。S-37 + AC-9(r)。
- **P1-2**：Step 1 命中后只清 pending + 记 `emergency_handled`，Step 1b 每 tick
  必经一次，随后才结束普通处理。SSF 仍在 Step 1 优先 commit + Disabled 后 ack
  快照 gen；Step 0 之后新看到的 SSF 留下一 tick，不由非 SSF 分支误 ack。
  S-38 + AC-8(j) 连续 4 tick EStop / AC-8(k) SSF 升级交错。
- **非阻断**：AC-9(o6) 第二次发布前安排两 consumer 已各 claim 一次，另测尚未
  消费 notice 的合法合并；AC-22(l16) 零业务 worker spawn，supervisor 已启动。
- **联动口径**：普通 lane 时延上界以没有 emergency 占用 tick 为前提；normal IO
  禁入指 t1 至本轮收敛完成（claim 前 CAS，claim 后 phase / inhibit），健康
  Emergency 完成后的采样仍按既有规则，StopTorque 继续禁 write_command。

## 拍板状态（round 21：Spec Approved）

- 已 ACK：范围、D1、D2、D3、D4、D6/D8、D9、D11–D14（清单 8 项）。
- AwaitAck 完成记录与 ACK 义务、policy 条件 CAS / SSF 零写、emergency 后 mailbox
  服务三项均闭合；AC-9(o7) 非阻断措辞已同步。
- 下一步：评审 Plan outline，展开函数签名 / 文件所有者 / 依赖与验证门槛，再进入
  Plan round。只有明确 Plan Approved 后才可实施；Spec Approved 不替代此门禁。

## 关键设计锚点（改动时别打破）

- SerialRT 权威 `safety_state` 只在 `safety_word.state`；serial / IPC 原语的
  strong CAS 保留他人字段；IPC 不把 StopFailed / AdapterUncertain 降为
  StopCommanded。serial 的 completed_safety 仅作本轮出口证据，不是第二权威状态。
- `gen = Some` 无例外经 AwaitAck / SafetyStopFailedAwaitAck；AwaitAck 的物理重开
  保留 gen 与 ACK 义务；mailbox 单词绑定 gen/code/detail/origin，发布顺序
  ① state / 记录 → ② inhibit → ③ 词，IPC 唯一 ack；连续 EStop 不跳过 Step 1b。
- estop_bound 与 code 升级正交；SerialRT `(Fault(_), EStop) → 原 Fault + Disabled`；
  serial 的 policy CAS 不能反向覆盖 Disabled，HardRT 通配转 Safe 不变。
- 每逻辑 EStop 恰一次 estop_monotonic，双 consumer 各自在处理开始 claim 自己的位；
  normal_io_active acquire-CAS 与 EStop CAS 在同 word 全序。不得恢复双发或 load
  检查后直接启动 normal 事务。
- 启动：main 首语句 boot_supervisor，N_max = 32 后只缩短；CP1 七 worker 全部
  Spawned，CP2 = T_abs + 1 s；_exit(88) 可重启；READY 按 try_notify_ready 真送达
  （required/standalone/auto）。AwaitAck 重开不再 Connect、不刷新启动截止。
- marker：`1\n` / `0\n` 双状态、rename 线性化点、backup-first 持久屏障、
  authorize 仅 canonical 1/1，退出码 0/2/3/4，锁冲突退 87。

## 相关文件

- Spec：`team-swe/specs/post_e_comm_fault_hardening.md`（untracked）
- CodeMap（已入库 `bd081cb`）：`team-swe/codemap/post_e_comm_fault_hardening.md`
- 历史版本抽取（若需回看某轮全文）：会话转录 jsonl 中 Write/heredoc 内容，脚本思路见
  记忆文件；本机 scratchpad 已有 round 1–9 版本副本（`spec_versions/`），可能已被清理。
- 记忆：`~/.claude/projects/-home-loosqk-repo-robot-robot-system/memory/
  candidate_e_execute_progress.md`（每轮一行）。

## 历史验证（round 20 amend，收口前）

- Spec 结构自检通过：1654 行；尾随空白、表格列错、正文历史依赖引用、旧名残留均为 0；
  D13 标题恰 1 个。自检脚本已实际执行。
- 两名独立 agent 只读复核：AwaitAck / policy 和 mailbox / AC / 拍板状态分别通过，
  未发现本轮未闭合的 P0/P1；补齐其提出的启动 deadline 完成记录、AC-9(o5)
  Step 0 claim 措辞和 AC-8(k) 初始 ACK 前置。
- 仅 Spec / 本交接文件及会话记忆更新；未改生产代码、未运行生产代码测试、未 commit。
  新 AC 是实施期验收要求，未宣称已经实现或通过；该次等待已由 round 21 的
  Spec Approved 结束，当前状态以上方审批记录为准。

## 当前 Plan outline 交接（Spec Approved 后）

- 位置：Spec 末尾 `## Plan outline（待审；未获 Plan Approved）`，同文件自包含。
- P.0 约束与文件 Owner；P.1 公共契约 / 原子槽 / Diag；P.2 transport / 抢占 / 工厂；
  P.3 serial 收敛；P.4 IPC / 状态机 / RR / pub；P.5 信任文件 / supervisor / READY；
  P.6 独立 QA 与分层质量门；P.7 部署 / 整体回滚 / Archive；P.8 函数级 Plan 待细化项。
- 必须先细化的接缝：AdapterBuildContext 的构造期服务注入、共享 ctrl 提前构造、
  七 worker stop 槽贯穿 spawn 与 handle、pub 直接读取 legacy safety_state 的改线。
  既有健康 monitor 不加入 CP1 七 worker。
- abort additive default 依据 Spec hard boundaries 第 3 条已在范围内；正式 Plan
  要收准 AC-1“无新方法”的该项例外，不增加 ExecutorCtrlClient 的 EStop 方法。
- 质量顺序 fmt→clippy→test→build；依赖 release 产物的 E2E/smoke 后验，显式构建
  当前 checkout 的全部所需 bin/feature，不能拿旧 release 充数。提纲中的命令尚未运行。
- 下一步仅评审 / 展开 Plan。Spec 审批已完成，不重复要求 Spec ACK；也不得把本节
  或提纲的存在解释为 Plan Approved / 实施 / 真机动作授权。

当前收口验证：Spec 1851 行，结构自检通过（零尾随空白 / 表格列错 / 正文历史依赖 /
旧名残留，D13 恰一处）；8 项 ACK 状态核对通过；Plan outline 经独立只读核验，
AC 覆盖与工作包依赖无缺项。未运行 Cargo / 生产测试，未修改代码，未 commit。
