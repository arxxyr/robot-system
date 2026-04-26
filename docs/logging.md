# 日志规范

> robot-system 所有 bin / lib 的日志统一到 **`tracing`**（Rust 生态 de-facto
> 标准，相当于 C++ spdlog）。本文档是字段命名 + 使用约定的权威。

## TL;DR

1. **bin 不用写 init 代码**：只要 `Cargo.toml` 依赖 `robot-logging`，
   `#[ctor::ctor]` 会在 `main` 运行前自动装上 tracing subscriber。
2. **lib / bin 打日志**：`tracing::info!(motor_id = 6, "handshake ok")` —— 用
   结构化字段（`k = v`），不要 `format!` 拼字符串。
3. **错误打印**：`tracing::error!(?err, "feetech connect failed")` —— 保留
   `thiserror` 的结构。
4. **环境变量控制**：`RUST_LOG=debug ./s0_daemon` / `ROBOT_LOG_FORMAT=json ./s0_daemon`。
5. **RT thread 内禁用** 带 format arg 的 `tracing::*!`（heap alloc，违反 §4.9）。

## 格式

### Human 模式（默认）

```
2026-04-25T23:45:12.123456Z [+12345.678901234s]  INFO  robot_s0::s0_daemon: feetech factory built adapter motor_id=6 address="/dev/..."
```

- **wall clock RFC3339 UTC** + **monotonic `[+秒.纳秒s]`**（自进程启动）
  - wall clock 方便跨机器对齐绝对时间（NTP 下）
  - monotonic 方便后期日志回放算事件时延（NTP 跳 / 闰秒无感）
- 级别 5 字符对齐：`INFO ` / `WARN ` / `ERROR` / `DEBUG` / `TRACE`
- ANSI 彩色 on（TTY 自动检测，非 TTY 关；`NO_COLOR=1` 强制关）
- `target` = 模块路径（`robot_s0::s0_daemon`），便于定位
- 结构化字段 `k=v` 紧随 message

### JSON 模式

`ROBOT_LOG_FORMAT=json` 切换，每行一个 JSON 对象，方便 journald / Loki / Elastic
采集：

```json
{"timestamp":"2026-04-25T23:45:12.123456Z [+12345.678901234s]","level":"INFO","target":"robot_s0::s0_daemon","fields":{"message":"feetech factory built adapter","motor_id":6,"address":"/dev/..."}}
```

## 字段命名约定（词汇表）

**所有 `tracing::*!` 调用共用这张表**。新增字段必须先更新本表 + 登记 CHANGELOG。

| Key            | 类型             | 示例                             | 含义                                                                 |
| -------------- | ---------------- | -------------------------------- | -------------------------------------------------------------------- |
| `robot_id`     | `&str`           | `"xlerobot_task29"`              | TOML `[robot] id`                                                    |
| `hardware_id`  | `&str`           | `"left_arm_single"`              | TOML `[[hardware]] id`                                               |
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
| `interested`   | `%` (Display)    | `"123:s0_daemon@1.5%[20M]"`      | `MonitorSection::interested_processes` 匹配到的进程（`pid:comm@cpu%[rssM]`，`-` = 空） |
| `top_by_cpu`   | `%` (Display)    | `"1:a@111%[38G],2:b@106%[32G]"`  | 全系统按 CPU 占用 top N，`,` 分隔，格式同上                          |

### 字段使用原则

- **名词单数**：`motor_id` 不是 `motors` / `motor_ids`。多值走 `motor_ids` 明确复数。
- **布尔字段命名**：`enabled: bool` / `connected: bool`。不要 `is_*` 前缀（冗余）。
- **单位加后缀**：`duration_ms` / `latency_us` / `bytes` / `rate_hz`。
- **Debug 值用 `?field`**：`error!(?err, ?state, "...")`。
- **Display 值用 `%field`**：`info!(%robot_id, "...")`（避免 Debug 多一层引号）。
- **不要拼 message 字符串**：错 ❌ `info!("motor {} failed", id)`；对 ✅ `info!(motor_id = id, "motor failed")`。

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

2. 完成。`#[ctor::ctor]` 会在 main 前自动 install subscriber。
3. 代码里直接用：

   ```rust
   tracing::info!(motor_id = 6, adapter = "feetech_sts3215", "built adapter");
   tracing::warn!(?err, attempt = 2, "sync_read retry");
   tracing::error!(?err, "connect failed");
   ```

可选：main 里显式 `let _ = robot_logging::init();` 作为 self-documenting anchor
（幂等，重复调 safe）。

## lib 接入步骤

只加 `tracing = { workspace = true }`，**不要**依赖 `robot-logging`（那是 bin
专属）。

## 测试

测试进程**不走 ctor 自动 init**（`#[cfg(not(test))]`），避免 subscriber 污染
测试输出。需要在测试里看 log 的用 `tracing-test` crate（本项目暂未引入，有
需求再加）。

## RT 铁律

| 调用                                           | RT thread 允许？                                    |
| ---------------------------------------------- | --------------------------------------------------- |
| `tracing::info!(x = 1, "literal")`             | ❌（虽然 literal message 不 alloc，但 tracing 内部 Event 构造可能分配字段） |
| `tracing::info!("literal only, no fields")`    | ❌（同上，保守禁用）                                |
| `tracing::info!("x = {}", x)`                  | ❌❌（format arg 构造 String，必 alloc）            |
| `eprintln!("...")`                             | ❌（stderr IO + 内部缓冲可能 alloc）                |

**RT thread 内不 log**。调试期需要日志走 metrics（atomic counter + 定期
non-RT 线程采样）或 lock-free ring buffer（v0.3+）。

## 参考

- `tracing` docs: <https://docs.rs/tracing/>
- `tracing-subscriber::EnvFilter`: <https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html>
- `docs/CHANGELOG.md` 2026-04-25 "task-32 日志骨架"：workspace 依赖白名单登记
