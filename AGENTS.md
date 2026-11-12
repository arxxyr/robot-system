# robot-system

Rust 机器人运行时；S0/S1/S2、iceoryx2 与 FlatBuffers 沿既有设计。`AGENTS.md` 是本项目唯一工作约定入口。

## 阅读范围

- 新会话只需本文件。当前落地后端为 XLeRobot；完成边界与下一步见 [当前状态](docs/s0_hal_status.md)。
- 按任务从 [上手导航](docs/02_start_here.md) 找源码或相关章节；不自动整读设计或 `.verify`。
- 冻结契约以 [00_design.md](docs/00_design.md) 为准；冲突顺序为设计 > 本文件 > 全局约定 > 上手指南，不覆盖用户明确指令。代码任务不改冻结设计语义；必要变更先登记 CHANGELOG 与验收影响。
- 当前结论、阻塞和 roadmap 只维护在 [当前状态](docs/s0_hal_status.md)。旧交接、重复清单和阶段流水账从工作树删除，按需查 Git；独有日志与实机原帧保留。旧授权、供电和看护状态不能当作当前事实。

## 最小实现原则

- 先读原版实际入口和调用链，再实现或排查；核对版本、初始化、单位、编码、连接复用及停止顺序。
- 当前只围绕一台 XLeRobot、现有 `scripted` 后端和维护入口闭环；每轮解决一个阻塞，不预建 HAL crate、RPC、插件、新构型或会话框架。
- 轮位唯一来源是 [现有 TOML](configs/xlerobot-bus2.toml) 的 `joints`/`motor_ids`；只支持既有 ID7/8/9 排列，不在代码复制映射。轮校准按物理 ID 保留数值；改配置需重新生成计划，旧 GO 失效。
- 软件、安装、运行、实际动作和停止分别验收。保留原始日志、原帧及失败事实，不回填历史 PASS，不生成重复源码包、报告或已删除的 `H_evidence.tar.gz`。
- 按修改风险补必要验证，已有同源结果可引用；不把文档整理扩为一次全仓或实机验收，不另建第二份进度清单。

## 目录与职责

- 分层为 `S1 → S0 HAL → 硬件适配器 → 设备`。`robot-core` 定义统一契约，`robot-s0` 负责设备组织、分组执行和生命周期；具体协议留在适配实现中，维护试验流程留在工具中。不为分层名称新建 crate。
- 配置用途以 [configs/README.md](configs/README.md) 为准：只保留当前配置和仍被调用的测试夹具；退出使用的旧配置、旧实现和重复文档核对引用后直接删除，不另建 archive、old 或备份副本。不按目录通配安装，不在源码复制配置值。
- 模块入口注释只写当前职责、接口与约束；已完成步骤、评审轮次和历史状态回到 Git 历史。复用须有实际调用需求，不为文件大小或目录对称拆模块。
- 测试构建与运行必须使用同一个隔离产物目录，不能覆盖服务引用的 `target/release` 或冻结产物；脚本需要测试时，优先用假命令验证路径与退出码，不启动硬件。
- 移动或删除前查代码、脚本、CI 与部署引用，同步当前链接；保留独有原帧、兼容性夹具和失败事实。仓库外仍在使用的历史工具先迁移并验证，不能当垃圾清理。
- `.verify` 仅作本地临时验收空间，不强制加入 Git。当前结论所需的最小证据放 `docs/evidence`，保留原始字节、来源和 SHA；完整日志与帧留在既有位置。证据选集不收构建产物、源码包或重复工具副本。

## 安全与执行边界

- 保持 S0/S1 隔离；每条总线一个所有者、SerialRT writer、命令流、控制权及停止域。IK 在 S1，相机采集走 SoftRT，不能阻塞执行器。
- 保留设计 §6.7 的软件安全合同、systemd 外层 watchdog、持久 EStop/fault/最后安全输出的 fsync 双写和幂等请求。EStop 仅由 maintainer+ 的 `ClearEStop` 解除，不删文件绕过。
- S0 失活所需硬件 fail-safe 尚未闭环：D15 保持 `OPEN/DEFERRED`，`deployable=false`；受监督局部通过不等于可无人值守。
- RT 不做 IPC、堆分配、`Arc::clone/drop`、格式化日志、文件/网络 IO 或 panic；业务时间用 `RobotClock`。校准快照沿既有 Reader/Publisher，不以 Arc 操作替代。
- 实机先准备编译产物、固定解释器、具体脚本、进程检查和 `PLAN_ONLY`，然后再问本轮现场条件；动作前先通知。有效授权覆盖内不重复询问，时效过期或状态变化才重新确认，不改时间戳续期或重用已消费 GO。
- 硬件入口用既有固定 Python 直接启动，不用 `uv run` 包一层；网络不稳用 tmux/screen。读写都复用原独占入口；停止读回、失败粘性及失败处置不放宽，不自动重试未知结果。
- 电机参数、限位和原位锚点不得为通过测试而改动；禁止用 clamp 把越界位置伪装成原位保持。具体已批准 raw/停止合同与待验项见当前状态，现场确认不从历史文档推定。

## 工具链与代码

- 正式可执行程序和 systemd 服务统一使用 `robot-` 前缀；配置文件使用小写连字符，名称保留机型与用途。内部 Rust 模块仍用 snake_case；文件改名不改变 robot ID、topic 或保护目录。

- Rust 使用浮动最新 `nightly`、Edition 2024，不写 `rust-version`，记录实际版本。核心 crate 默认不用 `#![feature]`；确需启用应注释并评审。仅真实编译回归可临时回退日期并登记解除条件。
- 自有 crate 的依赖版本和来源只在根 `Cargo.toml` 的 `workspace.dependencies` 定义，按用途分组，构建工具与测试依赖集中列出；子 crate 一律 `workspace = true`，仅保留必要的可选项与特性选择。测试工具也共用根 workspace 和锁文件；第三方补丁源码保持上游 manifest 结构。
- S0/S1 使用系统分配器且 RT 禁 alloc；外部工具按既有约定使用 mimalloc。沿用 Cargo 已登记的构建方案：当前 workspace `release` 为 abort，独立观测工具用 `release-tools` 保留 unwind；不在整理时改变 panic 语义。
- `robot-core` 保持冻结设计的零 runtime 依赖边界，遵守既有白名单；RPC 使用 `CommandMeta` 与强类型，不用 `protobuf.Any`；`layout_id` 保持 `u64`。具体接口、线程和 wire 规则按需读设计对应章节。
- 每次提交前实际执行 `cargo fmt --all`，再执行 `cargo clippy --all --all-targets -- -D warnings`，零警告后提交；相关测试按改动选择。CI 顺序沿既有配置。
- macOS 适用于纯值/部分单测；S0 的 Linux 路径在 Orin 或现有 Linux 环境验证。候选使用隔离工作目录和已有缓存，不在正式安装产物目录构建；只清理本任务资源。
- Python 开发/测试沿全局 `uv` 约定；上述固定解释器仅用于既有硬件入口。

## Git 收尾

- 长期只留 `master`、`dev`；开发使用 `feat/*` 或 `fix/*`，保护 `master`。提交为 emoji + Conventional Commits 中文说明，不带 scope 括号或 Co-Authored-By。
- 日常开发通过适用检查后，仅合入并推送 `dev`，同步 `origin`、`github`；`master` 只在正式发布时更新，并在已验证的发布提交创建对应 SemVer 标签。发布前先在 `dev` 完成验证与打包，包内提交、`master` 发布提交与标签必须一致。
- 核对两个远程的对应分支和发布标签 SHA 后，回收本次已合并临时分支；不强推覆盖他人提交。
- 保留无关修改和未完成工作树；分支回收不能丢草稿或将未验证内容合入。Git 完成不代表硬件验收完成。
