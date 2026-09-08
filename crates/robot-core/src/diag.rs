//! `SerialRT` 固定大小诊断记录；安全决策不得依赖诊断记录是否送达。

/// 一条 16 字节记录，无分配、无字符串格式化。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct DiagEvent {
    pub stamp_ns: u64,
    pub site: u8,
    pub kind: u8,
    pub a: u8,
    pub b: u8,
    pub seq_lo: u32,
}

const _: () = assert!(std::mem::size_of::<DiagEvent>() == 16);

/// 构造期注入的固定大小诊断入口。
pub trait DiagSink: Send + Sync {
    fn record(&self, event: DiagEvent);
}
