# robot-system

Rust 机器人运行时；S0/S1/S2、iceoryx2 与 FlatBuffers 沿既有设计。`AGENTS.md` 是本文件的符号链接，保持单份内容。

## 阅读范围

- 新会话只需本文件。当前任务是 XLeRobot 的 S0 全身硬件抽象与最小动作闭环；需要进度时读 [当前状态](docs/s0_hal_status.md)。
- 按任务从 [上手导航](docs/02_start_here.md) 找源码或相关章节；不自动整读设计、历史实施步骤、HANDOFF 或 `.verify`。
- 冻结契约以 [00_design.md](docs/00_design.md) 为准；冲突顺序为设计 > 本文件 > 全局约定 > 上手指南，不覆盖用户明确指令。代码任务不改冻结设计语义；必要变更先登记 CHANGELOG 与验收影响。
- 入口只更新当前结论、阻塞和下一步，不追加每日测试流水账。[历史索引](docs/handoffs/README.md) 按问题检索；旧授权、供电和看护状态不能当作当前事实。

## 最小实现原则

- 先读原版实际入口和调用链，再实现或排查；核对版本、初始化、单位、编码、连接复用及停止顺序。
- 当前只围绕一台 XLeRobot、现有 `scripted` 后端和维护入口闭环；每轮解决一个阻塞，不预建 HAL crate、RPC、插件、新构型或会话框架。
- 轮位唯一来源是 [现有 TOML](configs/xlerobot_bus2_full.toml) 的 `joints`/`motor_ids`；只支持既有 ID7/8/9 排列，不在代码复制映射。轮校准按物理 ID 保留数值；改配置需重新生成计划，旧 GO 失效。
- 软件、安装、运行、实际动作和停止分别验收。保留原始日志、原帧及失败事实，不回填历史 PASS，不生成重复源码包、报告或已删除的 `H_evidence.tar.gz`。
- 按修改风险补必要验证，已有同源结果可引用；不把文档整理扩为一次全仓或实机验收。当前状态只维护在 `s0_hal_status.md`，历史交接不反复同步。

## 安全与执行边界

- 保持 S0/S1 隔离；每条总线一个所有者、SerialRT writer、命令流、控制权及停止域。IK 在 S1，相机采集走 SoftRT，不能阻塞执行器。
- 保留设计 §6.7 的软件安全合同、systemd 外层 watchdog、持久 EStop/fault/最后安全输出的 fsync 双写和幂等请求。EStop 仅由 maintainer+ 的 `ClearEStop` 解除，不删文件绕过。
- S0 失活所需硬件 fail-safe 尚未闭环：D15 保持 `OPEN/DEFERRED`，`deployable=false`；受监督局部通过不等于可无人值守。
- RT 不做 IPC、堆分配、`Arc::clone/drop`、格式化日志、文件/网络 IO 或 panic；业务时间用 `RobotClock`。校准快照沿既有 Reader/Publisher，不以 Arc 操作替代。
- 实机先准备编译产物、固定解释器、具体脚本、进程检查和 `PLAN_ONLY`，然后再问本轮现场条件；动作前先通知。有效授权覆盖内不重复询问，时效过期或状态变化才重新确认，不改时间戳续期或重用已消费 GO。
- 硬件入口用既有固定 Python 直接启动，不用 `uv run` 包一层；网络不稳用 tmux/screen。读写都复用原独占入口；停止读回、失败粘性及失败处置不放宽，不自动重试未知结果。
- 电机参数、限位和原位锚点不得为通过测试而改动；禁止用 clamp 把越界位置伪装成原位保持。具体已批准 raw/停止合同与待验项见当前状态，现场确认不从历史文档推定。

## 工具链与代码

- Rust 使用浮动最新 `nightly`、Edition 2024，不写 `rust-version`，记录实际版本。核心 crate 默认不用 `#![feature]`；确需启用应注释并评审。仅真实编译回归可临时回退日期并登记解除条件。
- S0/S1 使用系统分配器且 RT 禁 alloc；外部工具按既有约定使用 mimalloc。`panic=abort` 仅限 S0/S1 的 release package；其他 crate 保留 unwind。
- `robot-core` 保持冻结设计的零 runtime 依赖边界，遵守既有白名单；RPC 使用 `CommandMeta` 与强类型，不用 `protobuf.Any`；`layout_id` 保持 `u64`。具体接口、线程和 wire 规则按需读设计对应章节。
- 每次提交前实际执行 `cargo fmt --all`，再执行 `cargo clippy --all --all-targets -- -D warnings`，零警告后提交；相关测试按改动选择。CI 顺序沿既有配置。
- macOS 适用于纯值/部分单测；S0 的 Linux 路径在 Orin 或现有 Linux 环境验证。候选使用隔离工作目录和已有缓存，不在正式安装产物目录构建；只清理本任务资源。
- Python 开发/测试沿全局 `uv` 约定；上述固定解释器仅用于既有硬件入口。

## Git 收尾

- 长期只留 `master`、`dev`；开发使用 `feat/*` 或 `fix/*`，保护 `master`。提交为 emoji + Conventional Commits 中文说明，不带 scope 括号或 Co-Authored-By。
- 已授权开发通过适用检查后，成果合入 `dev` 和 `master`，同步 `origin`、`github`，核对远程 SHA 后回收已合并临时分支；不强推覆盖他人提交。
- 保留无关修改和未完成工作树；分支回收不能丢草稿或将未验证内容合入。Git 完成不代表硬件验收完成。
