# 对外通讯与控制方案

本文记录已确定的通讯与控制方案：Web服务部署在Orin，浏览器使用WebSocket，内部复用现有gRPC，Nginx只作外网入口，访问管理由用户已有的外部系统承担。当前仅完成HTTP/JSON＋SSE只读软件基线；WebSocket、去掉Web令牌入口及动作控制尚未开发。本次只更新文档和TODO，不修改代码、配置或S0 0.0.1，不部署、不操作硬件。实施顺序与验证结果只维护在[当前状态的TODO](s0_hal_status.md#通讯与控制todo)。

## 通讯链路

下图为目标链路，不能作为已经部署或支持WebSocket的证明。

```mermaid
flowchart TD
    UI[浏览器] <-->|HTTPS页面 / WSS交互| NG[Nginx 外网入口]
    subgraph ORIN[机器人 Orin]
        WEB[Web服务：复用现有robot-rpc入口]
        RPC[现有bus1 / bus2 gRPC网关]
        S1[S1 coordinator与scripted]
        S0[S0 HAL]
        AD[现有硬件适配器]
        DEV[电机总线]
        WEB <-->|同机gRPC| RPC
        RPC -->|位置任务| S1
        RPC -->|使能和断扭RR，急停事件| S0
        S1 -->|iceoryx2 / FlatBuffers命令和心跳| S0
        S0 -->|关节状态| S1
        S0 -->|状态回传| RPC
        S1 -->|任务事件| RPC
        S0 --> AD
        AD -->|现有串口协议| DEV
    end
    NG <-->|HTTP / WebSocket转发| WEB
```

- Web页面与后端都部署在Orin，沿现有`robot-rpc`扩展；HTTP加载页面，WebSocket承载状态、控制请求、回执及任务事件。先迁移只读状态，再按TODO接控制，不同时维护SSE与WebSocket两套长期推送实现，不增加crate或另一套网关框架。
- Web后端调用同机现有gRPC；外部程序或CLI继续沿既有gRPC使用方式。浏览器只提交目标与时长，不实现运动插值、连续电机控制帧或串口读写。WebSocket转换复用既有业务语义，不重新定义动作、任务和停止协议。
- 机器人本机继续使用iceoryx2和FlatBuffers。状态查询、使能、断扭、急停不经过S1；位置任务由S1执行，再交S0。每条总线只有一个S0串口所有者、命令流和停止域，Web和网关不进入RT线程，也不成为S0启动依赖。
- Nginx只负责外网HTTPS/WSS入口及转发，不运行机器人业务、保存任务或执行动作。内部gRPC保持本机监听，不另开公网控制端口；当前Web也限定loopback，跨主机可用既有网络上的SSH转发，实际路径与Nginx网络模式在部署前核对。
- **Web接入管理交给用户已有的外部系统。** 项目页面不再要求输入访问token，不另建登录、令牌签发、账号或接入权限系统；后续迁移时去掉现有Web的令牌表单与Bearer访问校验。本次未删除这些代码。内部既有RPC调用约定与机器人控制校验不因文档更新而改变，浏览器不承担内部RPC凭据配置。
- 每个动作沿既有`CommandMeta`、请求标识、权限、截止时间和幂等规则。超时先查询原任务；结果未知不自动重发。截止时间由机器人侧时钟换算，不使用浏览器时钟代替`RobotClock`。
- 浏览器断连不等于电机已停。首版仅有限时长的单次定位，不做连续拖动或长按点动；本机S1/S0按既有心跳和命令超时合同处理。重连查询真实状态，不续发旧目标。

## 已有对外接口

定义见[robot_control.proto](../crates/robot-rpc/proto/robot_control.proto)，CLI已有[调用与payload构造](../crates/robot-cli/src/dispatch.rs)。下表表示代码能力，不表示已接入当前全总线部署。

| 能力 | 已有接口 | 当前边界 |
|---|---|---|
| 状态查询与订阅 | `GetState`、`StreamState` | 按hardware返回位置、模式、控制链路、输出模式、安全状态、故障码及时间戳；无逐电机`Torque_Enable`字段 |
| 使能与断扭 | `SetEnabled(true/false)` | 单S0所选hardware范围；当前bus1为左臂＋头部，bus2为右臂＋底盘，不是单电机开关 |
| 位置任务 | `InvokeTask(kind=Scripted)` | 完整关节位置向量，单位rad，指定插值时长；尚无按关节名修改单个目标的请求语义 |
| 任务查询与事件 | `GetTaskStatus`、`StreamTaskEvents` | 可查询原任务和接收终态；任务结束不能直接当作实测到位 |
| 取消任务 | `CancelTask` | 合作式取消；当前可能返回启动锚点保持，不能标为“停止当前位置” |
| 急停与解除 | `EStop`、`ClearEStop` | 急停Ack只证明事件发布；解除急停为独立maintainer操作，不自动使能 |

设备列表、总线身份、关节名称、物理ID和模式来自[现有TOML](../configs/README.md)，不另存一套映射。现有RPC尚无设备枚举接口，Web入口已将必要只读元数据与状态一起返回；后续控制限位仍须沿原校准和配置校验。

## 单电机位置控制还缺什么

最小请求表达“所选总线/hardware、关节名称、目标位置rad、运动时长”。界面可显示角度，但传输单位明确为rad；物理ID只用于展示与配置映射，不能用跨总线重复的裸ID定位设备。这是待补请求语义，当前没有可直接调用的单关节API。

1. **在S1接受任务时补齐目标。** 沿现有单任务串行入口，取得一份新鲜、完整且layout匹配的关节快照，以它构造起点和保持目标，只修改选中位置关节，再复用[scripted](../crates/robot-s1/src/action_backend/scripted.rs)。不能由浏览器或网关用旧状态拼完整目标数组；现有全向量请求与S1提交快照之间的时差尚未解决。只补必要的类型化请求与转换，不新建运动后端；具体wire变更实施前核对兼容性并登记，不在本文假定已有新RPC或schema。
2. **限制目标和执行范围。** 校验关节模式、配置/校准限位、有限数值、时长及所需运动速率；明确缺失的参数后才能开放动作，不任意放宽边界。现有raw校准边界容差不直接当作新的运动范围或到位容差。首版只开放双臂、夹爪和头部的位置关节，三轮保持raw0；不提供轮位置控制。每条总线一次一个任务，忙时拒绝，不增加队列；首次实机先左侧再右侧，不做双总线同时动作。
3. **补齐实测到位确认。** 现有Scripted成功只保证末目标已发布，见[实现](../crates/robot-s1/src/action_backend/scripted.rs)。将“请求接收、轨迹发送结束、实测到位”分开；使用后续新鲜反馈、明确的位置误差与稳定窗口确认到位，超时或反馈缺失报告未确认。其余位置关节保持目标和三轮零速也要纳入相应验证，不能只检查选中关节。
4. **明确取消与停止收尾。** 当前取消及失败路径会把Idle/Degraded保持目标重置为启动锚点，见[coordinator](../crates/robot-s1/src/coordinator.rs)。首次位置动作前必须明确处置并离线验证该回拉行为；首版操作明确提供停用/断扭和急停，不用`CancelTask`冒充停止，也不由Web静默改写原语义。若需“取消后保持当前位置”，先登记契约与验收影响，再改实现。停用后复用原停止读回，迟到的任务/使能回执不得重新上扭。

单关节移动不等于仅该电机使能。第一版沿总线使能，明确显示受影响关节；逐电机独立使能及扭矩读回不纳入此轮。

## 最小管理页与状态语义

当前页面只有两总线状态卡片、由配置生成的关节表、反馈位置和保护状态。后续控制仍在同页最小补齐目标位置、运动时长、执行与任务结果，以及使能、停用/断扭、急停；解除急停单独处理。业务拒绝显示实际原因，不自动清故障或启用电机。

| 显示 | 判断依据 |
|---|---|
| 服务在线、软件使能、扭矩状态 | 分开呈现；服务在线不证明上扭，`Safe`不证明已经断扭 |
| 已请求停止 / 已确认断扭 / 停止失败 / 未确认 | 沿`StopCommanded`、`TorqueOffConfirmed`、`StopFailed`、`TorqueUnknown/AdapterUncertain`，不把总线结果复制为每个电机的独立确认 |
| 当前数据 / 陈旧 / 无反馈 | 用机器人侧源时间戳及是否推进判断；`StreamState`重复发缓存不算新采样，浏览器接收时间不证明反馈新鲜 |
| 任务结束 / 已到位 | 前者来自任务事件，后者必须有实测误差和稳定窗口结果 |

当前Dirty保护待机不能当作S1可用的健康关节反馈。状态页可以显示未知和拒绝原因；位置动作必须等现有恢复流程、有效反馈和控制前提通过。停止确认、解除锁存及现场条件均沿既有入口，不另造旁路。

原[gateway](../scripts/systemd/robot-gateway-left.service)和[S1](../scripts/systemd/robot-s1-left.service)模板仍引用arm-only服务与身份，不能直接启动用于bus1/bus2。状态链路已有下述全总线网关模板；S1身份与依赖留到TODO第2步对齐。S0正式发布及D15边界保持不变。

## 当前只读软件基线（HTTP/SSE）

本节描述尚未迁移的现有实现，不是最终WebSocket和外部接入管理方案。在现有`robot-rpc`中启用`status-web`构建`robot-status`，不改变`robot-gateway`默认构建，也不加入S0 0.0.1包。静态页直接内嵌，无前端构建工具。使用独立产物目录：

```bash
CARGO_TARGET_DIR="$PWD/target/verify-status" cargo build -p robot-rpc --features status-web --bin robot-status --bin robot-gateway
target/verify-status/debug/robot-status --config configs/xlerobot-status.toml
```

程序只读取配置和本机gRPC。当前`http://127.0.0.1:8080`仍显示令牌输入框，API仍检查`read_state`；这些是待移除的Web接入实现，不是未来部署的令牌要求。文档更新没有让现有程序自动免令牌。远程查看基线可仅转发HTTP端口，例如`ssh -L 8080:127.0.0.1:8080 robot-mesh`；页面后端和两个网关必须运行在同一主机，不能将gRPC经远端转发后比较不同时钟域。

页面只有`GET /`、`GET /api/state`、`GET /api/events`；后两者校验`read_state`，没有动作/配置写入路由。固定轮询任务按配置调用`GetState`，浏览器数量不增加RPC读取，SSE只保留最新快照并限制并发连接。断开和退出回收任务；无首帧、连接失败、源时间戳重复/回退、布局或身份错配分别保留真实状态，不用收包时间续鲜。

`xlerobot-status.toml`引用原总线TOML，以配置所在目录解析相对路径，身份、8/9槽位、关节名与ID沿原文件派生。默认500ms轮询、2000ms陈旧阈值只影响页面展示，不修改S0/S1的控制、超时和停止合同。轮子标为原始速度模式，不显示位置角度；尚无每电机扭矩读回。

新的[bus1网关模板](../scripts/systemd/robot-gateway-bus1.service)、[bus2网关模板](../scripts/systemd/robot-gateway-bus2.service)和[状态页模板](../scripts/systemd/robot-status.service)只对齐配置与启动顺序，不自动启动S0/S1；本轮不安装、不enable或start。原arm-only模板继续供已有兼容入口使用，不改动它们。
