# 日志规范

robot-system使用`tracing`结构化日志；初始化实现在 [robot-logging](../crates/robot-logging/src/lib.rs)，本文维护字段与使用约定。日志及测试结果留在既有本地或Orin位置，不提交Git。

## 使用要点

1. S0、S1、网关和 Web 的入口显式调用 `robot_logging::init()`。`robot-logging` 链入后也通过 ctor 在 `main` 前尝试初始化，重复调用幂等。
2. **lib / bin 打日志**：`tracing::info!(motor_id = 6, "handshake ok")` —— 用
   结构化字段（`k = v`），不要 `format!` 拼字符串。
3. **错误打印**：`tracing::error!(?err, "feetech connect failed")` —— 保留
   `thiserror` 的结构。
4. **环境变量控制**：`RUST_LOG=debug ./robot-s0` / `ROBOT_LOG_FORMAT=json ./robot-s0`。
5. **RT线程内禁用所有日志调用**；通过既有定长诊断环或原子计数，由非实时线程输出。

## 格式

### Human 模式（默认）

```
2026-04-25T23:45:12.123456Z [+12345.678901234s]  INFO  robot_s0: feetech factory built adapter motor_id=6 address="/dev/..."
```

- **wall clock RFC3339 UTC** + **monotonic `[+秒.纳秒s]`**（从日志初始化开始累计）
  - wall clock 方便跨机器对齐绝对时间（NTP 下）
  - monotonic由`std::time::Instant`产生，仅为日志相对时间，不能直接与业务`CLOCK_MONOTONIC_RAW`的绝对纳秒值相减
- `target` = 模块路径（`robot_s0`），便于定位
- 结构化字段 `k=v` 紧随 message

### JSON 模式

`ROBOT_LOG_FORMAT=json` 切换，每行一个 JSON 对象，方便 journald / Loki / Elastic
采集：

```json
{"timestamp":"2026-04-25T23:45:12.123456Z [+12345.678901234s]","level":"INFO","target":"robot_s0","message":"feetech factory built adapter","motor_id":6,"address":"/dev/..."}
```

## 字段命名约定（词汇表）

**所有 `tracing::*!` 调用共用这张表**。新增字段必须先更新本表 + 登记 CHANGELOG。

| Key            | 类型             | 示例                             | 含义                                                                 |
| -------------- | ---------------- | -------------------------------- | -------------------------------------------------------------------- |
| `robot_id`     | `&str`           | `"xlerobot_bus1_full"`           | TOML `[robot] id`                                                    |
| `hardware_id`  | `&str`           | `"left_arm_full_with_head"`      | TOML `[[hardware]] id`                                               |
| `motor_id`     | `u8`             | `6`                              | Feetech motor ID                                                     |
| `joint`        | `&str`           | `"left_arm_gripper"`             | 关节名                                                               |
| `adapter`      | `&str`           | `"feetech_sts3215"`              | factory 名                                                           |
| `register`     | `&str`           | `"Goal_Position"`                | Feetech 寄存器名                                                     |
| `value`        | `i32`            | `1408`                           | 寄存器写值 / 读值（raw tick 等）                                     |
| `err`          | `?` (Debug)      | `FeetechError::TransportTimeout` | **必须用 `?err` 捕获 thiserror 结构**，不要 `{err}` 拍扁             |
| `attempt`      | `u32`            | `1`                              | 重试计数                                                             |
| `duration_ms`  | `u64`            | `150`                            | 延时毫秒                                                             |
| `cmd_seq`      | `u64`            | `123`                            | JointCommand seq 号                                                  |
| `hb_seq`       | `u64`            | `123`                            | Heartbeat seq 号                                                     |
| `state`        | `%` (Display)    | `"Enabled(Active, Ok, Active)"`  | S0 state machine 当前态（走 `Display`，自定义简洁格式）              |
| `tick_seq`     | `u64`            | `50`                             | RT 线程 tick 计数                                                    |
| `deadline_ns`  | `u64`            | `603924378768480`                | RT tick deadline（`CLOCK_MONOTONIC_RAW`）                            |
| `bus_address`  | `&str`           | `"/dev/serial/by-id/usb-..."`    | 真串口物理路径                                                       |
| `calibration`  | `"loaded"\|"none"` | `"loaded"`                     | factory 加载 calibration 状态                                        |
| `cpu_usage_pct` | `f32`           | `8.91`                           | 系统级 CPU 使用率（0-100，所有 core 合并）                           |
| `load_1m` / `load_5m` / `load_15m` | `f32` | `3.51`                 | 系统 load average                                                     |
| `mem_total_kib` / `mem_available_kib` | `u64` | `16333668`         | `/proc/meminfo` 内存（KiB）                                          |
| `mem_used_pct` | `f32`            | `85.9`                           | 内存使用率（0-100）                                                  |
| `interested`   | `%` (Display)    | `"123:robot-s0@1.5%[20M]"`      | `MonitorSection::interested_processes` 匹配到的进程（`pid:comm@cpu%[rssM]`，`-` = 空） |
| `top_by_cpu`   | `%` (Display)    | `"1:a@111%[38G],2:b@106%[32G]"`  | 全系统按 CPU 占用 top N，`,` 分隔，格式同上                          |

### 字段使用原则

- **名词单数**：`motor_id` 不是 `motors` / `motor_ids`。多值走 `motor_ids` 明确复数。
- **布尔字段命名**：`enabled: bool` / `connected: bool`。不要 `is_*` 前缀（冗余）。
- **单位加后缀**：`duration_ms` / `latency_us` / `bytes` / `rate_hz`。
- **Debug 值用 `?field`**：`error!(?err, ?state, "...")`。
- **Display 值用 `%field`**：`info!(%robot_id, "...")`（避免 Debug 多一层引号）。
- **不要拼 message 字符串**：错 ❌ `info!("motor {} failed", id)`；对 ✅ `info!(motor_id = id, "motor failed")`。

### Feetech 固定诊断事件的中止原因

串口执行线程通过 16 字节 `DiagEvent` 传递数字字段，由非实时 pub 线程输出
`site / kind / a / b / seq / stamp`。这些字段不是跨进程命令 schema；安全决策
不依赖诊断是否送达。`site=5` 表示 adapter 周期 `write_command`，不能当成
Enable 自身的写操作；其他站点仍按产生该事件的函数解释。

Feetech `record_error`按 [diagnostic.rs](../crates/robot-adapters/feetech/src/diagnostic.rs) 穷尽匹配`FeetechError`，再统一编码；新增错误变体必须显式分类，不以Unknown兜底：

| `kind` | `a` | `b / seq` | 含义 |
| --- | --- | --- | --- |
| 0 | `CommErrorKind` 编码 | 0 / 0 | 通信错误，保持既有编码 |
| 5 | 电机 ID | 设备状态码 / 0 | 既有 `StatusError` |
| 7 | 1 | 0 / 0 | `Aborted(Emergency)`：急停中止 |
| 7 | 2 | 0 / 0 | `Aborted(Timeout)`：请求超时中止 |
| 7 | 3 | 0 / 0 | `Aborted(Revoked)`：执行权限拒绝或撤销 |
| 8 | 1 | 0 / 0 | 无效寄存器、电机 ID、长度等参数 |
| 8 | 2 | 0 / 0 | 编码数值越界 |
| 8 | 3 | 0 / 0 | 标定参数无效 |
| 8 | 4 | 0 / 0 | 标定文件读取或解析失败 |
| 8 | 5 | 0 / 0 | 电机型号不匹配 |
| 8 | 6 | 0 / 0 | 当前位追平后的目标读回不一致 |
| 8 | 7 | 0 / 0 | 重复连接 |
| 8 | 3 | 电机ID / 有符号raw位模式 | 校准锚点改变；与普通校准参数错误区分 |
| 9 | `AbsoluteWriteReason`编码 | 写入进度编码 / 0 | 绝对截止写失败；具体编码见`error.rs` |

既有 `kind=6` 扭矩使能失败附加摘要不变，仍可能保留原 Unknown；其后完整错误
记录使用上述分类。所有事件保留 WARN 和原有计数，返回的
`RobotError::Aborted` 及安全策略也不变；权限拒绝不等同于串口发送失败。
旧日志的 `kind=0 / a=63` 丢失了原始原因，不能仅凭新编码将历史事件回填成
Revoked，也不能由一个中止事件推断当前物理扭矩或整个请求是否已有先前副作用。

## 级别约定

- `ERROR`：业务级不可恢复错误（adapter handshake failed、config parse error、
  state machine 进 Fault）。**只在操作失败时用**。
- `WARN`：可降级的异常（retry 中、fallback 生效、度量异常但不致命）。
- `INFO`：正常运行里程碑（startup banner、state transition、定期 heartbeat
  摘要、resource release）。**不要**淹在 hot path（50 Hz heartbeat 不要每次
  都 info!）。
- `DEBUG`：开发 / bring-up 时想看的细节（每次 sync_read / sync_write、goal /
  present 值对比）。`RUST_LOG=debug` 打开。
- `TRACE`：极详细（每个 packet byte、每个 register 操作）。`RUST_LOG=trace` 打开。

### raw 混合零目标的设备观测

完整 raw 混合零轮目标写成功后，Feetech 复用当前连接补读三轮 TE、GV、PV；相邻尝试至少20毫秒。每组三个事务全部成功才向原诊断环提交四条 `site=31` 记录：`kind=0` 的 `stamp` 为开始时刻、`a=3`；随后三条 `kind=1` 共用结束时刻，`a` 为配置中的物理ID，`b` 为TE，`seq` 低16位为GV、高16位为PV。原字不做符号归一化。

Pub 使用独立 target `s0_wheel_observation` 的 DEBUG 日志，默认 `info,robot=debug` 不输出；采证时追加 `s0_wheel_observation=debug`。未知kind和原故障仍为WARN。缺失、重复、丢弃或时间超界的组不能作为有效停止证据；日志本身不提交安全状态。额外读取失败沿原周期读错误路径处理，200毫秒上下文不等于整组三读硬超时；真实总线耗时和外层500毫秒窗口仍须实测。

同一 raw 混合实例另从原有周期 PP、PV 两次请求保留无符号原字，成功观测间隔至少50毫秒，不增加事务。完整读取及最后抢占检查通过后提交四条 `site=32` 记录：BEGIN 与 site31 同形；三个 kind1 的 `a` 为配置物理ID、`b=0`，`seq` 低16位为PP、高16位为PV，共用结束时刻。状态仍按原规则解码，未标定SI速度/力矩仍为NaN，不由解码值反推原字。site32复用同一DEBUG target。

受监督发送器只读取启动后追加的JSON记录；按配置ID组装，丢弃计数非零、半组、重复、文件替换或截断均失败。正常断扭确认后最多等待1.2秒只读尾窗，确认最后零速组之后的原摘要 `dropped=0` 且当前文件快照已完整解析，再结束软件证据；该尾窗不延长电机启用预算。

## 过滤（`RUST_LOG`）

`tracing-subscriber::EnvFilter` 标准语法。

**默认值**：`info,robot=debug` —— 项目内（crate 名以 `robot` 前缀）走
`debug`，外部依赖（iceoryx2、tokio、serde、...）走 `info`。权威实现在
`crates/robot-logging/src/lib.rs::init`（`EnvFilter::try_from_env(...)
.unwrap_or_else(|_| EnvFilter::new("info,robot=debug"))`）。修改默认值
必须同时改：① `crates/robot-logging/src/lib.rs::init`、② 本文件、③
`docs/00_design.md` §8 第 4 条，三处口径必须一致。

```bash
# 不传 RUST_LOG = 用默认 "info,robot=debug"
RUST_LOG=info                         # 全局 info（覆盖默认，连项目 crate 也降到 info）
RUST_LOG=debug                        # 全 debug（连外部依赖也升 debug）
RUST_LOG=info,robot=debug             # 显式写出默认值
RUST_LOG=robot_s0=trace,info          # 仅 s0 走 trace
RUST_LOG=robot_adapter_feetech=debug  # 仅 feetech adapter 走 debug
```

## bin 接入步骤

1. `Cargo.toml` 加 deps：

   ```toml
   [dependencies]
   robot-logging = { workspace = true }
   tracing       = { workspace = true }
   ```

2. 在 bin 入口调用 `let _ = robot_logging::init();`，确保二进制链接日志初始化实现。
3. 在非实时路径写结构化事件：

   ```rust
   tracing::info!(motor_id = 6, adapter = "feetech_sts3215", "built adapter");
   tracing::warn!(?err, attempt = 2, "sync_read retry");
   tracing::error!(?err, "connect failed");
   ```

## lib 接入步骤

只加 `tracing = { workspace = true }`，**不要**依赖 `robot-logging`（那是 bin
专属）。

## 测试

`robot-logging`自身单元测试构建由`#[cfg(not(test))]`关闭ctor；作为普通依赖被集成测试链接时，不能据此假定ctor关闭。需要捕获事件时复用已有`tracing::subscriber::with_default`局部subscriber，不为日志断言新增依赖。

## RT 铁律

RT线程禁止`tracing::*!`、格式化、`eprintln!`及文件/网络IO，不以字面量消息或日志级别关闭推定安全。现有 [SerialDiagRing](../crates/robot-s0/src/diag.rs)容量256，传递16字节`DiagEvent`；满或同侧竞争时丢弃新事件并计数，不等待消费者。非实时 [pub线程](../crates/robot-s0/src/pub_thread.rs)有界消费并输出，安全判断不依赖日志送达。

## 参考

- `tracing` docs: <https://docs.rs/tracing/>
- `tracing-subscriber::EnvFilter`: <https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html>
- `docs/CHANGELOG.md` 2026-04-25 "task-32 日志骨架"：workspace 依赖白名单登记
