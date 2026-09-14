# vendor iceoryx2 / iceoryx2-cal 0.9.3 来源说明

> 两个上游运行时 crate 的本地副本，供根 `Cargo.toml` 的 `[patch.crates-io]` 接入。
> 版本不变（仍 0.9.3），只把 source 从 crates.io 换成本地 path；目的是让后续
> RR 严格修正（R1 CAL / R2 IOX）落在单一补丁图上。其余 `iceoryx2-bb-*` /
> `iceoryx2-pal-*` / `iceoryx2-log*` 仍从 crates.io 解析，不升级、不 vendor。

## 原件身份

| 目录 | 原始发布包 | SHA256（.crate，等于 vendor 前根 Cargo.lock checksum） |
|---|---|---|
| `3rd-party/iceoryx2/` | `iceoryx2-0.9.3.crate` | `e226c6041866b416e8c384d69a54a8578cb26619725253351e3e17ea5f32627f` |
| `3rd-party/iceoryx2-cal/` | `iceoryx2-cal-0.9.3.crate` | `bd00991d2e43e9315f555f8ec07bda7fd1a67da3beac9a504b5edaf7de25502f` |

- 来源：本机 cargo registry 缓存 `~/.cargo/registry/cache/index.crates.io-1949cf8c6b5b557f/`
  中的原始 `.crate`（与 `https://static.crates.io/crates/<name>/<name>-0.9.3.crate` 同一文件）。
- 上游 commit：两包 `.cargo_vcs_info.json` 均指 `eclipse-iceoryx/iceoryx2`
  `6ef7c55176a8487ae7ca53c2e0649d5df9a1b3b4`，`path_in_vcs` 分别 `iceoryx2` / `iceoryx2-cal`。
- 提取方式：直接从 tar 流写出全部成员（iceoryx2 93 件、iceoryx2-cal 75 件），逐件
  SHA256 与原包一致；成员全部为普通文件、无链接、无 `..`。不复制 registry 解包目录，
  因此不含本机解包标记 `.cargo-ok`。
- 保留原件：已规范化的 `Cargo.toml`（crates.io 发布形态，依赖为 registry 版本要求）、
  `Cargo.toml.orig`（上游 workspace 继承形态，仅作历史，不用于构建）、`.cargo_vcs_info.json`、
  `BUILD.bazel`、上游 `Cargo.lock`（嵌套锁，根构建不读取）、`README.md`（仅 iceoryx2）。
- 许可证：两包 `license = "MIT OR Apache-2.0"`，逐源文件带 SPDX 头。

## 原包缺失、本仓库补入的文件

原始 `.crate` **不含** `LICENSE-APACHE` / `LICENSE-MIT` / `NOTICE.md`，也不含
`.cargo-checksum.json`。本仓库只补入前三者，取自同一上游 commit
`6ef7c55176a8487ae7ca53c2e0649d5df9a1b3b4` 的仓库根目录（两个 vendor 目录各一份）：

| 文件 | SHA256 | 字节 |
|---|---|---|
| `LICENSE-APACHE` | `62c7a1e35f56406896d7aa7ca52d0cc0d272ac022b5d2796e7d6905db8a3636a` | 9723 |
| `LICENSE-MIT` | `23f18e03dc49df91622fe2a76176497404e46ced8a715d9d2b67a7446571cca3` | 1023 |
| `NOTICE.md` | `4001e4a44dc66b4092318f2cc7d464df72de96e3fa2ac43ac65665e5e67b601c` | 1718 |

`.cargo-checksum.json`：原包不含，本仓库也未生成；原件校验输入以上表 `.crate` SHA256
为准。

## 接入方式与边界

- 根 `Cargo.toml`：`[patch.crates-io]` 仅这两条；`[workspace] exclude` 显式列出两个目录
  （以及预留的 `tools/rr-memory-model`），本方 14 个 members 不变。两包不是 workspace
  成员：`--workspace` / `--all` 门只选本方 crate，vendor 作为库依赖编译。
- 根 `Cargo.lock` 由 Cargo 自动重解析生成，对应两条 `[[package]]` 去掉 `source` / `checksum`，
  其余依赖版本不变。
- 上游 `tests/tests.rs`（两包各一）显式注册为 `[[test]]`，但只 `extern crate`
  未发布、未声明的 `iceoryx2_tests_common` / `iceoryx2_cal_tests_common`，**无法编译运行**。
  原件保留不删；任何“跑过完整上游集成测试”的说法都不成立。
- 实测（2026-09-14，cargo 1.100.0-nightly 3c0b53475）：从根执行
  `cargo test --locked -p iceoryx2 --lib` 退出码 101，
  `error: package `iceoryx2` cannot be tested because it requires dev-dependencies and is not a member of the workspace`；
  `cargo test --locked -p iceoryx2-cal --lib --features std` 退出码 101，
  `error: cannot specify features for packages outside of workspace`。库内既有
  `#[cfg(test)]` 单测依赖 dev-dependency `iceoryx2-bb-testing`（不在根锁中）。后续
  vendor 私有门的运行方式另定。
- 实测同日：两包作为 path 依赖编译时 rustc 不再 `--cap-lints allow`，上游
  `src/port/client.rs:611` 的 `Atomic::<u64>::fetch_update` 在当前 nightly 触发
  `deprecated` 警告（registry 形态下被 cap 隐藏），`CARGO_BUILD_WARNINGS=deny` 的
  test/build 门因此失败。

## 与原包的唯一差异（R0 nightly 兼容补正）

root 于 2026-09-14 批准的唯一源码改动，不静音警告、不改 Ordering / 闭包 / expect / 取模：

| 文件 | 行 | 改动 | 原件 SHA256 → 现 SHA256 |
|---|---|---|---|
| `3rd-party/iceoryx2/src/port/client.rs` | 611 | `Client::next_request_id` 内 `.fetch_update(` → `.try_update(` | `800696371d9f42eb1044a241eca0ae1ace4f64e9574ec967c1e793c05d2fb301` → `1bdfbe6b73452a3e42e55574c37217ddf3be694aea3f1f11ee72a03f0c6303c0` |

依据：nightly 文档中 `fetch_update` 是 `try_update` 的别名（后者 1.95 起可用，前者
1.99 起弃用）；本项目为 nightly 硬要求，不受上游 `rust-version = "1.85"` 约束。
除此一处，两目录其余全部成员与原包逐件一致（iceoryx2-cal 无改动）。

## 后续变更约定

之后对 vendor 的任何修改（R1 / R2）须以本说明中的 `.crate` SHA256 为原件基线、
以上表为 R0 已知差异，另附 diff 与理由；不回写“原件身份”一节。
