# Handoffs（session 续接快照）

`docs/handoffs/<YYYY-MM-DD>.md` 每一份 = 一次 session 结束时的进度 + 决定快照，供下一次 session（尤其是异地续开、换机器、隔几天回来）**5–10 分钟读完就能 resume**。

## 这是什么 / 不是什么

| 这是 | 这不是 |
|---|---|
| 本次 session 做了什么 + 下一步做什么 | 设计 spec（去 `00_design.md`）|
| 关键决定的**摘要 + 指针**（详情去 CHANGELOG） | 决定的权威记录（去 `CHANGELOG.md`）|
| 当前状态（在 v0.x 哪一步 / 什么卡住） | 指标 baseline / 测试状态（去 `10_verification.md`）|
| 异地 resume 的 bootstrap 指南 | 长期维护的 onboarding（去 `02_start_here.md`）|

## 改动规则

- **非 spec，自由更新 / 追加**，不走 `CHANGELOG.md` 登记。
- 每次 session 结束追加一份 `<YYYY-MM-DD>.md`。同日多次 session 可加后缀 `<YYYY-MM-DD>_<n>.md`。
- 过期的 handoff **保留不删**（作为演进历史）；但不是 onboarding 的阅读材料，新人读 `02_start_here.md` 即可。

## 怎么用（新 session 开场）

```
# 拉代码（服务器或别的机器）
cd ~/repo/rust/robot-system && git pull

# 按时间降序读最新 1-2 份 handoff
ls -t docs/handoffs/*.md | head -2 | xargs -I {} less {}

# 然后按 handoff 末尾 "开工前必读" 补齐语境
```

## 列表

| 日期 | 主题 | 文件 |
|---|---|---|
| 2026-04-24 | Target 文档三分重构 + RT 线程拓扑方案 B + policy_host RT 内核判断 + iceoryx2 RT baseline 提前到 Step 6 | [2026-04-24.md](2026-04-24.md) |
