//! Versioned Spine configuration snapshots, including explicit conversion of legacy locks.
use codex_config::ConfigLayerEntry;
use codex_config::ConfigLayerSource;
use codex_config::config_toml::ConfigToml;
use codex_config::spine_snapshot::ConfigLockfileToml;
use codex_config::spine_snapshot::SpineConfigLockToml;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Serialize;
use serde::de::DeserializeOwned;
use spine_core::host::RecordDigest;
use spine_core::host::SpineConfig;
use std::io;

pub(crate) const CONFIG_LOCK_VERSION: u32 = 3;

pub(crate) async fn read_config_lock_from_path(
    path: &AbsolutePathBuf,
) -> io::Result<ConfigLockfileToml> {
    let contents = tokio::fs::read_to_string(path).await?;
    let mut lock: ConfigLockfileToml = toml::from_str(&contents).map_err(|error| {
        config_lock_error(format!("invalid snapshot {}: {error}", path.display()))
    })?;
    match lock.version {
        1 | 2 => {
            let mut merged: toml::Value = toml::from_str(spine_core::host::DEFAULT_CONFIG_TOML)
                .map_err(|error| config_lock_error(error.to_string()))?;
            let bundled_digest =
                RecordDigest::digest(spine_core::host::DEFAULT_CONFIG_TOML.as_bytes())
                    .as_str()
                    .to_string();
            let mut snapshot = match (lock.version, lock.spine_config.take()) {
                (1, None) => SpineConfigLockToml {
                    schema_version: 1,
                    bundled_digest: bundled_digest.clone(),
                    sources: Vec::new(),
                    effective_config: None,
                },
                (2, Some(snapshot)) => snapshot,
                _ => {
                    return Err(config_lock_error(format!(
                        "snapshot {} has invalid version {} source metadata",
                        path.display(),
                        lock.version
                    )));
                }
            };
            if snapshot.bundled_digest != bundled_digest {
                return Err(config_lock_error(format!(
                    "snapshot {} references unavailable bundled Spine config {}",
                    path.display(),
                    snapshot.bundled_digest
                )));
            }
            if lock.version == 1 && lock.config.spine_config_file.is_some() {
                return Err(config_lock_error(format!(
                    "snapshot {} does not contain the external Spine config required to reconstruct version 1",
                    path.display()
                )));
            }
            for source in &snapshot.sources {
                // A missing optional layer was not part of this old snapshot. New files must not change its meaning.
                let Some(digest) = &source.digest else {
                    if source.required {
                        return Err(config_lock_error(format!(
                            "snapshot source {} has no recorded digest",
                            source.path.display()
                        )));
                    }
                    continue;
                };
                let contents = tokio::fs::read_to_string(&source.path)
                    .await
                    .map_err(|error| {
                        config_lock_error(format!(
                            "cannot reconstruct snapshot source {}: {error}",
                            source.path.display()
                        ))
                    })?;
                if RecordDigest::digest(contents.as_bytes()).as_str() != digest {
                    return Err(config_lock_error(format!(
                        "snapshot source {} no longer matches recorded digest {digest}",
                        source.path.display()
                    )));
                }
                let layer: toml::Value = toml::from_str(&contents).map_err(|error| {
                    config_lock_error(format!(
                        "invalid snapshot source {}: {error}",
                        source.path.display()
                    ))
                })?;
                codex_config::merge_toml_values(&mut merged, &layer);
            }
            let effective =
                toml::to_string(&merged).map_err(|error| config_lock_error(error.to_string()))?;
            SpineConfig::parse_toml(&effective)
                .map_err(|error| config_lock_error(error.to_string()))?;
            snapshot.effective_config = Some(effective);
            lock.spine_config = Some(snapshot);
            lock.version = CONFIG_LOCK_VERSION;
        }
        CONFIG_LOCK_VERSION => {}
        version => {
            return Err(config_lock_error(format!(
                "unsupported Spine snapshot version {version} in {}",
                path.display()
            )));
        }
    }
    let snapshot = lock.spine_config.as_ref().ok_or_else(|| {
        config_lock_error(format!("snapshot {} has no Spine config", path.display()))
    })?;
    let effective = snapshot.effective_config.as_deref().ok_or_else(|| {
        config_lock_error(format!(
            "snapshot {} has no effective Spine config",
            path.display()
        ))
    })?;
    SpineConfig::parse_toml(effective).map_err(|error| config_lock_error(error.to_string()))?;
    lock.config.spine_config_snapshot = Some(snapshot.clone());
    Ok(lock)
}

pub(crate) fn config_lockfile(
    mut config: ConfigToml,
    spine_config: SpineConfigLockToml,
) -> ConfigLockfileToml {
    config.spine_config_snapshot = Some(spine_config.clone());
    ConfigLockfileToml {
        version: CONFIG_LOCK_VERSION,
        codex_version: env!("CARGO_PKG_VERSION").to_string(),
        spine_config: Some(spine_config),
        config,
    }
}

pub(crate) fn lock_layer_from_config(
    path: &AbsolutePathBuf,
    lock: &ConfigLockfileToml,
) -> io::Result<ConfigLayerEntry> {
    Ok(ConfigLayerEntry::new(
        ConfigLayerSource::User {
            file: path.clone(),
            profile: None,
        },
        toml_value(
            &config_without_lock_controls(&lock.config),
            "Spine snapshot",
        )?,
    ))
}

pub(crate) fn config_without_lock_controls(config: &ConfigToml) -> ConfigToml {
    let mut config = config.clone();
    clear_config_lock_debug_controls(&mut config);
    config
}

pub(crate) fn clear_config_lock_debug_controls(config: &mut ConfigToml) {
    config.debug = None;
    config.spine_snapshot = None;
}

fn config_lock_error(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn toml_value<T: Serialize>(value: &T, label: &str) -> io::Result<toml::Value> {
    toml::Value::try_from(value)
        .map_err(|err| config_lock_error(format!("failed to serialize {label}: {err}")))
}

pub(crate) fn toml_round_trip<T>(value: &impl Serialize, label: &'static str) -> io::Result<T>
where
    T: DeserializeOwned + Serialize,
{
    let value = toml_value(value, label)?;
    let toml = value.clone().try_into().map_err(|err| {
        config_lock_error(format!("failed to convert {label} to TOML shape: {err}"))
    })?;
    let represented_value = toml_value(&toml, label)?;
    if represented_value != value {
        return Err(config_lock_error(format!(
            "resolved {label} cannot be fully represented as TOML"
        )));
    }
    Ok(toml)
}
