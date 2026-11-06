//! 只读观测桥：验证消息、复制有界数据，再交给独立 Rerun SDK。

pub mod config;
pub mod decode;
mod input;
pub mod media;
pub mod output;
pub mod runtime;
pub mod supervisor;

/// 单帧 blob 的既有 transport 档位上限。
pub const MAX_BLOB_BYTES: usize = 16 * 1024 * 1024;
/// 媒体解码后的元素数上限，防止小整数张量展开占用无界内存。
pub const MAX_ELEMENTS: usize = 1_048_576;

/// 观测输入拒绝原因；不转成机器人控制故障。
#[derive(Debug, thiserror::Error)]
pub enum VizError {
    #[error("消息不符合观测约定：{0}")]
    Invalid(String),
    #[error("消息传输校验失败：{0}")]
    Transport(#[from] robot_transport::TransportError),
    #[error("FlatBuffers 校验失败：{0}")]
    Flatbuffer(#[from] flatbuffers::InvalidFlatbuffer),
}

pub(crate) fn invalid(reason: impl Into<String>) -> VizError {
    VizError::Invalid(reason.into())
}

#[derive(Debug, thiserror::Error)]
#[error("{primary}；{stage}也失败：{secondary}")]
struct SecondaryFailure {
    #[source]
    primary: Box<dyn std::error::Error>,
    secondary: Box<dyn std::error::Error>,
    stage: &'static str,
}

/// 收尾失败附加到主错误，不覆盖最初的失败及其错误链。
pub(crate) fn preserve_primary(
    primary: Result<(), Box<dyn std::error::Error>>,
    secondary: Result<(), Box<dyn std::error::Error>>,
    stage: &'static str,
) -> Result<(), Box<dyn std::error::Error>> {
    match (primary, secondary) {
        (Ok(()), secondary) => secondary,
        (Err(primary), Ok(())) => Err(primary),
        (Err(primary), Err(secondary)) => Err(Box::new(SecondaryFailure {
            primary,
            secondary,
            stage,
        })),
    }
}
