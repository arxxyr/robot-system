# head1 左7历史回程本机复核

仅分析2026-10-08原件；2026-10-09本机只读取证，无SSH/UART/新动作。

归档 `dac2699bcb191b57199ff95a21d9c85882a40f9ac3b46f57e4429fc7203c2e7a`，1202201 bytes，匹配旁侧索引。只提取3件原件，逐项SHA/bytes核验；run内359条commands与commands.jsonl逐项相同。当前维护脚本与原件逐字一致：`caf87b36f0689b4e40fb8fade0f4577ef0a6ff691732c157a10ed620b9a6d4f4`。

## 结论

- 目标2658写入/读回后，出程6样本2634→2638→2649→2652→2653→2655，311.540ms时误差-3，按原±4门PASS；不宣称精确到2658。
- 回程目标2634已写入并读回，约190.694ms时到2642；随后直到801.681ms共11次均2642。证据支持本次采样窗口内平台+8，不支持“仍一直向目标缓慢移动”，也不能证明更长时间或亚计数运动状态。
- 发现有限的软件收尾缺陷：末次读在deadline之后返回且不合格，代码仍固定sleep50ms才FAIL；仅能修掉已过期后的多余等待，不能将+8或原硬件FAIL变为PASS。
- 原head1断扭写、TE0/PV0读回均成功；全局停止仍因right9 Present_Velocity=-50失败，原NEEDS_MANUAL_POWER_CUT不变。

## 精确时序

以下均为同一Orin boot `6cf7ee3e-a3c5-4c91-80bc-4ca924818b53` 的monotonic_ns；零点为回程witness开始 `3192038953890366`。小数ms用于阅读，完整start/finish ns见head1_facts.json。不是wire传输或电机内部采样的精确时刻。

| command | 操作/回执值 | 启动相对ms | 完成相对ms |
|---|---|---:|---:|
| 204 | read Operating_Mode = 0 | -512.637838 | -504.383893 |
| 205 | read Torque_Enable = 0 | -502.372520 | -494.100783 |
| 206 | read Present_Velocity = 0 | -492.103267 | -483.850250 |
| 207 | read Present_Position = 2634 | -481.863039 | -473.631814 |
| 208 | write Goal_Position = 2634 | -471.666492 | -463.477189 |
| 209 | read Goal_Position = 2634 | -441.291676 | -433.131111 |
| 210 | read Present_Position = 2634 | -381.108722 | -372.967934 |
| 211 | write Torque_Enable = 1 | -371.036212 | -362.809820 |
| 212 | read Torque_Enable = 1 | -360.668747 | -352.438483 |
| 213 | write Goal_Position = 2658 | -350.483305 | -342.276242 |
| 214 | read Goal_Position = 2658 | -340.199043 | -331.892712 |
| 215 | read Present_Position = 2634 | -329.917725 | -321.531615 |
| 216 | read Present_Position = 2638 | -269.459913 | -261.234705 |
| 217 | read Present_Position = 2649 | -209.176507 | -200.957187 |
| 218 | read Present_Position = 2652 | -148.923886 | -140.697654 |
| 219 | read Present_Position = 2653 | -88.660609 | -80.499084 |
| 220 | read Present_Position = 2655 | -28.486360 | -20.335555 |
| 221 | write Goal_Position = 2634 | -18.397529 | -10.179906 |
| 222 | read Goal_Position = 2634 | -8.218711 | -0.071426 |
| 223 | read Present_Position = 2655 | 1.852007 | 10.048701 |
| 224 | read Present_Position = 2653 | 62.072402 | 70.285417 |
| 225 | read Present_Position = 2644 | 122.298910 | 130.455283 |
| 226 | read Present_Position = 2642 | 182.465991 | 190.618012 |
| 227 | read Present_Position = 2642 | 242.637905 | 250.771109 |
| 228 | read Present_Position = 2642 | 302.779545 | 310.917262 |
| 229 | read Present_Position = 2642 | 363.204396 | 371.799346 |
| 230 | read Present_Position = 2642 | 423.909194 | 432.138978 |
| 231 | read Present_Position = 2642 | 484.169495 | 500.398206 |
| 232 | read Present_Position = 2642 | 552.453556 | 560.746862 |
| 233 | read Present_Position = 2642 | 612.770978 | 620.959000 |
| 234 | read Present_Position = 2642 | 672.964044 | 681.175651 |
| 235 | read Present_Position = 2642 | 733.179414 | 741.403694 |
| 236 | read Present_Position = 2642 | 793.429666 | 801.614232 |
| 237 | write Torque_Enable = 0 | 853.645069 | 861.806882 |
| 238 | read Torque_Enable = 0 | 913.933658 | 922.092751 |
| 239 | read Present_Velocity = 0 | 924.026489 | 932.180878 |
| 278 | read Present_Position = 2642 | 1319.484359 | 1327.671581 |
| 280 | read Goal_Position = 2634 | 1339.687781 | 1347.826649 |

所有上述命令exit0、真实reaped、stderr空、无信号/超child预算。归档未保留这些head1独立stdout文件或wire原帧；本次按脚本完整stdout合同重建文本并逐项核其SHA/bytes，均吻合commands记录。不能将重建文本称原stdout。Goal读回证据同时由实际执行跨过require及最终Goal2634后验交叉支持。

## 两段位置采样

| 阶段 | command | 阶段内ms | 位置raw | 误差raw |
|---|---:|---:|---:|---:|
| outward | 215 | 10.354121 | 2634 | -24 |
| outward | 216 | 70.657687 | 2638 | -20 |
| outward | 217 | 130.922692 | 2649 | -9 |
| outward | 218 | 191.183921 | 2652 | -6 |
| outward | 219 | 251.375164 | 2653 | -5 |
| outward | 220 | 311.539621 | 2655 | -3 |
| return | 223 | 10.132769 | 2655 | +21 |
| return | 224 | 70.356588 | 2653 | +19 |
| return | 225 | 130.523702 | 2644 | +10 |
| return | 226 | 190.693919 | 2642 | +8 |
| return | 227 | 250.834727 | 2642 | +8 |
| return | 228 | 310.979760 | 2642 | +8 |
| return | 229 | 371.898358 | 2642 | +8 |
| return | 230 | 432.212293 | 2642 | +8 |
| return | 231 | 500.475617 | 2642 | +8 |
| return | 232 | 560.825393 | 2642 | +8 |
| return | 233 | 621.033371 | 2642 | +8 |
| return | 234 | 681.246598 | 2642 | +8 |
| return | 235 | 741.490673 | 2642 | +8 |
| return | 236 | 801.680571 | 2642 | +8 |

回程原deadline=3192039753890366ns。末read 793.429666→801.614232ms，sample801.680571ms；因此该样本即使误差合格也按原严格门拒绝。随后无条件sleep使FAIL记录落到851.746629ms（比deadline晚51.746629ms）；TE0命令853.645069→861.806882ms，TE0读回922.092751ms，PV0读回932.180878ms。更后的PP2642/Goal2634读回分别1327.671581/1347.826649ms，已经断扭，不能拿来证明仍有扭矩时会继续收敛。

## 等待逻辑核对与最小修复边界

- `scripts/run_hardware_maintenance.py:397–404`：800ms从对应Goal读回之后的witness开始；本次回程计时比Goal写命令完成晚10.179906ms。现有代码如此，不能把此口径擅改为wire写出起点。
- `:358–370`：循环入口只检查开始时未超期；读返回后按stamp<=deadline及±4决定PASS。失败样本无条件再sleep50ms。已超期后无需再等待，窄修应在原PASS判断之后若stamp>=deadline则break。未改IO自然wait/2s预算、速度、PID、原800ms/±4、次数或正常成功路径。
- `:414–415`及`:327–344`：位置尝试finally调用safe_off，先TE0写、再50ms停稳等待、随后TE/PV读回。该安全收尾50ms与过期样本后的多余50ms是两个不同位置；安全收尾不改。
- 旧测试`:377–393`已检查799/800/801ms判定，但只断言失败elapsed>800ms，没有锁住超期后不得再sleep；新增确定性假时钟回归应锁这点。
- 本次不证明电机为何停在+8；负载/外力/死区/固件参数均未知，不从该样本推断具体根因。

## 仍需的最小现场事实（待授权，不执行）

先等root本轮七项现场事实答复；本报告不新增动作许可。对+8定位的最小补充是：左7是否确为该头关节、当时头部是否受人工支撑/接触或机械阻力；保留实际供电与配置身份。若另获只读授权，记录左7固件/模式、位置/速度/负载以及现有位置相关参数的原始读回，不能写PID/Phase/模式/EEPROM或调大容差。历史缺失的负载和参数无法用当前读回反推当时值；只有另行批准的新动作才可补同步运动证据，现阶段不安排自动重试。

## 本机草稿验证（未整合）

root授权后，修复只放`draft/scripts`，生产树未改。`implementation.diff`仅在原PASS判定之后增加到期break；没有缩短正常采样sleep或改变safe_off的停稳sleep。同步IO本身仍沿原自然wait规则，所以不宣称断扭能够绝对在800ms内完成。

`tests-only.diff`新增4个确定性假时钟方法：到期/迟到+8仍FAIL且不再sleep、迟到误差0仍FAIL、正常多次采样及原50ms路径、恰好800ms误差4仍PASS。对原实现实跑4方法，2方法失败（含两个边界子项，共3个断言失败）；对草稿实跑原40+新4=44方法全部通过。原red.log原样保留，green.log与真实Popen/wait/reaped及SHA在test_results.json；本机软件通过不等于新硬件验证。

主树与H2冻结alignment保持原样；由root评审后另选整合时点。本次没有SSH/UART/新硬件动作、修改阈值/IO预算/参数或重试现场。
