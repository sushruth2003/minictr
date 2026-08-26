use serde::Deserialize;
use std::{fs, io, path::Path};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RuntimeConfig {
    pub(crate) resources: ResourceConfig,
}

impl RuntimeConfig {
    pub(crate) fn load(path: Option<&Path>) -> io::Result<Self> {
        let Some(path) = path else {
            return Ok(Self::default());
        };

        let contents = fs::read_to_string(path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("failed to read config {}: {error}", path.display()),
            )
        })?;
        Self::parse(&contents, path)
    }

    fn parse(contents: &str, path: &Path) -> io::Result<Self> {
        let config: ConfigFile = serde_json::from_str(contents).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("failed to parse config {}: {error}", path.display()),
            )
        })?;
        config.validate(path)?;
        Ok(Self {
            resources: config.resources,
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(default)]
    resources: ResourceConfig,
}

impl ConfigFile {
    fn validate(&self, path: &Path) -> io::Result<()> {
        if self
            .resources
            .pids
            .as_ref()
            .is_some_and(|pids| pids.max == 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "invalid config {}: resources.pids.max must be greater than zero",
                    path.display()
                ),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResourceConfig {
    pub(crate) pids: Option<PidsConfig>,
}

impl ResourceConfig {
    pub(crate) fn is_empty(&self) -> bool {
        self.pids.is_none()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PidsConfig {
    pub(crate) max: u64,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn config_path() -> PathBuf {
        PathBuf::from("test-config.json")
    }

    #[test]
    fn missing_config_means_no_resource_limits() {
        let config = RuntimeConfig::load(None).unwrap();

        assert!(config.resources.is_empty());
    }

    #[test]
    fn parses_pid_limit() {
        let config =
            RuntimeConfig::parse(r#"{"resources":{"pids":{"max":16}}}"#, &config_path()).unwrap();

        assert_eq!(config.resources.pids.unwrap().max, 16);
    }

    #[test]
    fn rejects_unknown_fields() {
        let error = RuntimeConfig::parse(r#"{"resources":{"pid":{"max":16}}}"#, &config_path())
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("unknown field `pid`"));
    }

    #[test]
    fn rejects_zero_pid_limit() {
        let error = RuntimeConfig::parse(r#"{"resources":{"pids":{"max":0}}}"#, &config_path())
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("must be greater than zero"));
    }
}
