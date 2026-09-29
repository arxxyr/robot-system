//! 只读取机器人身份和关节顺序；地址、标定与控制配置均不使用。

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Deserialize;

use crate::{VizError, invalid};

#[derive(Debug, Deserialize)]
struct FileConfig {
    robot: Robot,
    #[serde(default)]
    hardware: Vec<Hardware>,
}

#[derive(Debug, Deserialize)]
struct Robot {
    id: String,
}

#[derive(Debug, Deserialize)]
struct Hardware {
    id: String,
    #[serde(default)]
    joints: Vec<String>,
    #[serde(default)]
    motor_ids: Vec<u32>,
}

/// 有身份约束的有序关节表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub robot_id: String,
    pub hardware_id: String,
    pub layout_id: u64,
    pub names: Vec<String>,
    pub ids: Vec<u32>,
}

impl Layout {
    pub fn new(
        robot_id: String,
        hardware_id: String,
        names: Vec<String>,
        ids: Vec<u32>,
    ) -> Result<Self, VizError> {
        validate_segment(&robot_id)?;
        validate_segment(&hardware_id)?;
        if names.is_empty()
            || names.len() > robot_core::adapter::MAX_JOINTS
            || names.len() != ids.len()
        {
            return Err(invalid("关节名称/ID 长度必须相等且为 1..32"));
        }
        for name in &names {
            validate_segment(name)?;
        }
        if names.iter().collect::<BTreeSet<_>>().len() != names.len()
            || ids.iter().collect::<BTreeSet<_>>().len() != ids.len()
        {
            return Err(invalid("关节名称或 ID 重复"));
        }
        let ordered: Vec<_> = names
            .iter()
            .zip(&ids)
            .map(|(name, id)| (name.as_str(), *id))
            .collect();
        let layout_id =
            robot_msg::joint_layout::compute_layout_id(&robot_id, &hardware_id, &ordered);
        Ok(Self {
            robot_id,
            hardware_id,
            layout_id,
            names,
            ids,
        })
    }
}

/// 允许观察的机器人与已验证的布局；广播不能覆盖冲突配置。
#[derive(Debug, Default)]
pub struct Layouts {
    robots: BTreeSet<String>,
    hardware: BTreeSet<(String, String)>,
    entries: BTreeMap<(String, String), Layout>,
}

impl Layouts {
    pub fn load(&mut self, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        // 配置大小有界；不打开其中出现的串口或标定路径。
        if std::fs::metadata(path)?.len() > 1024 * 1024 {
            return Err(invalid("配置超过 1 MiB").into());
        }
        self.add_toml(&std::fs::read_to_string(path)?)?;
        Ok(())
    }

    pub fn add_toml(&mut self, text: &str) -> Result<(), VizError> {
        let config: FileConfig =
            toml::from_str(text).map_err(|error| invalid(error.to_string()))?;
        validate_segment(&config.robot.id)?;
        if self.robots.len() >= 16 && !self.robots.contains(&config.robot.id) {
            return Err(invalid("最多观察 16 个机器人"));
        }
        self.robots.insert(config.robot.id.clone());
        for hardware in config.hardware {
            validate_segment(&hardware.id)?;
            self.hardware
                .insert((config.robot.id.clone(), hardware.id.clone()));
            if hardware.joints.is_empty() && hardware.motor_ids.is_empty() {
                continue;
            }
            self.insert(Layout::new(
                config.robot.id.clone(),
                hardware.id,
                hardware.joints,
                hardware.motor_ids,
            )?)?;
        }
        Ok(())
    }

    pub fn insert(&mut self, layout: Layout) -> Result<(), VizError> {
        if !self
            .hardware
            .contains(&(layout.robot_id.clone(), layout.hardware_id.clone()))
        {
            return Err(invalid("布局来源机器人/硬件未列入配置"));
        }
        let key = (layout.robot_id.clone(), layout.hardware_id.clone());
        match self.entries.get(&key) {
            Some(existing) if existing != &layout => Err(invalid("布局广播与已知布局冲突")),
            Some(_) => Ok(()),
            None if self.entries.len() >= 64 => Err(invalid("布局缓存最多 64 个硬件")),
            None => {
                self.entries.insert(key, layout);
                Ok(())
            }
        }
    }

    #[must_use]
    pub fn allows_topic(&self, topic: &str) -> bool {
        topic
            .strip_prefix('/')
            .and_then(|path| path.split('/').next())
            .is_some_and(|robot| self.robots.contains(robot))
    }

    pub fn slot<'a>(&self, topic: &'a str) -> Result<(&'a str, &'a str), VizError> {
        let parts: Vec<_> = topic.split('/').collect();
        match parts.as_slice() {
            ["", robot, "s0" | "s1", hardware, _] if self.robots.contains(*robot) => {
                if self
                    .hardware
                    .contains(&(String::from(*robot), String::from(*hardware)))
                {
                    Ok((robot, hardware))
                } else {
                    Err(invalid("topic 硬件未列入配置"))
                }
            }
            _ => Err(invalid("关节 topic 必须匹配配置机器人和 s0/s1 硬件槽位")),
        }
    }

    pub fn get(&self, topic: &str, layout_id: u64) -> Result<&Layout, VizError> {
        let (robot, hardware) = self.slot(topic)?;
        let layout = self
            .entries
            .get(&(robot.to_owned(), hardware.to_owned()))
            .ok_or_else(|| invalid("未知布局"))?;
        if layout.layout_id != layout_id {
            return Err(invalid("未知 layout_id；不解释关节数组"));
        }
        Ok(layout)
    }
}

pub(crate) fn validate_segment(value: &str) -> Result<(), VizError> {
    if value.is_empty()
        || value.len() > 128
        || value.contains('/')
        || value.chars().any(char::is_control)
    {
        return Err(invalid("名称必须为非空、无斜杠/控制字符且不超过 128 字节"));
    }
    Ok(())
}
