//! 发布样本的底层来源身份；与应用 `FrameHeader.source_id` 分开。
//!
//! 标识保留 iceoryx2 原始值的全部 128 bits，不依赖后端 feature，不做哈希或截断。
//! 来源身份不是密码学认证，也不代替消息验证、配置来源、序号或新鲜度守门。

/// 发布节点的完整标识；不能与发布端口标识混用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublisherNodeId(u128);

impl PublisherNodeId {
    /// 返回完整原始值，不丢失高位。
    #[must_use]
    pub const fn value(self) -> u128 {
        self.0
    }
}

/// 发布端口的完整标识；同一个节点的不同端口仍有不同身份。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublisherPortId(u128);

impl PublisherPortId {
    /// 返回完整原始值，不丢失高位。
    #[must_use]
    pub const fn value(self) -> u128 {
        self.0
    }
}

/// 一条样本实际关联的发布节点与发布端口。
///
/// 未知来源由 `Option::None` 表达；本类型没有默认身份。
///
/// ```compile_fail
/// use robot_transport::PublisherOrigin;
/// let _ = PublisherOrigin::default();
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublisherOrigin {
    node_id: PublisherNodeId,
    publisher_id: PublisherPortId,
}

impl PublisherOrigin {
    #[cfg(any(feature = "mock", feature = "iceoryx2", test))]
    const fn from_values(node_id: u128, publisher_id: u128) -> Self {
        Self {
            node_id: PublisherNodeId(node_id),
            publisher_id: PublisherPortId(publisher_id),
        }
    }

    /// 显式构造 mock 夹具身份；不会把它验证成真实发布者或已授权来源。
    ///
    /// 原始值包括显式提供的零值都原样保留；未知身份仍须使用 `None`。
    #[cfg(feature = "mock")]
    #[must_use]
    pub const fn for_mock(node_id: u128, publisher_id: u128) -> Self {
        Self::from_values(node_id, publisher_id)
    }

    /// 只允许真实后端从样本系统 header 构造，不开放应用层 native 构造入口。
    #[cfg(feature = "iceoryx2")]
    pub(crate) const fn from_native_values(node_id: u128, publisher_id: u128) -> Self {
        Self::from_values(node_id, publisher_id)
    }

    /// 返回发布节点身份；两个发布端口可以共享同一节点。
    #[must_use]
    pub const fn node_id(self) -> PublisherNodeId {
        self.node_id
    }

    /// 返回发布端口身份；不能把同节点的两端口当作同一端口。
    #[must_use]
    pub const fn publisher_id(self) -> PublisherPortId {
        self.publisher_id
    }
}

#[cfg(test)]
mod tests {
    use super::PublisherOrigin;

    #[test]
    fn origin_preserves_full_128_bit_values() {
        let origin = PublisherOrigin::from_values(u128::MAX - 1, u128::MAX);
        let copied = origin;
        assert_eq!(origin.node_id().value(), u128::MAX - 1);
        assert_eq!(origin.publisher_id().value(), u128::MAX);
        assert_eq!(copied, origin);
    }

    #[test]
    fn node_high_bits_do_not_alias() {
        let low = PublisherOrigin::from_values(7, 9);
        let high = PublisherOrigin::from_values((1_u128 << 127) | 7, 9);
        assert_ne!(low.node_id(), high.node_id());
        assert_eq!(low.publisher_id(), high.publisher_id());
        assert_ne!(low, high);
    }

    #[test]
    fn publisher_high_bits_do_not_alias() {
        let low = PublisherOrigin::from_values(7, 9);
        let high = PublisherOrigin::from_values(7, (1_u128 << 127) | 9);
        assert_eq!(low.node_id(), high.node_id());
        assert_ne!(low.publisher_id(), high.publisher_id());
        assert_ne!(low, high);
    }
}
