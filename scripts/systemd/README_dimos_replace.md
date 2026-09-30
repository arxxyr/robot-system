# dimos → robot-system 交接与 S48 整候选部署操作单

> **范围**：当前 S48 候选的四个二进制、七个 unit、两臂配置与 RPC 角色配置，
> 包括停止、备份、原子安装、整体回滚、guard / daemon 监督与人工恢复。
> 这是 P.7 的可评审操作单，**不代表本文件中的命令已经执行**；正式产物与远端
> 安装状态须按 §1 新取证，仓库模板不等于 Orin 已安装件。当前保持
> `deployable=false`；软件验收不自动放行安装、服务启动或真实电机验证。
>
> §9 保留候选 E 的受监督 demo 步序（原 Spec P.2.10），不替代当前通信候选的
> H1/H3/H2 驱动与新现场条件。历史 H2 失败、右臂未测也不因文档更新变为通过。
>
> 收口宣称唯一口径 = **"机器人控制执行面受监督功能 demo 跑通"**；
> **不得**宣称生产替换 / 无人值守 / 硬件 fail-safe 完成（D15 仍 OPEN）。

---

## 0. 交接的安全前提（先读，别跳）

arm-only S0 **不驱动也不能 EStop** head / base 电机；旧栈退出只 best-effort
断扭并吞掉失败（`_manipulator_base.py` disconnect 逐电机 `Torque_Enable=0`
包在 `except: pass`；`bus2_base.py` 轮子 `Goal_Velocity=0` 同款）。因此切换
窗口内 head / wheels 的扭矩与速度状态**未知**。

更要命的是 `auto_enable = false` **只防新 S0 自动上扭，不消除崩溃残留扭矩**
——Feetech `connect()` 只建连接 / 翻译 id / 建缓存，**不写 `Torque_Enable=0`**。
S0 在 Enabled 状态崩溃后，本臂 ID 1-6 在新 S0 起来时仍保持上次带扭状态。

### 0.1 arm ↔ config 必须锁在同一条 D2 行上

`--arm` 决定**安全写序**（左 `[T1..T8]` / 右 `[T1..T6, V7,T7, V8,T8, V9,T9]`），
config 决定**打开哪条物理串口**。两者不绑定时，`--arm left --config
…right_arm.toml` 会打开 bus2 却跑左侧写序——**干扰正在跑的右臂，并且漏掉
wheel ID 9 的零速+断扭**，全程没有任何运行期信号。

因此 guard / `preflight-arm` / `preflight-handoff` 在**打开串口之前**按 D2
身份表逐字校验 config：`[robot].id` / `[[hardware]].id` / `adapter` /
**完整 by-id 串口路径（整串相等，不是子串匹配）** / `motor_ids` / `joints` /
`auto_enable = false` / **`calibration_paths`（恰好一项、绝对路径、文件名 ==
本臂标定文件）**；guard 另外校验转发给 `s0_daemon` 的 `--robot-id` /
`--hardware-id` 与 D2 一致。任一项不符 → 退 86，**不消耗连败预算**（配置错误
每次重试必然重现）。

| | 左臂 | 右臂 |
|---|---|---|
| `robot_id` | `xlerobot_left` | `xlerobot_right` |
| `hardware_id` | `left_arm` | `right_arm` |
| 串口（完整 by-id） | `/dev/serial/by-id/usb-1a86_USB_Single_Serial_5A7C121427-if00` | `/dev/serial/by-id/usb-1a86_USB_Single_Serial_5A7C122340-if00` |
| `calibration_paths` | 恰好一项绝对路径，文件名 `left_arm.json` | 恰好一项绝对路径，文件名 `right_arm.json` |

仓库配置当前引用 `/home/loosqk/repo/dimensional-applications/calibration/` 下的
对应 JSON；现场须核真实文件路径、内容与 SHA。允许复用 Orin 上原 Python 标定值，
不能因旧仓库清理而用空标定顶替。unit 的 `dev-ttyACM0/1.device` 只表达设备启动
依赖，**不能替代完整 by-id 的 D2 身份核验**。

> **为什么串口要整串比**：`contains("5A7C121427")` 会放行 `-if01` 这类伪后缀
> 和 `5A7C1214270` 这类更长的伪串号，两者都指向另一个设备节点。
>
> **为什么标定必须校验**：`calibration_paths` 住 `[[hardware]]` 的 extras，
> `s0_daemon` 对「缺失 / 非数组 / 元素非字符串」一律静默折叠成空数组，而空
> 数组在 feetech factory 里等价于「无标定照常启动」——整条链不会报任何错。

所以启动链是 **fail-closed** 的：任何一条安全写或 readback 失败 → 该臂新栈
**不起**；失败不自动改变旧 unit 的启用策略，也不自动回启旧栈。
**是否回启旧栈与残余风险由人工判断**
（硬件 I/O 已失败的场景**不承诺**旧栈必然安全恢复）。

---

## 1. 整候选清单、停止、备份与安装

### 1.1 先取得真实清单，再形成可安装包

以下是**仓库模板合同**，不是当前远端状态。候选版本以根 `Cargo.toml` 的
workspace 版本为准（当前 `0.1.2`）；配置中的 `0.5.0-e-demo` 是已有配置版本。
本操作单不改版本、依赖、生产 feature 或安装根。

| 同批组件 | 源文件 / 产物名 | 模板目标 |
|---|---|---|
| S0 | `s0_daemon` | `/home/loosqk/repo/robot/robot-system/target/release/s0_daemon` |
| guard / handoff | `xlerobot_deploy` | 同目录 `xlerobot_deploy` |
| S1 | `s1_daemon` | 同目录 `s1_daemon` |
| Gateway | `robot_rpc_gateway` | 同目录 `robot_rpc_gateway` |
| unit ×7 | `scripts/systemd/xlerobot-handoff.service`、`xlerobot-s0-{left,right}-arm.service`、`xlerobot-s1-{left,right}.service`、`xlerobot-gateway-{left,right}.service` | `/etc/systemd/system/` 下对应文件名 |
| 两臂配置 | `configs/xlerobot_{left,right}_arm.toml` | `/etc/robot/` 下对应文件名 |
| RPC 角色配置 | `configs/rpc_roles.toml` | `/etc/robot/rpc_roles.toml` |

为这一批建立一份安装记录，至少含下列三部分；有未知项就停在准备阶段，不能拿
P-E 的 dev/mock/hook 产物或旧 C5 四件补成“当前正式包”。

1. **源与构建清单**：已提交的完整 commit、两端实际同 HEAD 且工作树 clean 的记录、
   实际源清单 SHA、`Cargo.lock` SHA、
   实际 nightly 日期及 `rustc -Vv`、原生 ARM 主机/target、每条 Cargo argv / cwd /
   feature 与默认 feature 状态、必要环境、自然退出码和完整日志。保留
   `--message-format=json` 的 `compiler-artifact` 原记录，按 executable 字段找到
   四件实际输出；记录每件 SHA-256、大小、ELF AArch64 身份与依赖。仅“文件存在”
   或同名旧文件不合格。P-E 的两端同源证据不替代正式提交/clean 核验。P.7已生成
   独立清单，每次使用核当前批准来源；驱动三Python变化后不得豁免旧build继承门。
2. **新包安装清单**：上述十四件逐项绑定源路径、目标绝对路径、SHA、UID/GID、
   mode；unit 若按实际根调整，保留调整后的最终内容与 SHA。保存相关 drop-in、
   EnvironmentFile、标定 JSON 的路径与内容身份，确认没有遗漏的外部依赖。
   RPC 配置/环境中的真实凭据不写进公开日志或提交；安装记录按现有权限保存。
3. **旧包与主机清单**：现场新采 `systemctl cat/show` 的 FragmentPath、DropInPaths、
   ExecStart、User/Group、SupplementaryGroups、Environment/EnvironmentFiles、
   WorkingDirectory、依赖、调度/权限、Restart/Watchdog、启用与 mask 状态；
   分别记录 active/substate/MainPID/退出结果、实际进程可执行文件与落盘文件身份。
   保存文件类型、符号链接及其实际目标、权限、SHA、原来不存在的路径，以及七件
   unit 的有效合并配置。不可只备份仓库文件后声称备份了已安装系统。

正式四件须在 Orin **原生 ARM release、无任何 `test-*` feature** 构建；本阶段
产物准备采用 `P7_RELEASE_ARTIFACTS_V2`，准备执行器先成功完成 `rustup update nightly`，
再以 `RUSTUP_TOOLCHAIN=nightly` 执行 `--locked --release --message-format=json`、
jobs=2；不默认固定日期或沿用历史编译器。六条构建共用同一对实际 `rustc -vV`
自然完成回执（`rustc_before` / `rustc_after`）和完整环境、源码 cwd、boot_id。
`rustc_identity` 严格绑定唯一 `release`（nightly）、40 位 `commit_hash` 和原生 `host`；
前后身份必须相同，且 `before.end <= build.start <= build.end <= after.start`。
日期和 SHA 用于本轮溯源，不改变默认跟随最新 nightly 的项目策略。验证器不联网或更新工具链。
V1 仅允许显式 `allow_legacy=True` 历史只读验读；真实 H2 execute 强制两套 V2 及同一对回执，
不会把旧包默认放行为新 run。源、ELF、feature/profile、sidecar、真实 wait 与正式件隔离门保持。
分别按包构建并保存实际 feature 闭包：S0 与 guard 保留 default 并使用
`--features xlerobot-real`，S1 保留默认组合（含 `scripted`），Gateway 使用
`--no-default-features` 且不加测试 feature。两个配置实际选
`[s1.profile].backends = ["scripted"]`，构建清单须证明该后端存在。
`xlerobot_deploy` 要求 `xlerobot-real`，缺失时 Cargo 会跳过该 bin，因此必须核
本次 JSON 产物身份。生产与测量产物放入各自独占且拒覆盖的冻结目录，先冻结
生产再构建测量，防止同名文件被后一次构建覆盖；原生 ELF 的 Machine 为 AArch64
（183）。完整命令与质量门按 P.7 产物合同执行，不能从文档推断已经构建或验收。

2026-09-20 只读盘点确认，Orin 上述四个 `target/release/` 路径就是实际安装件。
本轮构建必须显式使用独立 `CARGO_TARGET_DIR=target/rr-r4db-arm/root`（已有缓存），
执行 Cargo 前核构建输出与安装件的真实路径及 inode 均不重合；禁止让准备构建覆盖
现役文件。该路径是构建缓存，不是安装根；仍须另行冻结生产与测量两套产物。

**RT 无分配约束仍成立，但不要夸大保障机制**：生产 `s0_daemon` 当前没有安装
全局 `AllocDisabler`；给 feature 加上 `rt_guard` 不等于生产进程已全局拦截分配。
实际拦截测试 ELF 与生产 ELF、测试覆盖与运行时拦截须分别记录。本轮沿用生产
feature，不借部署文档修改 allocator。

### 1.2 双臂停止并核实际退出

共享四件二进制更新前，**两臂所有业务实例都必须停稳**。先记录六件状态、PID、
日志起点与新现场支撑/断电手段；带扭、进程故障或状态未知时按 §7 处理，
`systemctl stop` 成功不证明电机已经物理断扭。

以下为分层停止命令面；每条返回后核本层实际状态/退出，再进入下一层：

```bash
sudo systemctl stop xlerobot-gateway-left.service xlerobot-gateway-right.service
sudo systemctl stop xlerobot-s1-left.service xlerobot-s1-right.service
sudo systemctl stop xlerobot-s0-left-arm.service xlerobot-s0-right-arm.service
```

记录 LoadState、ActiveState、SubState、MainPID、Result、ExecMainCode/Status 与
本轮 journal。只有已知 unit 已停、对应 cgroup / 原 PID 与手工启动的同类进程均
已退出，且 USB/IPC 真实持有者已核清，才可换文件。`failed` 可能已无进程，但必须
解释失败；查询错误、unknown、activating/deactivating、残余进程不能当作已停。
同一源码目录还有手工测试进程时也须收尾，不能只匹配六件 unit。

**不要 stop/restart `xlerobot-handoff.service` 作为清理**。它虽无常驻进程，
却保存 oneshot 完成态；使其失活会让下次依赖启动重新执行真实全 bus I/O。
记录其当前状态与最近一次运行身份，是否需要新的 handoff 必须按 §2/§3 的真实
现场条件单独安排。更新文件或 `daemon-reload` 本身不会刷新旧的完成事实。

### 1.3 备份完整旧包

沿现有目录 `/var/lib/robot/deploy-backups/post-e-<候选完整commit>` 保存一次
独占备份；目录已存在时不能覆盖旧证据，另加本次实际时间标识。将 §1.1 旧包
清单对应的**四件二进制、七件 unit、三个配置**及实际生效的 drop-in/外部配置
原件备份，保持路径对应、文件类型、链接、UID/GID/mode 与校验值，记录原来不存在
的目标。共享标定文件至少记录准确身份；若本次确需改它，也纳入同批备份与回滚。

备份完成后重新核旧源与备份 SHA 相同、元数据齐全、旧 ELF/配置可追溯，才可安装。
保留旧 unit 的 enable/disable/mask 状态，但不把还原自启状态等同于马上启动。
**RX marker、EStop latch、`.clear_pending` 和稳定锁不是可回滚的包内容**：可以
保存只读诊断记录，不得从旧快照恢复授权或安全状态，不得删除/替换稳定锁 inode。

### 1.4 同目录原子换入，整批校验后才允许启动

1. 对照本次清单核新包完整、四件 ELF 同源、无测试 hook，目标均无执行者；
   目标是符号链接时先核清链接布局，不能把链接误替换成普通文件。
2. 为每一件在**目标同一目录**创建本次独占临时文件；按清单复制内容并设好权限，
   核临时文件 SHA，fsync 文件后同目录 rename 到目标，再 fsync 父目录。二进制
   通常 `0755`，unit/config 按既有模板 `0644`；UID/GID 与特殊权限以经过审查的
   清单为准。不要用 `cp/install` 直接覆盖最终 ELF，也不要跨文件系统搬临时文件。
3. 逐件记录旧/新 SHA、安装结果与父目录持久化结果。这只是**每个文件原子替换**，
   十四件并非一个文件系统事务；整批过程中保持服务停止。任一件失败即停在当前
   状态并按 §1.6 恢复**完整旧包**，不能带半套新旧件启动验证。
4. 核配置与全部引用：handoff、guard、guard 自动追加给 daemon 的 config 必须
   同为 `/etc/robot/xlerobot_<side>_arm.toml`；S1/Gateway 也使用同臂配置。
   `guard --` 之后不能再带 `--config`，重复会退 86。配置路径若必须变更，要同步
   所有引用并通过既有 `deploy_units` 一致性检查，不现场改一处顶替。
5. 核持久化目录为 `loosqk:loosqk`、`0750`；当前 marker、RX 锁与 latch 锁创建
   模式为 `0600`，latch 原语为 `0640`；`/run` 下 guard count/lock 为 `0640`，
   由各自实现管理；
   已有目录与文件先核真实权限，不能递归改权限覆盖稳定锁/安全状态。首次建目录
   可以按既有操作使用 `sudo install -d -o loosqk -g loosqk -m 0750`。
   latch 本身由 guard provision / daemon 恢复逻辑管理，不手写：双缺失按
   latched `1\n` 双份创建，不能借换 robot_id 丢 EStop 记忆。
6. 全件新 SHA、权限、引用与 unit 静态检查通过后，执行 `sudo systemctl daemon-reload`，
   再采实际合并 unit，与清单逐项比较。reload 不启动服务、不修复 RX 信任。

### 1.5 启动核验：先左完成并验证，再右

仅在这一批安装获准、物理现场条件新确认且 §5 的对应臂一次性信任步骤成功后
启动。先评估 handoff 是否会执行，确认双臂/head/base 已有支撑与断电手段；它会
访问两条 bus，不能把“只测左臂”理解为只触碰左臂。

先启动左 S0，核 guard、授权消费、首 Safety / Connect / Passed、`READY 已送达`
和实际 `Type=notify` 状态；再启动左 S1、Gateway，核各自 READY、当前 PID 和只读
GetState 的身份、layout、safety/Fault 状态。Gateway 仍保持其 `Requires/Wants/After`
依赖，不能绕过失败的 S0。右侧在左侧本次验证收尾与新的右侧条件到齐后同样执行。
启动验证不自动发 ClearEStop 或 SetEnabled，不自动进入 §9 的运动 demo。

串口线程实查 FIFO 70，左 CPU 4 / 右 CPU 5；其余线程/工具/Gateway 普通调度落
CPU 0–3。核真实 TID、调度、affinity/cpuset、rtprio/memlock 与内核事实，不能凭
`sudo`、unit 文本或旧线程数宣称达标。任一 Dirty/SSF、预算超时、锁冲突或退出
异常保留原结果并停在故障处，不自动 authorize 或 restart 循环。

### 1.6 完整回滚

1. 保存本次新包、安装进度、日志与原始失败结果；按 §1.2 停双臂整条链并确认现场
   安全与进程退出，不仅停 S0。不动 handoff 完成态来“清理”。
2. 对照同一备份清单，用 §1.4 的同目录临时文件、校验、fsync、rename 恢复四件
   二进制、七件 unit、三个配置及本次改动的 drop-in/外部文件，核旧 SHA/权限。
   原本不存在而本批新建的包文件，按记录定向撤销；不得扩大到 marker/latch/锁。
3. 完整旧包恢复并核实后 `daemon-reload`，再核有效 unit 与原记录一致；启用/mask
   状态按已记录的旧策略恢复，业务服务保持停止，等待适用的现场启动条件。
4. 旧版本可能不理解新 marker、权限/责任退休与 owned 关闭协议；**回滚不等于恢复
   RX 信任或消除硬件风险**。不启动新 Gateway 配旧 S0 等混合组合。旧栈恢复也须
   走其适用的候选 E 受监督边界；重新使用 S48 仍需新 D9 物理边界和一次性授权。

源码回退由评审后的整范围 revert 保留提交与失败证据，不 reset 用户工作区。
没有现场条件时，完成文件准备/回滚记录后保持停机，不能用旧版本自动试运动。

---

## 2. 依赖拓扑（谁拉起谁）

```
xlerobot-handoff.service            Type=oneshot + RemainAfterExit=yes
  ├─ Conflicts/After  dimos ×2（coordinator / web）+ 旧 full S0 ×2
  └─ ExecStart        preflight-handoff（全 17 电机 / 20 条写）
        ▲ Requires + After
        │
xlerobot-s0-<side>-arm.service      ExecStart = guard（六步）→ exec s0_daemon
        ▲ Requires + After
        ├── xlerobot-s1-<side>.service
        └── xlerobot-gateway-<side>.service   （另 Wants + After 同臂 S1）
```

- **handoff 是旧栈停止顺序的第一责任人**：conflict 目标的 stop job 按 `After`
  排在它 start 之前完成。
- `RemainAfterExit=yes` → 在 handoff 仍为 active/exited 时，启动第二条臂不会
  重跑 handoff；该状态只说明**上一次命令完成**，不证明当前所有电机安全。
  Orin reboot 会丢失此完成态，下一次依赖启动会重跑；**Orin 重启不等于电机
  电源断开**，不能推断残余扭矩已消失，仍需新的现场/通信边界。
- 首次启动一条臂也会先对全部 17 电机执行 handoff；之后本臂 S0 的 enable
  只处理本臂 6 个电机。另一臂/head/base 的上次成功 readback 不是持续状态
  证明，arm-only S0 不提供跨臂联锁或它们的运行期安全保障。
- watchdog 重启只重跑本臂 guard（handoff 因 `RemainAfterExit` 不重跑）。

下面仅说明依赖入口；实际安装后的分步启动按 §1.5 / §5，不能跳过前置：

```bash
sudo systemctl start xlerobot-gateway-left.service   # 会自动拉起 handoff → S0 → S1
```

---

## 3. handoff transaction 做了什么

`xlerobot_deploy preflight-handoff` 对**两条 bus 全部 17 个电机**执行：

| bus | 电机 | 安全写 |
|---|---|---|
| bus1（左） | ID 1-8（6 臂 + 2 head） | `Torque_Enable = 0` —— 8 条 |
| bus2（右） | ID 1-6（6 臂） | `Torque_Enable = 0` —— 6 条 |
| bus2（右） | ID 7-9（3 wheels） | 先 `Goal_Velocity = 0` **再** `Torque_Enable = 0` —— 6 条 |

合计 **20 条安全写**，写序严格 = 左 `[T1..T8]`、右
`[T1..T6, V7,T7, V8,T8, V9,T9]`（**每轮零速先于同轮断扭**），随后**逐项
readback** 断言值为 0。

- **单项失败不中断**：继续处理其余电机，末尾聚合；
- **一侧串口构造失败不阻断另一侧**：失败侧记 `Connect{arm}` 并跳过整条写序，
  另一侧照常完整执行；
- 任一失败 → 整体 FAIL（exit 70）→ handoff unit failed → 两个 arm S0 unit
  因 `Requires=` 不会启动。

单独运行的诊断入口如下：它们虽不改变 unit 状态，**都会打开真实串口并执行安全
写和 readback**，不是只读检查。须两条/对应 bus 已无占用且具备上述物理条件，
不能拿来探测仍运行的实例；也不能据手动成功伪造 handoff unit 的完成态。

```bash
target/release/xlerobot_deploy preflight-handoff \
    --left-config /etc/robot/xlerobot_left_arm.toml \
    --right-config /etc/robot/xlerobot_right_arm.toml
target/release/xlerobot_deploy preflight-arm --arm left \
    --config /etc/robot/xlerobot_left_arm.toml
```

---

## 4. guard 六步与退出码

`ExecStart` 是 **guard**，不是 `s0_daemon`——当前两个 arm 模板实际配置
`RestartPreventExitStatus=86 87 89`，
只作用于主进程，pre 步放 `ExecStartPre=` 会让停止态失效。

六步前先校验参数、D2 与 latch 名称；结构性参数/D2 错误退 86，不消耗连败
预算。latch 名称不合法退 **1**，不是 86，也在任何锁/计数副作用之前拒绝。

1. 持 `/run/robot-system/preflight/<robot_id>.lock`（**稳定 inode**，永不
   rename / 删除），读 `<robot_id>.count`；`>= 5` → **退 86**；
2. **在任何 preflight / provision 之前**原子写入 `n+1`（写失败 → 86）；
3. 本 bus 全量 ensure-safe（左 8 条写 / 右 12 条写 + 各自 readback）；
4. latch provision（三分支 + 统一 parent-dir fsync）；
5. 两步全绿 → **清零计数**（清零失败 → 86）；
6. 显式关锁 → `exec s0_daemon`（自动追加 `--config`）。

**guard / handoff 与 daemon 是不同调用边界**：guard 第 ③ 步直接
`open_arm_bus → ensure_safe_arm`，provision 之后才 exec。它没有 daemon 的
RX marker 消费、BootSupervisor 或串口执行器启动门；不能说 daemon 监督已覆盖
preflight，也不能说 marker 未授权就保证 guard 不会触碰串口。guard 第 ④ 步
才尝试 latch 稳定锁，所以 guard 退 89 之前可能已经执行本 bus 安全写。

计数语义 = **未成功 guard 尝试数**（pre-increment）：guard 在慢 preflight 中
panic / 被 SIGKILL 也保留已递增值，下次启动接着累计。

| 退出码 | 含义 | systemd 行为 |
|---|---|---|
| `0` | daemon 完成其正常收尾；guard 成功是 exec 不返回 | `on-failure` 不重启；不是机械安全证明 |
| `1` | daemon 启动/运行/文件失败，或 owned 关闭真值表不满足（含失败/残余）；guard latch 名称/对象/锁 I/O 等错误也可退 1 | `on-failure`，受 unit 限流；保留具体来源 |
| `2` | CLI/配置解析错误 | `on-failure`，不能靠重试修配置 |
| `3` / `4` | `rx-trust` 维护 CLI 的部分提交结果，精确定义见 §5.2；其他启动错误按实际来源分析 | 维护 CLI 不应装成服务；不能把同一数字脱离命令上下文解释 |
| `70` | preflight 失败（连败未满 5） | `RestartSec=2s` 重试 |
| `71` | provision 失败（连败未满 5） | `RestartSec=2s` 重试 |
| `86` | **guard 停止态**：预算耗尽 / 计数 I/O / guard 实例锁冲突 / 结构性 exec 前置失败（含 D2、重复 config 等）；daemon 保留此码 | 停止自动重启 |
| `87` | daemon / marker CLI 的 **RX 信任目录稳定锁**被另一进程持有 | 两个 arm unit 停止自动重启；不是 latch 锁或授权不足 |
| `88` | daemon main 首句启动的独立监督器，READY 前启动未按既有截止完成，直接 `_exit` | **不在禁止重启表**，受 `on-failure` 与 unit 限流；不覆盖 exec 前 guard |
| `89` | **latch 目标稳定锁**被另一进程持有；guard provision 与 daemon 同码 | 停止自动重启；guard 已预增的计数保留，即使第 5 次也不改成 86 |
| `90` | READY 后独立运行监督器在原截止到达且两次复核同一条件后强退（含关闭预算超时）；不是普通关闭真值表的残余分支 | 不在禁止重启表；保留原退出与残余证据，不能当作正常停机 |

两档预算彼此独立：

- **运行期崩溃 / watchdog 风暴** → unit 级 `StartLimitIntervalSec=60s` +
  `StartLimitBurst=5`；
- **preflight/provision 连败** → guard 自管、**与墙钟无关**的计数（单次 pre
  最坏 ~12 s，会滚出 60 s 窗口，unit 级限流抓不住慢失败循环）。

### 4.1 READY、监督与实际线程范围

daemon main 的首句启动 BootSupervisor，之后才解析、取 RX 锁、取 latch 锁、
恢复存储与构造 adapter。RX 锁由 daemon 与维护 CLI 共享并持至退出；正常 daemon
再持 latch 锁至退出，维护 CLI 不取 latch 锁也不构造 adapter。真实 Feetech 固定
marker 策略，没有 `static` 绕过入口。marker 消费发生在 adapter 构造之后、
worker 启动之前，因此“未授权”也不能被解释为整个进程未打开串口。

当前 S0 必需八个 worker = IPC、ClearEStop RR、SetEnabled RR、S1 heartbeat
watchdog、executor、publisher、I/O、唯一 latch writer；另有 main、独立监督器，
运行期装配 health monitor，并按环境装配 systemd watchdog。SerialRT 的 executor 按实际
adapter 工作方式运行；不要把“八 worker”当作进程总 TID 数。READY 前先收敛
启动与故障 ACK；READY 后由独立监督器观察同一真实 authority/writer，不启动
第二套权限或影子账本。正常关闭保存真实 ACK、退休、持久化与 join 结果；writer
未真实 join 不能用兜底写伪造完成，丢句柄不算回收。

S1 的 liveness observer 是 tick loop、heartbeat publisher、四个 RR server、
可选 state cache；另有事件回收/动态 drain 与 systemd watchdog，不能套 S0 的
严格关闭结论。Gateway 观察六个 RR、EStop publisher、state pool 与 tonic 探针，
state pool / Tokio 线程数量取决于实际装配；其 READY 在 worker ready、bind、serve
和 loopback GetState 探针首次成功之后。S1 退出不因 `Wants=` 自动停 Gateway。

两个 arm unit 都是 `Type=notify`、`NotifyAccess=main`，guard exec 保持 MainPID；
daemon 明确带 **`--notify-mode required`**，缺通知 socket 或发送失败均不是
成功启动。`standalone` 不能拿来规避服务 READY 验收；该参数属于 S0，不能擅自
塞给 S1/Gateway。监督器强退 88/90 不保证析构/最后日志/断扭已经完成，外层
systemd 重启也不会重新授予 RX 信任。

### 4.2 保留的 unit 预算与权限

- **`TimeoutStartSec=infinity`**（handoff + 两条 arm S0）：D13 明确"给
  preflight 定义可证明总时限"这一备选**未采**——任何有限值都可能在"部分电机
  已写、其余尚未处理"时把进程杀掉，留下一条半安全的 bus 且没有聚合报告。
  单次 preflight 最坏 ~12 s/bus 本身有界；真卡死由人工介入。
- **生产 `WatchdogSec`**：S1 / gateway = **5s**（Spec D12 拍板），S0 = 2s；
  `scripts/systemd/*-freeze-test.service` 测试模板保持 2s。daemon 侧 ping
  周期恒 = systemd 注入的 `WATCHDOG_USEC / 2`，代码里没有硬编码。

`TimeoutStartSec=infinity` 保留的是 guard/handoff 既有策略，并不关闭 exec 后
daemon 的有限启动监督。当前模板 S0 为 `LimitRTPRIO=99`、`LimitMEMLOCK=infinity`、
`CPUAffinity=0-3`，串口线程自行设 FIFO 70 并 pin 左 4 / 右 5；未设限制串口核的
`AllowedCPUs`，也不能加 `RestrictRealtime` 阻断既定调度。实际 cgroup/权限与
每个 TID 必须在现场核实，配置不是测量结果。

`RuntimeDirectory=robot-system/preflight` + `RuntimeDirectoryMode=0750` +
`RuntimeDirectoryPreserve=yes` 四态：**首次启动自动创建 / restart 保留 /
手工 stop-start 保留 / reboot 清零**（/run 是 tmpfs，计数**不**具备掉电
持久性；reboot 后重新执行 handoff 与 guard）。

---

## 5. 人工恢复：先分清计数、锁、通信信任与 EStop

### 5.1 失败分类和停止点

先保存本轮 journal、真实进程退出/信号、unit 的 Result/ExecMainStatus、原 PID / TID
与文件身份；退出状态不可用时不要用“日志最后一行正常”代替。按 §1.2 停对应
业务链并确认没有手工实例/残余持有者；共享文件改动、全 bus handoff 或整包回滚
必须停双臂。Enabled 或机械状态未知按 §7 先处理现场。

| 对象 | 位置 / 生命周期 | 恢复边界 |
|---|---|---|
| guard 连败计数 | `/run/robot-system/preflight/<robot_id>.count` | 仅记录未成功 guard 尝试；修明根因并确认无 guard 后才可定向清 count。它不是 RX 授权 |
| guard 实例锁 | 同目录 `<robot_id>.lock` | 稳定 inode；86 中的这一类须查真实持有者，不删锁解冲突 |
| RX 信任锁 | trust 目录 `rx_trust.lock` | daemon / marker CLI 共用；87 先查同目录并发者，持锁者的真实退出后由 OS 释放 |
| RX marker 双份 | `rx_trust_marker`、`rx_trust_marker.bak` | 严格 `0\n` 双份为一次性授权候选，`1\n` 双份为 canonical Dirty；缺失/坏值/不一致不授信 |
| EStop latch 与待清事务 | `estop_latch`、`.bak`、`.clear_pending` | daemon / guard 的真实恢复逻辑管理；不得删除、手写清零或从旧快照覆盖 |
| latch 稳定锁 | `estop_latch.lock` | guard provision 临时持有；daemon 持至退出；89 与 RX 锁 87 是不同来源 |

默认 trust 目录沿 latch 父目录：左 `/var/lib/robot/xlerobot_left`，右
`/var/lib/robot/xlerobot_right`。生产模板没有另设 `--rx-trust-dir`；维护 CLI 必须
显式使用与目标 daemon 一致的目录，不能只换 robot_id 却指向别臂文件。

86：修复配置/计数 I/O/串口/供电或锁冲突的具体原因，再按需删除**本臂 `.count`**；
不要清整个 `/run/robot-system/preflight`。87/89：核 `/proc`、文件锁与进程身份，
正常停止真正持有者并确认退出；锁内 PID 文字只用于诊断，不是活进程的充分证据。
不要通过删除、rename 锁文件或复制另一个锁来“解锁”。88/90/1：保存启动阶段、
真实线程收尾与存储错误，不能只清 count 掩盖失败。原 unit 自动重启策略保持，
人工处置先停服务阻止故障循环；不增加额外 restart 脚本。

根因修复且全部前置满足后，若确实触发 start-limit，可对**具名目标 unit**
`systemctl reset-failed`，再执行本次获准的单次 start。reset-failed 不清 guard
预算、不授 RX 信任、不清 EStop，也不能在未知残余句柄时用来促成重启。

### 5.2 一次性 RX 信任的人工操作

H2 监督驱动 `scripts/run_post_e_h2.py --side left|right` 默认 `left`；不带
`--execute` 只打印计划。右臂必须同时绑定 `inputs.side=right`、当次
`physical.side=right` 与 `--side right`，配置、USB、标定、TRUST 和串口
FIFO70@CPU5 均按 §0.1 的右侧 D2 行核验（左为 CPU4）。V1 输入缺 `side`
只兼容左臂，不能据此运行右臂。两侧仍检查全部六服务、双配置与全量进程清场。
右臂先独立完成新 H1/H3 及现场事实验收，再进入 H2；不能复用左侧结果或已消费
授权。本工具更新须按新源码绑定三 Python 与真实构建来源，不能替换正在等待或
执行左 H2 的冻结工具。当前格式报告必须有原始 `final_trace.json` 并与报告一致，
确认两 child 已停且最终 Disarm 无 sticky；原 `final_stop` 的真实 DisableAck
仍独立必需，旧缺原件报告不回填、不静默作为 PASS。

`rx-trust` 子命令在打开 adapter 之前分流，**不触串口**，但 status 也会获取 RX
稳定锁并写诊断 PID，不能与运行中的同目录 daemon 并发。当进程仍活着时获得 87
应停止维护，不得强抢。真实设备信任来自新的物理供电/USB 边界和现场事实，文件
命令本身不能证明它们发生过。

左臂命令面如下，均以服务用户 `loosqk` 运行，并绑定 §1 清单里的正式 S0 ELF；
这里的模板根只有与现场清单一致时才可使用。右臂须独立换为右 ID/目录，不能
在循环中同时授权两臂。

```bash
/home/loosqk/repo/robot/robot-system/target/release/s0_daemon rx-trust status \
  --robot-id xlerobot_left --rx-trust-dir /var/lib/robot/xlerobot_left
```

1. 在任何marker CLI前，以loosqk复用驱动的`inspect_runtime_preconditions`、
   `validate_runtime_preconditions`与`process_snapshot`，核实际UID/EUID/GID/EGID、
   两函数均显式传入所选侧 TRUST（默认仅左），并核返回路径一致；检查
   RT/MEMLOCK、TRUST属主及0750、实际进程读取能力；仅sudo扫描通过不能替代普通路径。
   确认对应链已停止、没有RX/latch/串口持有者，核D2、实际ELF/config、目录与锁身份。
   再status并保存主/备状态、退出码；status退0只表示命令完成，
   **不表示已经授权**，必须读实际状态输出。
2. 双份不是 canonical Dirty 时，调查是否存在旧的未消费授权/部分写。使用同一
   正式 ELF、ID/目录执行 `rx-trust revoke` 并重新 status；仅在根因已处理后单次
   规范化，不能写“重试直到成功”。未达到双份 Dirty 时停在错误处。
3. 在开始本次真实测试前，重新记录机械支撑、清空负载与接触区、操作者持续在场、
   立即断电能力，以及**本臂电机总线断电重上电和 USB 物理拔插/重新枚举**。历史
   “已完成”不能沿用，Orin reboot / 重开 fd / 停服务也不能代替此边界。
4. 从 canonical Dirty 出发，同参数**单次** `rx-trust authorize`，保留结果与
   status。只有命令完整成功且观察到双份 AuthorizedOnce 才继续本次启动；这是
   下一次 daemon 消费的候选，不是永续“可信”状态。中途出错或再次重启需重新
   判定现场条件，不能自动再 authorize。
5. 按 §1.5 单次启动。daemon 消费时先持久写备份 Dirty、再写主文件 Dirty，只有
   双份消费成功才取得本次授权；此后文件为 Dirty 是正常消费结果，不能因看到
   Dirty 就补发授权。授权也不清 EStop latch，不等于 SetEnabled 放行。

维护 CLI 的退出码须连同命令看：

| 退出码 | 实际含义与处置 |
|---|---|
| `0` | 命令完成；status 仍须读主/备状态，authorize/revoke 仍须核各自目标状态 |
| `2` | 参数拒绝或操作被拒；authorize 非 canonical Dirty 也走此码，不推断无任何文件变化 |
| `3` | authorize 主文件已提交但持久化屏障未完成，**授权可能已经发生**；保原结果，停下调查，不当成“未授权所以安全重试” |
| `4` | revoke 已撤销授权候选，但尚未完成 canonical Dirty 双份；需核原因与规范化结果，不直接 authorize |
| `87` | RX 稳定锁竞争；先处理真实持有者，不删锁 |
| `1` | 系统/目录/锁/文件错误；保留原错误，不放宽文件策略 |

任一启动后失败、SSF、超时或不明通信状态，按现场风险停止并保留真实 final_stop
及进程回收结果。新的硬件驱动须另审，不能复用旧 H2 目录/旧采样片段，也不能拿
SoftwareLoad 的 `s48_lifecycle` 或 EXPECTED_RED 判据代替 Physical 收尾。
ClearEStop 只经 maintainer+ 的既有 RPC 事务，成功后仍 Safe；上扭是单独动作。

---

## 6. 旧栈处置（D-BI3 r5 现行口径，2026-09-01）

以下保留历史决策与当时的执行事实；当前远端 enable/文件状态仍须按 §1.1 新采，
本次文档对齐没有重新验证或变更远端旧栈。

- 允许：为释放硬件而 `systemctl stop`（handoff 的 `Conflicts=` 也会自动做
  这件事）。
- **r5（2026-09-01，用户拍板）：coordinator / web 已 `systemctl disable`**
  （仅关自启，unit 文件保留；触发事实 = isolcpus 重启后旧栈 enabled 自启
  占双串口）。回滚 = 显式 `systemctl enable`（如需自启）+ `start`。
- **仍然禁止**：删除 unit / `mask` / 移除 LCM 等运行时——即任何破坏
  回滚路径的动作。
- 正式退役、删除、LCM 随仓退出——**全部等 post-E 生产安全候选闭环**
  （决策单 D-BI3 r3 时点不变；r5 只解除 disable 禁令，见 CHANGELOG
  2026-09-01 + 决策单 r5 修订块）。

旧栈 unit：`xlerobot-coordinator.service` / `xlerobot-web.service` /
`xlerobot-s0-bus1.service` / `xlerobot-s0-bus2.service`。

---

## 7. 应急协议（Enabled 期间，权威文本见 Spec P.2.10）

**Enabled 期间任一 S0 进程死亡 / 重启 → 立即人工切断总电源并中止本轮。**

**同款人工协议扩到整条链（D17 已登记的 client 死亡风险 + 通信韧性评估
§6.1）**：Enabled 期间 `supervised_demo`（验收 bin）/ gateway / S1 任一
**死亡或挂起** → 同样立即人工切断总电源并中止本轮——不能依赖已死亡的 client
继续发 cancel / 双 EStop，也不能据其他软件路径推断机械已安全；
跨臂也没有 daemon 级联锁（一臂 Fault 不会自动断另一臂的扭）。

**不得只依赖 EStop 弱 Ack**——S0-dead 时 Ack = `published_at_ns`，**不证明
事件已被消费**。

恢复 = 断电确认 → 排查根因 → 重新走安全边界 checklist 从步 1 起
（checklist 与 demo 步序在段(10-demo)，本 runbook 不代替）。

---

## 8. 相关文件

| 文件 | 作用 |
|---|---|
| `scripts/systemd/xlerobot-handoff.service` | 全 17 电机交接（唯一入口） |
| `scripts/systemd/xlerobot-s0-{left,right}-arm.service` | guard → `exec s0_daemon` |
| `scripts/systemd/xlerobot-s1-{left,right}.service` | S1 coordinator（硬依赖同臂 S0） |
| `scripts/systemd/xlerobot-gateway-{left,right}.service` | gRPC gateway（50051 / 50052） |
| `scripts/systemd/README.md` | 旧 full-bus unit 的安装 / 验证说明（保留） |
| `crates/robot-s0/src/deploy/` | identity / guard / preflight / provision / counter 实现 |
| `crates/robot-s0/tests/deploy_units.rs` | unit 指令文本 + 三处 config 路径一致锁 |
| `crates/robot-s0/tests/deploy_guard_kill.rs` | pre-increment 崩溃注入（真 SIGKILL） |
| `crates/robot-s0/src/bin/s0_daemon.rs` | marker / latch 锁、八 worker、required READY 与 owned 关闭的实际接线 |
| `crates/robot-s0/src/boot_supervisor.rs`、`shutdown_mechanism.rs` | 88/90 监督与真实句柄/责任收尾 |
| `crates/robot-s0/src/rx_trust.rs`、`latch_lock.rs` | 一次性授权、稳定锁及文件权限 |
| `team-swe/specs/post_e_comm_fault_hardening.PLAN.md` P.7/P.8 | 整候选操作、回滚与阶段边界；当前进度看对应 HANDOFF/H 索引 |

---

## 9. 受监督 demo（段(10-demo)；权威步序 = Spec P.2.10）

> **本节保留候选 E 历史 demo 命令面，不是当前通信候选 H2 验收脚本。**
> 使用 S48 时须先完成 §1/§5 的产物、现场和信任前置；不能直接从本节重新构建或
> 起双臂。下方构建示例属于历史工具阶段，执行时绑定另行验收的工具与正式四件。
> 执行门槛 = AC-E12 安全边界五条 + 操作者在场 + 明确放行；`supervised_demo` 的上扭 phase 都要求 `--ack-safety-checklist`，
> 不带即打印五条并退 2、绝不 `SetEnabled`。

```bash
# 0) demo 驱动件（PX-45；required-features 未满足时 cargo 静默跳过该 bin）
cargo build --release -p robot-tests --features supervised-demo --bin supervised_demo

# 1) 证据基线时刻（5c 取证窗起点）——在起臂之前记录一次：
DEMO_T0=$(date -Is)

#    起目标臂（handoff transaction 自动链：全 17 电机 ensure-safe → guard →
#    s0 → s1 → gateway）。⚠ 物理效应：旧栈被 Conflicts 停掉 + 全部电机断扭
#    ——若此刻有臂被扭矩撑着，会失力下垂。操作者必须在场。
sudo systemctl start xlerobot-gateway-left.service
sudo systemctl start xlerobot-gateway-right.service

# 2) 读状态（只读；不上扭）。附 robotctl 旁路证据（D10）。
ROBOT_TOKEN=operator-fake-token-001 \
  ./target/release/supervised_demo --phase read-state

# 3) 首启 latch 是 latched（guard provision 写 1\n，D2 状态链）：
#    ClearEStop（maintainer+）后仍 Safe，等 SetEnabled。左右各一次。
ROBOT_TOKEN=maintainer-fake-token-001 ./target/release/robotctl clear-estop \
  --confirmation CONFIRM_CLEAR_ESTOP --gateway-addr 127.0.0.1:50051 \
  --audit-reason "demo first-boot latch clear"
ROBOT_TOKEN=maintainer-fake-token-001 ./target/release/robotctl clear-estop \
  --confirmation CONFIRM_CLEAR_ESTOP --gateway-addr 127.0.0.1:50052 \
  --audit-reason "demo first-boot latch clear"

# 4) 安全边界五条逐条确认并记录（证据 docs）→ 单臂 → 双臂 → 双夹爪。
#    每个 phase 独立调用 = operator 显式；phase 内 SetEnabled(true)，
#    phase 末 SetEnabled(false)。
ROBOT_TOKEN=operator-fake-token-001 ./target/release/supervised_demo \
  --phase single-arm --side left  --ack-safety-checklist
ROBOT_TOKEN=operator-fake-token-001 ./target/release/supervised_demo \
  --phase single-arm --side right --ack-safety-checklist
ROBOT_TOKEN=operator-fake-token-001 ./target/release/supervised_demo \
  --phase dual-arm  --ack-safety-checklist
ROBOT_TOKEN=operator-fake-token-001 ./target/release/supervised_demo \
  --phase grippers  --ack-safety-checklist

# 5) 资源基线 + 现状证据（只采不执法；P.2.10 + 通信韧性评估 §6.1）：demo 窗口内
./target/release/iceoryx2_rt_baseline --duration 10m --hz 50   # synthetic transport latency
for u in xlerobot-s0-left-arm xlerobot-s1-left xlerobot-gateway-left \
         xlerobot-s0-right-arm xlerobot-s1-right xlerobot-gateway-right; do
  systemctl show -p MainPID $u.service; done                    # (comm,pid) join 映射

# 5b) 评估 §6.1 记录项（不改生产代码，只留证据）：
#     USB 拓扑 / 控制器归属（"不同 by-id ≠ 不同 root hub"）——从**权威
#     by-id 串号**解析，任何一步失败即中止（不吞错误）：
set -e
for ser in 5A7C121427 5A7C122340; do
  link=/dev/serial/by-id/usb-1a86_USB_Single_Serial_${ser}-if00
  dev=$(readlink -f "$link")
  syspath=$(udevadm info -q path -n "$dev")
  echo "== serial=$ser dev=$dev"
  echo "   syspath=$syspath"          # usbN 段 = root hub / 控制器归属
done
lsusb -t
#     控制器级 IRQ：cdc_acm 是 USB 类驱动，无独立 IRQ 行——有意义的记录是
#     xHCI 控制器的 IRQ 分布（两串口若同 xHCI 即同 IRQ 故障域）。
grep -iE 'xhci' /proc/interrupts
set +e
#     每 S0 的调度落点（左右各一次，保存对照）
for side in left right; do
  systemctl show xlerobot-s0-$side-arm.service \
    -p MainPID -p AllowedCPUs -p CPUAffinity -p CPUWeight \
    -p MemoryCurrent -p MemoryMin -p MemoryMax -p OOMScoreAdjust
  ps -L -p "$(systemctl show -p MainPID --value xlerobot-s0-$side-arm.service)" \
    -o pid,tid,psr,cls,rtprio,pri,ni,comm
done

# 5c) CPU/RSS 采样归档（Spec P.2.10：warmup 丢首帧后 ≥ 30 帧/进程）。
#     数据源事实：health 帧只由**两个 S0** 的 monitor 线程产生（robot-health
#     挂在 s0_daemon；S1/gateway 自身不发），日志正文是 "system health"，
#     interested 字段形如 `<pid>:<comm>@<cpu>%[<rss>M]`——每个 S0 的 monitor
#     按 comm 全局匹配，六个进程的 (pid,comm) 都出现在任一 S0 的 journal 里。
#     **权威采样源固定为左臂 S0 单条 journal**：两条合并时每个 monitor 各有
#     独立的 CPU 差分首帧，"减一帧"会计数膨胀（round 3 修正）。
#     [monitor] interval_secs = 1 → 尾窗 ≥ 40 s（丢首帧后仍 ≥ 30 帧）。
: "${DEMO_T0:?DEMO_T0 must be set before demo (§9 step 1)}"  # 守门：步 1 已记录，此处**不再赋值**
sleep 40
# F4（真机实测）：旧 journalctl 不认带时区的 ISO —— 用 date -d 转本地格式
T0_J=$(date -d "$DEMO_T0" "+%Y-%m-%d %H:%M:%S")
journalctl -b -u xlerobot-s0-left-arm.service \
  --since "$T0_J" --no-pager -o cat | grep -F "system health" \
  > /tmp/demo_health.log
fail=0
for u in xlerobot-s0-left-arm xlerobot-s1-left xlerobot-gateway-left \
         xlerobot-s0-right-arm xlerobot-s1-right xlerobot-gateway-right; do
  pid=$(systemctl show -p MainPID --value $u.service)
  [ -n "$pid" ] && [ "$pid" != 0 ] || { echo "FAIL: $u no MainPID"; fail=1; continue; }
  # 按**当前 MainPID** 逐进程计帧。F5（真机实测）：生产 unit 用
  # ROBOT_LOG_FORMAT=json，interested 是 JSON 字段 "interested":"<pid>:…"
  # ——不是控制台的 interested=<pid>:；前界 = 引号或 ','（防 12340 误配 1234）
  frames=$(grep -cE "\"interested\":\"([^\"]*,)?${pid}:" /tmp/demo_health.log)
  usable=$((frames > 0 ? frames - 1 : 0))   # 丢首帧（单一 monitor 源 → 恰一帧 CPU 差分无效）
  echo "$u pid=$pid frames=$frames usable=$usable"
  [ "$usable" -ge 30 ] || { echo "FAIL: $u usable=$usable < 30 —— 延长尾窗重跑本节"; fail=1; }
done
test "$fail" -eq 0   # ← 本节退出码（非零 = 取证不合格；归档前必须重跑，round 3 修正）
```

- 判据数值（AC-E12 ③，bin 内双层强制）：|target−anchor| ≤ 0.087 rad、
  斜率 ≤ 0.14 rad/s；缺省探针 = `*wrist_roll*` ±0.05 rad、出→回各 0.5 s。
- **应急协议**（§7 同文）：Enabled 期间 **S0 / S1 / gateway /
  `supervised_demo` 任一死亡或挂起** → 立即人工切断总电源并中止本轮。
- demo 结束宣称唯一口径 = **"受监督功能 demo 跑通"**。


---

## 10. iceoryx2 运行用户与残留处置

同一部署中所有参与同一 iceoryx2 通信域的进程保持同一非 root 用户；当前 unit
模板为 `loosqk`，现场必须核实际 UID。sudo 调试/管理权限的既有授权继续有效：
可以用 sudo 管服务、检查受限进程、设置所需权限/rlimit，但最终 IPC 进程的有效
UID 与资源所有者须一致。不能把整条测试命令提升为 root 后，与 `loosqk` 服务
混跑；也不能仅凭 sudo 成功就宣称 FIFO/CPU/内存锁达标。

H2驱动先绑定PID/start_ticks/boot/PGID/UID，仅对权限不足候选执行一次有限固定sudo
只读补证。启动计数0可以合法；负值、缺字段、未知或身份变化仍拒绝，不按ssh/pam
名称豁免，不以sudo运行整个驱动。前置拒绝不自动重授权或重跑。

历史 F8 的 root 属主残留曾导致非 root node 创建 `InternalError`，IPC panic
发生在 guard exec 之后；成功 guard 已清零计数，因此该事故不由其连败预算
兜底。这只是一个已知来源，当前同类报错仍须核实际路径/权限/持有者，不能直接
推断“又是 root 污染”。程序级 root fail-fast 未实现，不将其写成现有保护。

处理按以下顺序进行，**不提供一条通配删除再双臂自启的命令**：

1. 保存错误、相关文件的路径/类型/UID/GID/mode 与 unit/手工进程身份；权限不足
   用 sudo 查看，不能吞 stderr 把 Permission denied 当文件不存在。
2. 按 §1.2 停 Gateway → S1 → S0，核两臂和所有测量/手工进程真实退出；handoff
   不作为清理对象。不能只 `pgrep iceoryx2`，daemon 名称本就不含该字符串。
3. 盘点本次通信域实际使用的 `/tmp/iceoryx2/` 管理对象及 `/dev/shm/iox2_*` 中
   对应对象，结合 node/service 身份、进程 fd/map 与 `fuser`/`lsof` 查真正持有者。
   全局管理资源可能属于别的任务，不能只凭前缀归本项目。使用 `fuser` 时，
   exit 0 是仍有使用者；只有 exit 1 且无输出才能作为该项无使用者证据，其他
   错误/未知状态一律先排查。还须核管理目录下对象，不能只检查 shm。
4. 经独立确认属于本次残留、没有任何持有者且无需保留后，按明确路径列表定向
   清理或使用对应现有 Node 清理流程，保存动作/返回值；禁止无差别
   `rm -rf /tmp/iceoryx2 /dev/shm/iox2_*`。任何 RX/latch/guard 稳定锁及安全状态
   文件都不在这一清理清单内。
5. 使用同一非 root 身份做获准的软件恢复检查并记录结果。若涉及真机重新启动，
   回到 §5 的新现场/通信边界和 §1.5 的分步启动；清理残留不授权电机、不恢复
   一次性信任、不自动 reset-failed 所有 `xlerobot-*` 或同时起双臂。
