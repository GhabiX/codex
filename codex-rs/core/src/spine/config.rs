use crate::config::ManagedFeatures;
use codex_config::spine_snapshot::SpineConfigLockToml;
use codex_config::spine_snapshot::SpineConfigSourceLockToml;
use codex_features::Feature as CodexFeature;
use codex_utils_absolute_path::AbsolutePathBuf;
use spine_core::host::RecordDigest;
use spine_core::host::SpineConfig;
use spine_core::host::SpineConfigLoader;
use spine_core::host::ToolCatalog;
use std::io;
use std::path::Path;

/// SDK configuration is selected at session initialization, after resume history is available.
#[derive(Clone, Debug, PartialEq)]
pub struct SpineConfiguration {
    state: ConfigurationState,
}

#[derive(Clone, Debug, PartialEq)]
enum ConfigurationState {
    Sources(SpineConfigLoader),
    Snapshot(SpineConfigLockToml),
    Resolved(Box<ResolvedConfiguration>),
}

#[derive(Clone, Debug, PartialEq)]
struct ResolvedConfiguration {
    sdk: SpineConfig,
    tools: ToolCatalog,
    snapshot: SpineConfigLockToml,
}

impl SpineConfiguration {
    pub(crate) fn pending(
        path: Option<&AbsolutePathBuf>,
        snapshot: Option<&SpineConfigLockToml>,
        working_directory: &Path,
        home_directory: Option<&Path>,
        project_config_trusted: bool,
    ) -> Self {
        Self {
            state: match snapshot {
                Some(snapshot) => ConfigurationState::Snapshot(snapshot.clone()),
                None => ConfigurationState::Sources(loader(
                    path,
                    working_directory,
                    home_directory,
                    project_config_trusted,
                )),
            },
        }
    }

    /// Creates an already resolved SDK configuration for an embedding host.
    pub fn from_sdk(sdk: SpineConfig) -> anyhow::Result<Self> {
        let snapshot = SpineConfigLockToml {
            schema_version: sdk.schema_version(),
            bundled_digest: RecordDigest::digest(spine_core::host::DEFAULT_CONFIG_TOML.as_bytes())
                .as_str()
                .to_string(),
            sources: Vec::new(),
            effective_config: Some(sdk.snapshot_toml()?),
        };
        let tools = ToolCatalog::new(&sdk)?;
        Ok(Self {
            state: ConfigurationState::Resolved(Box::new(ResolvedConfiguration {
                sdk,
                tools,
                snapshot,
            })),
        })
    }

    /// Returns the SDK after the session initialization boundary has resolved it.
    pub fn sdk(&self) -> &SpineConfig {
        match &self.state {
            ConfigurationState::Resolved(resolved) => &resolved.sdk,
            ConfigurationState::Sources(_) | ConfigurationState::Snapshot(_) => {
                panic!(
                    "session initialization must resolve Spine configuration before using the SDK"
                )
            }
        }
    }

    /// Returns tools from the same resolved configuration as the SDK.
    pub fn tools(&self) -> &ToolCatalog {
        match &self.state {
            ConfigurationState::Resolved(resolved) => &resolved.tools,
            ConfigurationState::Sources(_) | ConfigurationState::Snapshot(_) => {
                panic!("session initialization must resolve Spine configuration before using tools")
            }
        }
    }

    pub(crate) fn snapshot(&self) -> &SpineConfigLockToml {
        match &self.state {
            ConfigurationState::Resolved(resolved) => &resolved.snapshot,
            ConfigurationState::Sources(_) | ConfigurationState::Snapshot(_) => {
                panic!(
                    "session initialization must resolve Spine configuration before exporting it"
                )
            }
        }
    }

    pub(crate) fn resolve(
        &self,
        saved: Option<&str>,
        features: &ManagedFeatures,
    ) -> anyhow::Result<Self> {
        let snapshot = match saved {
            Some(saved) => Self::from_sdk(SpineConfig::parse_toml(saved)?)?
                .snapshot()
                .clone(),
            None => match &self.state {
                ConfigurationState::Sources(loader) => lock_snapshot(loader.clone())?,
                ConfigurationState::Snapshot(snapshot) => snapshot.clone(),
                ConfigurationState::Resolved(resolved) => resolved.snapshot.clone(),
            },
        };
        let effective = snapshot.effective_config.as_deref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Spine snapshot is missing effective config",
            )
        })?;
        let enabled = [
            (CodexFeature::SpineJit, spine_core::host::Feature::Jit),
            (CodexFeature::SpineSpawn, spine_core::host::Feature::Spawn),
        ]
        .into_iter()
        .filter(|(host, _)| features.enabled(*host))
        .map(|(_, sdk)| sdk);
        let sdk = SpineConfig::parse_toml(effective)?.with_features(enabled)?;
        let tools = ToolCatalog::new(&sdk)?;
        Ok(Self {
            state: ConfigurationState::Resolved(Box::new(ResolvedConfiguration {
                sdk,
                tools,
                snapshot,
            })),
        })
    }
}

fn lock_snapshot(loader: SpineConfigLoader) -> io::Result<SpineConfigLockToml> {
    let mut sources = loader
        .optional_source_files()
        .into_iter()
        .map(|path| {
            let digest = match std::fs::read(&path) {
                Ok(contents) => Some(RecordDigest::digest(&contents).as_str().to_string()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(io::Error::new(
                        error.kind(),
                        format!(
                            "failed to pin Spine config source {} in config lock: {error}",
                            path.display()
                        ),
                    ));
                }
            };
            Ok(SpineConfigSourceLockToml {
                path: AbsolutePathBuf::try_from(path)?,
                required: false,
                digest,
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    if let Some(path) = loader.required_source_file() {
        let contents = std::fs::read(&path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "failed to pin Spine config source {} in config lock: {error}",
                    path.display()
                ),
            )
        })?;
        sources.push(SpineConfigSourceLockToml {
            path: AbsolutePathBuf::try_from(path)?,
            required: true,
            digest: Some(RecordDigest::digest(&contents).as_str().to_string()),
        });
    }

    Ok(SpineConfigLockToml {
        schema_version: SpineConfig::v1().schema_version(),
        bundled_digest: RecordDigest::digest(spine_core::host::DEFAULT_CONFIG_TOML.as_bytes())
            .as_str()
            .to_string(),
        sources,
        effective_config: Some(
            loader
                .load()
                .map_err(io::Error::from)?
                .snapshot_toml()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        ),
    })
}

fn loader(
    path: Option<&AbsolutePathBuf>,
    working_directory: &Path,
    home_directory: Option<&Path>,
    project_config_trusted: bool,
) -> SpineConfigLoader {
    let mut loader = SpineConfigLoader::new(working_directory);
    if !project_config_trusted {
        loader = loader.without_working_directory_layers();
    }
    if let Some(home_directory) = home_directory {
        loader = loader.with_home_directory(home_directory);
    }
    if let Some(path) = path {
        loader = loader.with_custom_path(path.as_path());
    }
    loader
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;

/// Reuse the SDK configuration captured at the effective transaction boundary on resume or fork.
pub(crate) fn restore_sampling_config(
    config: &mut crate::config::Config,
    history: &codex_history::InitialHistory,
) -> anyhow::Result<()> {
    let effective = super::effective_rollout(history.get_spine_rollout_items());
    let snapshot = effective.iter().rev().find_map(|(_, item)| match item {
        codex_history::RolloutItem::SpineSamplingStarted(started) => started.sdk_config.as_deref(),
        _ => None,
    });
    config.spine = config.spine.resolve(snapshot, &config.features)?;
    Ok(())
}
