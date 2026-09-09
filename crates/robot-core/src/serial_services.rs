//! `SerialRT` 构造期服务与通信错误协议；不依赖执行器或异步运行时。

use crate::time::DurationNs;

/// 通信错误编码。未知值统一归为不可恢复信任的 `Unknown`。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CommErrorKind {
    Timeout = 0,
    Disconnected = 1,
    IoRead = 2,
    IoWrite = 3,
    OpenFailed = 4,
    FrameCorrupt = 5,
    ResponseIdMismatch = 6,
    PeerNoResponse = 7,
    DiscardFailed = 8,
    PendingDeadline = 9,
    Unknown = 0x3f,
}

impl CommErrorKind {
    #[must_use]
    pub const fn from_u8(raw: u8) -> Self {
        match raw {
            0 => Self::Timeout,
            1 => Self::Disconnected,
            2 => Self::IoRead,
            3 => Self::IoWrite,
            4 => Self::OpenFailed,
            5 => Self::FrameCorrupt,
            6 => Self::ResponseIdMismatch,
            7 => Self::PeerNoResponse,
            8 => Self::DiscardFailed,
            9 => Self::PendingDeadline,
            _ => Self::Unknown,
        }
    }

    #[must_use]
    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    /// 本地请求超时不构成接收失步；打开失败只发生在启动构造期。
    #[must_use]
    pub const fn clears_trust(self) -> bool {
        !matches!(self, Self::PendingDeadline | Self::OpenFailed)
    }
}

/// 仅发生于正常事务边界或可抢占等待的中止原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AbortReason {
    Emergency,
    Timeout,
}

/// 区分已有执行序号和尚未提交的请求，绝不以零序号冒充请求。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NormalAbortTarget {
    Submitted { seq: u64 },
    Unsubmitted,
}

pub const SEQ_BITS: u32 = 40;
pub const SEQ_MASK: u64 = (1_u64 << SEQ_BITS) - 1;
pub const ABORT_NONE: u64 = 0;
pub const ABORT_UNSUBMITTED: u64 = 1_u64 << 63;

/// 非法编码与空槽分开，消费端才能对损坏请求采取保守故障处置。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidAbortRequest(pub u64);

impl NormalAbortTarget {
    /// 不可编码的已提交序号返回 None，不能截断为其它请求。
    #[must_use]
    pub const fn encode(self) -> Option<u64> {
        match self {
            Self::Submitted { seq } if seq > 0 && seq <= SEQ_MASK => Some(seq),
            Self::Submitted { .. } => None,
            Self::Unsubmitted => Some(ABORT_UNSUBMITTED),
        }
    }

    /// 先匹配完整 tag，再检查序号范围；禁止先 mask 丢弃非法位。
    pub const fn decode(raw: u64) -> Result<Option<Self>, InvalidAbortRequest> {
        match raw {
            ABORT_NONE => Ok(None),
            ABORT_UNSUBMITTED => Ok(Some(Self::Unsubmitted)),
            1..=SEQ_MASK => Ok(Some(Self::Submitted { seq: raw })),
            _ => Err(InvalidAbortRequest(raw)),
        }
    }
}

/// 同一 bus 的事务操作域。Safety、Confirm、启动 Connect 不受正常请求抢占。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum BusOpClass {
    NormalEffectful,
    PeriodicRead,
    PeriodicWrite,
    Safety,
    Confirm,
    Connect,
}

impl BusOpClass {
    #[must_use]
    pub const fn is_normal(self) -> bool {
        matches!(
            self,
            Self::NormalEffectful | Self::PeriodicRead | Self::PeriodicWrite
        )
    }
}

/// 在构造时共享给 adapter 的服务；事务执行权与进度检查点不能互相替代。
pub trait PreemptProbe: Send + Sync {
    fn should_abort(&self, class: BusOpClass) -> Option<AbortReason>;
    fn begin_transaction(&self, class: BusOpClass) -> Result<(), AbortReason>;
    fn finish_transaction(&self, class: BusOpClass);
    fn checkpoint(&self);
    fn sleep_or_abort(&self, class: BusOpClass, dur: DurationNs) -> Result<(), AbortReason>;
    fn operation_class(&self) -> BusOpClass;
    fn confirmation_trusted(&self) -> bool;
    /// 安全事实独立于可丢弃诊断环，Confirm 成功也不能覆盖先前 hygiene 错误。
    fn report_comm_error(&self, kind: CommErrorKind);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_clearing_classification_is_exhaustive() {
        for raw in 0_u8..=u8::MAX {
            let kind = CommErrorKind::from_u8(raw);
            assert_eq!(kind.clears_trust(), !matches!(raw, 4 | 9), "raw={raw}");
            assert_eq!(CommErrorKind::from_u8(kind.to_u8()), kind);
        }
    }

    #[test]
    fn abort_tag_decoding_never_masks_invalid_bits() {
        assert_eq!(NormalAbortTarget::decode(0), Ok(None));
        assert_eq!(
            NormalAbortTarget::decode(ABORT_UNSUBMITTED),
            Ok(Some(NormalAbortTarget::Unsubmitted))
        );
        for seq in [1, 2, SEQ_MASK] {
            let target = NormalAbortTarget::Submitted { seq };
            assert_eq!(target.encode(), Some(seq));
            assert_eq!(NormalAbortTarget::decode(seq), Ok(Some(target)));
            assert!(NormalAbortTarget::decode(seq | ABORT_UNSUBMITTED).is_err());
        }
        for seq in [0, SEQ_MASK + 1, u64::MAX] {
            assert_eq!(NormalAbortTarget::Submitted { seq }.encode(), None);
        }
        assert!(NormalAbortTarget::decode(SEQ_MASK + 1).is_err());
    }
}

use crate::control_authority::{
    ControlAuthority, NormalIoIntent, NormalIoLease, NormalIoReleased, Refused,
};

/// 总线操作持有的真实权限租约；Drop 不代替显式完成。
///
/// ```compile_fail
/// use robot_core::serial_services::BusLease;
/// fn duplicate(lease: BusLease) { let _ = lease.clone(); }
/// ```
///
/// ```compile_fail
/// use robot_core::serial_services::{BusLease, BusOpClass};
/// let forged = BusLease { class: BusOpClass::Safety, lease: None };
/// ```
#[derive(Debug)]
pub struct BusLease {
    class: BusOpClass,
    lease: Option<NormalIoLease>,
}

impl BusLease {
    #[must_use]
    pub const fn class(&self) -> BusOpClass {
        self.class
    }

    #[must_use]
    pub const fn holds_normal_lease(&self) -> bool {
        self.lease.is_some()
    }

    /// 类别与意图逐项匹配，拒绝时返回仍占用原权限域的唯一租约。
    #[allow(
        clippy::match_like_matches_macro,
        reason = "枚举组合分发保留项目约定的 match 表"
    )]
    pub fn with_normal(class: BusOpClass, lease: NormalIoLease) -> Result<Self, NormalIoLease> {
        let allowed = match (class, lease.intent()) {
            (BusOpClass::PeriodicRead, NormalIoIntent::PeriodicRead)
            | (BusOpClass::PeriodicWrite | BusOpClass::NormalEffectful, NormalIoIntent::Motion)
            | (
                BusOpClass::NormalEffectful,
                NormalIoIntent::EnableStep(_) | NormalIoIntent::NonEnergizingControl(_),
            ) => true,
            _ => false,
        };
        if allowed {
            Ok(Self {
                class,
                lease: Some(lease),
            })
        } else {
            Err(lease)
        }
    }

    #[must_use]
    pub const fn special(class: BusOpClass) -> Option<Self> {
        match class {
            BusOpClass::Safety | BusOpClass::Confirm | BusOpClass::Connect => {
                Some(Self { class, lease: None })
            }
            BusOpClass::NormalEffectful | BusOpClass::PeriodicRead | BusOpClass::PeriodicWrite => {
                None
            }
        }
    }

    /// 错域失败完整返还 wrapper；特殊事务没有普通 IO 占位可释放。
    pub fn release(
        self,
        authority: &ControlAuthority,
    ) -> Result<Option<NormalIoReleased>, Refused<Self>> {
        let Self { class, lease } = self;
        match lease {
            None => Ok(None),
            Some(lease) => authority
                .release_normal_io(lease)
                .map(Some)
                .map_err(|refused| Refused {
                    token: Self {
                        class,
                        lease: Some(refused.token),
                    },
                    reason: refused.reason,
                }),
        }
    }
}

/// 后续总装迁移的接口声明；本包不安装生产实现。
pub trait LeasedPreemptProbe: PreemptProbe {
    fn begin_leased(&self, class: BusOpClass) -> Result<BusLease, AbortReason>;
    fn finish_leased(&self, lease: BusLease);
}
