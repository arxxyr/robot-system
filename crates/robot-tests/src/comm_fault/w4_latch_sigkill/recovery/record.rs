//! 仅解析本代 stdout 的固定完整行；诊断字段不产生任何事务凭证。

use serde_json::{Value, json};

pub(super) const TAG: &str = "R4_RECOVERY_REACHED_V1";
pub(super) const PREFIX: &str = "ReadMarker,ValidateMarker,SyncMarkerFile,SyncMarkerDirectory,CreatePrimary,WritePrimary,SyncPrimaryFile,RenamePrimary";
const MAX_RECORD_BYTES: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Record {
    pub(super) pid: u32,
    pub(super) base: u64,
    pub(super) reached: u64,
    pub(super) pause_until: u64,
    pub(super) dir_dev: u64,
    pub(super) dir_ino: u64,
    pub(super) marker_dev: u64,
    pub(super) marker_ino: u64,
    pub(super) txn: u128,
    pub(super) epoch: u64,
    pub(super) latched: bool,
    pub(super) bytes: [u8; 112],
    pub(super) raw: String,
}

fn value<'a>(token: &'a str, key: &str) -> Result<&'a str, String> {
    token
        .strip_prefix(key)
        .ok_or_else(|| format!("恢复记录字段次序或标签错误：{key}"))
}

fn decimal(token: &str, key: &str) -> Result<u64, String> {
    let text = value(token, key)?;
    let number = text.parse::<u64>().map_err(|error| error.to_string())?;
    if text != number.to_string() {
        return Err("恢复记录数字不是规范十进制".into());
    }
    Ok(number)
}

fn hex<const N: usize>(text: &str) -> Result<[u8; N], String> {
    if text.len() != 2 * N
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("恢复记录十六进制长度或大小写不符".into());
    }
    let mut bytes = [0; N];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)
            .map_err(|error| error.to_string())?;
    }
    Ok(bytes)
}

fn parse_line(raw: &str) -> Result<Record, String> {
    if raw.len() > MAX_RECORD_BYTES {
        return Err("恢复完整记录超过1024字节".into());
    }
    let line = raw.strip_suffix('\n').ok_or("恢复记录未完整换行")?;
    let parts: Vec<_> = line.split(' ').collect();
    if parts.len() != 15 || parts[0] != TAG || value(parts[14], "prefix=")? != PREFIX {
        return Err("恢复记录格式、字段数或真实前缀错误".into());
    }
    let pid = u32::try_from(decimal(parts[1], "pid=")?).map_err(|error| error.to_string())?;
    let txn_text = value(parts[9], "txn=")?;
    let _ = hex::<16>(txn_text)?;
    let txn = u128::from_str_radix(txn_text, 16).map_err(|error| error.to_string())?;
    let latched = match value(parts[11], "latched=")? {
        "0" => false,
        "1" => true,
        _ => return Err("恢复记录latch非法".into()),
    };
    let bytes = hex::<112>(value(parts[12], "record=")?)?;
    let checksum = hex::<32>(value(parts[13], "checksum=")?)?;
    let record = Record {
        pid,
        base: decimal(parts[2], "base=")?,
        reached: decimal(parts[3], "reached=")?,
        pause_until: decimal(parts[4], "pause_until=")?,
        dir_dev: decimal(parts[5], "dir_dev=")?,
        dir_ino: decimal(parts[6], "dir_ino=")?,
        marker_dev: decimal(parts[7], "marker_dev=")?,
        marker_ino: decimal(parts[8], "marker_ino=")?,
        txn,
        epoch: decimal(parts[10], "epoch=")?,
        latched,
        bytes,
        raw: raw.to_owned(),
    };
    if pid == 0
        || record.base == 0
        || record.dir_ino == 0
        || record.marker_ino == 0
        || txn == 0
        || !latched
        || &bytes[..8] != b"RBTCLRTX"
        || bytes[8..10] != [1, 0]
        || bytes[10..16] != [0; 6]
        || bytes[48..64] != txn.to_le_bytes()
        || bytes[64..72] != record.epoch.to_le_bytes()
        || bytes[72] != u8::from(latched)
        || bytes[73..80] != [0; 7]
        || bytes[80..112] != checksum
    {
        return Err("恢复记录诊断字段与规范112字节不一致".into());
    }
    // checksum只核原记录内一致性；完整112字节另与真实原marker逐字节对照。
    Ok(record)
}

pub(super) fn extract(bytes: &[u8]) -> Result<Option<Record>, String> {
    let stdout = std::str::from_utf8(bytes).map_err(|error| error.to_string())?;
    let mut found = None;
    let mut mentions = 0;
    for line in stdout.split_inclusive('\n') {
        if line.contains("R4_RECOVERY_DIAGNOSTIC_V1") {
            return Err("子进程恢复观测已记录独立失败".into());
        }
        if !line.contains(TAG)
            && (line.starts_with("R4_RECOVERY_") || TAG.starts_with(line.trim_end_matches('\n')))
        {
            if !line.ends_with('\n') && found.is_none() {
                return Ok(None);
            }
            return Err("恢复记录标签截断、未知或重复".into());
        }
        if !line.contains(TAG) {
            continue;
        }
        mentions += line.matches(TAG).count();
        if mentions != 1 || !line.starts_with(TAG) {
            return Err("恢复记录重复或被其它输出穿插".into());
        }
        if line.len() > MAX_RECORD_BYTES {
            return Err("恢复记录已超出固定长度".into());
        }
        if !line.ends_with('\n') {
            return Ok(None);
        }
        found = Some(parse_line(line)?);
    }
    Ok(found)
}

impl Record {
    pub(super) fn cp1(&self) -> Result<u64, String> {
        self.base
            .checked_add(5_000_000_000)
            .ok_or_else(|| "原CP1溢出".into())
    }
    pub(super) fn window(&self, before: u64, after: u64) -> Result<(), String> {
        if self.reached < self.base
            || self.pause_until <= self.reached
            || self.pause_until - self.reached > 250_000_000
            || self.pause_until >= self.cp1()?
            || before < self.reached
            || after < before
            || after >= self.pause_until
        {
            return Err("恢复记录接纳或实际信号不在原CP1/250ms窗口".into());
        }
        Ok(())
    }
    pub(super) fn json(&self) -> Value {
        json!({"pid":self.pid,"base":self.base,"reached":self.reached,"pause_until":self.pause_until,
            "dir_dev":self.dir_dev,"dir_ino":self.dir_ino,"marker_dev":self.marker_dev,"marker_ino":self.marker_ino,
            "txn":self.txn.to_string(),"epoch":self.epoch,"latched":self.latched,"record_bytes":self.bytes.as_slice(),
            "record_checksum":&self.bytes[80..],"prefix":PREFIX,"raw":self.raw})
    }
}

#[cfg(test)]
mod tests;
