//! 只打开现存镜像订阅；本模块没有 publisher、RR client 或硬件入口。

use iceoryx2::prelude::Node;
use iceoryx2::service::ipc::Service;
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::{markers, markers::BodyMarker, schema_id};
use robot_transport::iceoryx2::{ObserverMetadata, ObserverSubscriber, observer_subscriber};

use crate::config::Layouts;
use crate::decode::{self, Observation};
use crate::{VizError, invalid};

pub(crate) type Received = (ObserverMetadata, Result<Observation, VizError>);

pub(crate) trait Source {
    fn poll(&mut self, topic: &str, layouts: &mut Layouts) -> Result<Option<Received>, VizError>;
}

struct TypedSource<M: BodyMarker>(ObserverSubscriber<M>);

impl<M: BodyMarker> Source for TypedSource<M> {
    fn poll(&mut self, topic: &str, layouts: &mut Layouts) -> Result<Option<Received>, VizError> {
        let Some(sample) = self.0.try_recv()? else {
            return Ok(None);
        };
        let metadata = sample.metadata();
        let now_ns = SystemClock.now().0;
        let result = sample
            .view()
            .map_err(VizError::from)
            .and_then(|view| decode::decode(&view, topic, now_ns, layouts));
        // sample 到这里归还，SDK 永远不持有共享内存 lease。
        Ok(Some((metadata, result)))
    }
}

pub(crate) fn open(
    node: &Node<Service>,
    topic: &str,
    schema: u32,
) -> Result<Box<dyn Source>, VizError> {
    macro_rules! subscriber {
        ($marker:ty) => {
            Ok(Box::new(TypedSource(observer_subscriber::<$marker>(
                node, topic,
            )?)))
        };
    }
    match schema {
        schema_id::SCHEMA_JOINT_LAYOUT => subscriber!(markers::JointLayout),
        schema_id::SCHEMA_JOINT_STATE => subscriber!(markers::JointState),
        schema_id::SCHEMA_JOINT_COMMAND => subscriber!(markers::JointCommand),
        schema_id::SCHEMA_TWIST => subscriber!(markers::Twist),
        schema_id::SCHEMA_IMAGE_HEADER => subscriber!(markers::ImageHeader),
        schema_id::SCHEMA_POINTCLOUD_HEADER => subscriber!(markers::PointCloudHeader),
        schema_id::SCHEMA_TENSOR_HEADER => subscriber!(markers::TensorHeader),
        schema_id::SCHEMA_ESTOP_EVENT => subscriber!(markers::EStopEvent),
        schema_id::SCHEMA_HEARTBEAT => subscriber!(markers::Heartbeat),
        schema_id::SCHEMA_ACTION_EVENT_ENVELOPE => subscriber!(markers::ActionEventEnvelope),
        _ => Err(invalid("发现非 pub/sub 观测 schema")),
    }
}
