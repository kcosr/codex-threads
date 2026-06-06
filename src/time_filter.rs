use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::errors::usage_error;

pub fn parse_since(since: &str) -> Result<i64> {
    if let Ok(timestamp) = since.parse::<i64>() {
        return Ok(timestamp);
    }
    let (number, multiplier) = if let Some(value) = since.strip_suffix('s') {
        (value, 1)
    } else if let Some(value) = since.strip_suffix('m') {
        (value, 60)
    } else if let Some(value) = since.strip_suffix('h') {
        (value, 60 * 60)
    } else if let Some(value) = since.strip_suffix('d') {
        (value, 60 * 60 * 24)
    } else {
        return Err(usage_error(format!("invalid --since value `{since}`")));
    };
    let seconds: i64 = number
        .parse()
        .with_context(|| format!("invalid --since value `{since}`"))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    Ok(now - seconds * multiplier)
}
