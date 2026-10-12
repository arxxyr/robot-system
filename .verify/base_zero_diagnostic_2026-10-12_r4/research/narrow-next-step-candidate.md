爸爸，当前资料支持优先核实 Phase bit3，不能把它当作 R4 已证实原因。

- 当前厂家门户公开 GET 返回 version 201，这是文档响应版本，不是固件版本。Phase bit3=0 定义速度目标0为停止，bit3=1定义速度目标0为最高速度；Phase12=0x0C，bit2、bit3均置1。资料未明确绑定 FW3.10，也未将该说明限定为 Mode0。R4 的 PV0/0/50、断扭后-50不能据此证明最高速、物理转动或因果。
- Lock55 的文档含义：Lock0时配置写入掉电保存，Lock1时掉电不保存。不能把Lock1当作拒绝配置写入，也不能把文档当作FW3.10的临时Phase写即时生效或不持久化认证。Phase18仍有特殊功能勿随意修改的提醒；型号适用性须按具体内存表。
- 官方Linux WriteSpe、Python WriteSpec从41连续写7字节：加速度、42–45清零、46–47速度；WheelMode另写33=1。零速示例未读/设Phase或声明固件前提。XLeRobot固定源码stop_base直接同步写Goal_Velocity=0，与七字节写有差异。
- 下一步候选：先核实厂家对FW3.10/Mode1、bit3与Lock1临时Phase写生效时机的适用说法。若后续明确授权，可考虑仅right9在断扭下临时Phase12→4，保留bit2及其他位，核原帧读回；独立新GO沿原500ms、三次PV整数零和STOP51门验证并按方案恢复。不要同时换SDK七字节写。当前只保存公开资料，无设备操作或厂商联系。

引用：

1. [厂家公开内存表页面](http://doc.feetech.cn/#/prodinfodownload?srcType=FT-SMS-STS-emanual-229f4476422d4059abfb1cb0)；[页面使用的公开GET接口](http://longbos.com:9009/workflow/yf-specification/getBysrcType/FT-SMS-STS-emanual-229f4476422d4059abfb1cb0)。本机原表关键行号：Phase18第38行、Mode33第51行、Goal46第66行、Lock55第69行、bit2第102行、bit3第103行。
2. [官方协议第2、5页](https://www.feetech.cn/Data/feetechrc/upload/file/20240702/%E8%88%B5%E6%9C%BA%E5%8D%8F%E8%AE%AE%E6%89%8B%E5%86%8C-%E7%A3%81%E7%BC%96%E7%A0%81%E7%89%88%E6%9C%AC.pdf)。
3. [官方Linux实现](https://raw.githubusercontent.com/ftservo/FTServo_Linux/main/src/SMS_STS.cpp)、[零速示例](https://raw.githubusercontent.com/ftservo/FTServo_Linux/main/examples/SMS_STS/WriteSpe/WriteSpe.cpp)、[官方Python实现](https://raw.githubusercontent.com/ftservo/FTServo_Python/main/scservo_sdk/sms_sts.py)。
4. [XLeRobot固定源码](https://github.com/Vector-Wangel/XLeRobot/blob/b017b5e6354bd9f61f4247a920c72622ca0aade0/software/src/robots/xlerobot/xlerobot.py)。
