爸爸，四份 R8 普通 READ 原件独立本地核验通过。每条 trace 均32行/10036B，实际串口 fd 3 为 /dev/ttyACM1 (166:1)，四个 PV 响应 error=0、payload=00 00、普通 stdout=0。

|序号|电机|外层 strace PID|trace /proc/self PID|TX (PV READ)|RX|
|---|---|---:|---:|---|---|
|136|右8|624896|624899|`ff ff 08 04 02 3a 02 b5`|`ff ff 08 04 00 00 00 f3`|
|142|右9|624905|624908|`ff ff 09 04 02 3a 02 b4`|`ff ff 09 04 00 00 00 f2`|
|337|右8|625109|625112|`ff ff 08 04 02 3a 02 b5`|`ff ff 08 04 00 00 00 f3`|
|343|右9|625118|625121|`ff ff 09 04 02 3a 02 b4`|`ff ff 09 04 00 00 00 f2`|

每条均有完整 PING、地址3/2B READ、地址58/2B READ，所有12 TX及12 RX的长度、ID、字节数和校验和匹配；串口成功关闭，trace进程exit0。三对TX/RX及完整SHA见review.json，原run/trace/stdout/stderr核验前后SHA一致。

仅四个右8/右9速度 READ 响应原帧；不等同整机51停止字段原帧。

每条普通read还含PING和地址3的2B读取；串口TX写入协议READ请求，不含电机参数写指令或Enable。

PID由被追踪进程的/proc/self/maps解析获得；trace不含exec、wait或-f，不把nested PID计入driver直属child，不宣称其wait/reap直接证据。

外层strace直属child的exit0/reaped/signals[]为原run回执记录，父任务另核完整363child；这里不核全部owner路径。

四次PV原帧为零，不能解释或覆盖旧±50停止失败，不能据此判断历史轮子实际运动。

地址3响应为09 03即raw777；此处只核字节，不外推现场固件版本或速度单位。

不改变原run中的PENDING_ROOT_REVIEW_OF_COMPLETE_STRACE字段或其整体结果。
