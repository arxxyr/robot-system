# rr-memory-model

RR R1 的 Loom 同源内存模型。独立测试 workspace（`publish = false`，无产品版本），
根 `Cargo.toml` 的 `workspace.exclude` 已列出本目录；不进入任何生产依赖图。

## 测试一览

| 具名测试 | 种类 | 线程 | 内容 |
|---|---|---|---|
| `model_normal_close_publishes_tail` | 正向，完整探索 | client（根）+ server | 两种初态各自 `check` 一次、各自计数、各一行报告：**普通**（spawn 前 `prepare`）与**预置 hint**（spawn 前 `prepare` + `set_hint`）。server 发布尾（Relaxed）后同源 `finish`（Release）；spawn 后 client **只做一次** `classify_prepared`（Acquire）再读尾，期间不 hint、不 join、不读额外 ACK、无任何额外 Acquire。看到 Closed 就必须看到尾。`normal_closed` 与 `hinted_closed` 分别非零。 |
| `model_negative_control_relaxed_finish_is_detected` | 负对照，`should_panic` | 同上（普通初态） | 模型侧适配 `WeakenedCas` 把 server CAS 的成功 / 失败序降为 Relaxed，仍调用同源 `finish`；Loom 必须找到"Closed 可见但尾不可见"的反例。 |
| `model_negative_control_relaxed_observer_is_detected` | 负对照，`should_panic` | 同上（普通初态） | 模型侧适配 `WeakenedLoad` 把观察者 load 降为 Relaxed，仍调用同源 `classify_prepared`；Loom 必须找到同样的反例。 |
| `model_cleanup_never_proves_tail_drained` | 正向，完整探索 | client + server + cleaner | cleaner 只走同源 `mark_orphaned`（不 join server、不读 ACK）。正常关闭赢与清理赢两种 CAS 胜者都非零、互斥；看到 ORPHANED 只能 Unknown，且存在尾未发布的排列（ORPHANED 不携带发布关系）。 |
| `model_hint_prepare_cleanup_and_generation_never_upgrade_unknown` | 正向，完整探索 | client + server + cleaner | 三方真实 CAS 竞争（client 并发 `set_hint`）后：清理赢则迟到 `finish` / `set_hint` / `prepare(REQ2)` / `prepare(REQ)` 全部不重开、`classify_prepared` 为 Unknown(Orphaned)；正常关闭赢则 `prepare(REQ2)` 成功（合法复用），此时旧请求分类为 Unknown(Open(REQ2))，新请求 Open。hint 先于 / 晚于关闭与清理四种分支均非零。 |
| `model_capability_boundary_reused_closed_is_indistinguishable` | 能力边界，单线程 | 根 | 顺序执行 `prepare(REQ) → finish(REQ) → prepare(REQ2) → finish(REQ2)`，断言 `classify_prepared(state, REQ)` **确实返回 Closed**（非零反例计数）；从未 prepare 的初始 CLOSED 同样返回 Closed；对照：复用代次仍 Open 时旧请求为 Unknown(Open(REQ2))。这是 CAL 原语的能力边界（同值 CLOSED 分不清初始 / 复用代次），不是生产保证通过；R2 必须用原 route / pin 阻止复用，本包不新增伪 generation 原子。见"边界"。 |

模型只通过 `src/lib.rs` 的 `#[path]` 包含仓库同一份
`3rd-party/iceoryx2-cal/src/zero_copy_connection/channel_state.rs`；模型侧只提供
`loom::sync::atomic::AtomicU64` 的 `AtomicState` 适配（直接转发同一 Ordering），不复制
CAS / 算法，不加 Barrier / 锁 / 额外 Acquire。两个负对照只更换模型侧适配，仍调用同源函数，
绝不改生产源。计数用 std 原子，只在断言与 join 之后累加（反例计数在断言失败前一行记录），
不参与模型调度或同步。

## 运行

在仓库根执行（Cargo 由本方串行执行）：

```bash
cargo test --locked --manifest-path tools/rr-memory-model/Cargo.toml \
  --target-dir target/rr-memory-model -- --nocapture --test-threads=1
```

## 报告行

每个测试打印可 grep 的报告行，格式：

```
MODEL <测试名> kind=<positive|negative_control|capability_boundary> <字段>=<值> ...
```

两类计数含义不同，不能混读：

- `kind=positive`：`iterations=N` 是 `check` **完整探索**真实执行的排列数，必须非零；
  模型 1 两种初态各一行（`variant=plain` / `variant=hinted`）。随后按名断言各关键分支
  计数非零，`counterexample` 必须为 0。
- `kind=negative_control`：`counterexample_found=true iterations_before_counterexample=N`
  只表示 Loom 在第 N+1 次排列找到了反例（前 N 次排列成功结束），**不是穷举**。计数器由
  测试函数在 check 闭包外持有，反例 panic 被捕获后仍读到真实值，不填 0；`counterexample`
  是反例断言失败前记录的次数（应为 1）。确认 panic 载荷为"观察到 Closed 后尾内容不可见"
  后以同一文本重新 panic 供 `should_panic(expected)` 匹配。若探索完整结束而没有反例，
  打印 `counterexample_found=false iterations=N` 并以其它文本失败（不匹配 expected）。
- `kind=capability_boundary`：单线程一次执行（`iterations=1`），
  `fresh_closed_reported_closed` 与 `reused_closed_reported_closed` 是记录的反例计数，
  `reused_open_reported_unknown` 是对照。

`--test-threads=1` 只为让报告行与测试名逐一对应。

## Builder 参数（显式覆盖，不继承任何 `LOOM_*` 环境变量）

| 字段 | 值 |
|---|---|
| `max_threads` | 4（含模型根线程；worker ≤ 3） |
| `max_branches` | 4096（每次排列的线程切换上限，超出即 panic，不是截断） |
| `max_permutations` / `max_duration` / `preemption_bound` | `None`（不截断探索） |
| `checkpoint_file` | `None`；`checkpoint_interval` 显式 20000（无文件时不生效） |
| `expect_explicit_explore` | `false` |
| `location` / `log` | `false` |

## 边界：同源状态机证明了什么，什么仍是 R2 的调用方前提

已由同源状态机（Loom 完整探索）证明的：

- 持有本次 prepare 记录且 channel 尚未被复用时，正常 finish 的 Release 与
  `classify_prepared` 的 Acquire 形成 happens-before：看到 Closed 就看到关闭前发布的尾（普通与
  hint 两种初态）；两个负对照证明该结论对 Release / Acquire 缺失敏感。
- 清理者写入的 ORPHANED 不携带发布关系（存在尾未发布的排列），只能 Unknown；正常关闭与清理
  恰一胜者，清理赢后迟到的 finish / hint / prepare 都不重开。
- 合法复用（正常关闭后 `prepare(REQ2)`）成功；复用代次仍 Open 时旧请求分类为 Unknown。

**尚待 R2 的调用方前提（本包只记录反例，不解决）：**

- `classify_prepared` 只看单一状态字。复用代次关闭后的 CLOSED 与初始 CLOSED 是同一个值，
  原语对旧请求同样返回 Closed（能力边界测试记录非零反例）。因此"旧请求完成"只有在
  **原 route / pin 仍被持有、channel 未被复用**的前提下才成立；R2 必须由原 route / pin 与
  channel 复用所有权（未归还的 pin 阻止 prepare 复用、初始空 registry 不给 Closed）阻止该调用
  成为合法完成证据。本包不新增伪 generation 原子、地址身份或自报 bool。
- R1 不自动关闭原代次 / 映射缺陷；R2 的真实 pin 阻止复用、初始空 / 复用代次相关门仍为强制
  验收，不能由本能力边界测试替代。
- Loom 只探索其支持的调度与 C11 可见性子集，不覆盖全部弱内存重排；Release→Acquire 论证与
  x86 / ARM 实测另列。模型 PASS 不等于真实 SHM、映射、allocator 或 ARM 证明，也不代替本方
  IPC 子进程门与真机验证。
- 依赖：仅 dev-dependency `loom =0.7.2`（default-features = false），传递依赖由 `Cargo.lock`
  记录；原包 checksum 见 `Cargo.toml` 注释。
