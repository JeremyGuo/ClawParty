use anyhow::{Result, anyhow};
use serde_json::{Map, Value};

fn object_arg<'a>(arguments: &'a Map<String, Value>, key: &str) -> Result<&'a Value> {
    arguments
        .get(key)
        .ok_or_else(|| anyhow!("missing required argument: {}", key))
}

pub(super) fn string_arg(arguments: &Map<String, Value>, key: &str) -> Result<String> {
    object_arg(arguments, key)?
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("argument {} must be a string", key))
}

pub(super) fn f64_arg(arguments: &Map<String, Value>, key: &str) -> Result<f64> {
    object_arg(arguments, key)?
        .as_f64()
        .ok_or_else(|| anyhow!("argument {} must be a number", key))
}

pub(super) fn usize_arg_with_default(
    arguments: &Map<String, Value>,
    key: &str,
    default: usize,
) -> Result<usize> {
    match arguments.get(key) {
        Some(value) => value
            .as_u64()
            .map(|value| value as usize)
            .ok_or_else(|| anyhow!("argument {} must be an integer", key)),
        None => Ok(default),
    }
}

pub(super) fn string_arg_with_default(
    arguments: &Map<String, Value>,
    key: &str,
    default: &str,
) -> Result<String> {
    match arguments.get(key) {
        Some(value) => value
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| anyhow!("argument {} must be a string", key)),
        None => Ok(default.to_string()),
    }
}

pub(super) fn string_array_arg(arguments: &Map<String, Value>, key: &str) -> Result<Vec<String>> {
    let Some(value) = arguments.get(key) else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| anyhow!("argument {} must be an array of strings", key))?;
    let mut values = Vec::with_capacity(items.len());
    for item in items {
        values.push(
            item.as_str()
                .ok_or_else(|| anyhow!("argument {} must be an array of strings", key))?
                .to_string(),
        );
    }
    Ok(values)
}

pub(super) fn string_arg_with_alias(
    arguments: &Map<String, Value>,
    key: &str,
    alias: &str,
) -> Result<String> {
    arguments
        .get(key)
        .or_else(|| arguments.get(alias))
        .ok_or_else(|| anyhow!("missing required argument: {}", key))?
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("argument {} must be a string", key))
}

pub(super) fn usize_arg_with_alias(
    arguments: &Map<String, Value>,
    key: &str,
    alias: &str,
) -> Result<Option<usize>> {
    match arguments.get(key).or_else(|| arguments.get(alias)) {
        Some(value) => value
            .as_u64()
            .map(|value| Some(value as usize))
            .ok_or_else(|| anyhow!("argument {} must be an integer", key)),
        None => Ok(None),
    }
}
