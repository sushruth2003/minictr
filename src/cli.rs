use clap::{Parser, error::ErrorKind};
use std::{
    ffi::OsString,
    path::{Component, Path, PathBuf},
};

const MAX_HOSTNAME_LEN: usize = 64;
pub(crate) const OLD_ROOT_PATH: &str = "/oldroot";

#[derive(Parser, Debug, PartialEq, Eq)]
#[command(author, version, about)]
pub(crate) struct Cli {
    pub(crate) operation: String,
    #[arg(long, value_name = "path")]
    pub(crate) rootfs: PathBuf,
    #[arg(long, value_name = "hostname", value_parser = validate_hostname)]
    pub(crate) hostname: Option<String>,
    #[arg(
        long = "mount",
        value_name = "host_path:container_path",
        value_parser = parse_bind_mount
    )]
    pub(crate) bind_mounts: Vec<BindMount>,
    #[arg(long, value_name = "path")]
    pub(crate) config: Option<PathBuf>,
    #[arg(long)]
    pub(crate) init: bool,
    #[arg(num_args = 1.., value_name = "command", allow_hyphen_values = true)]
    pub(crate) command_tokens: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BindMount {
    pub(crate) source: PathBuf,
    pub(crate) destination: PathBuf,
}

pub(crate) fn parse_cli<I, T>(args: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = Cli::try_parse_from(args)?;
    if cli.operation != "run" {
        return Err(clap::Error::raw(
            ErrorKind::ValueValidation,
            "only supported operation is run",
        ));
    }
    Ok(cli)
}

fn parse_bind_mount(value: &str) -> Result<BindMount, String> {
    let (source, destination) = value
        .rsplit_once(':')
        .ok_or_else(|| "mount must use host_path:container_path syntax".to_owned())?;
    if source.is_empty() {
        return Err("mount host path must not be empty".to_owned());
    }

    let destination = PathBuf::from(destination);
    if !destination.is_absolute() {
        return Err("mount container path must be absolute".to_owned());
    }
    if destination
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err("mount container path must not contain '..'".to_owned());
    }
    if destination == Path::new("/") {
        return Err("mount container path must not replace the container root".to_owned());
    }
    if destination.starts_with(OLD_ROOT_PATH) {
        return Err(format!(
            "mount container path must not use reserved path {OLD_ROOT_PATH}"
        ));
    }

    Ok(BindMount {
        source: PathBuf::from(source),
        destination,
    })
}

fn validate_hostname(value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err("hostname must not be empty".to_owned());
    }
    if value.len() > MAX_HOSTNAME_LEN {
        return Err(format!("hostname must be at most {MAX_HOSTNAME_LEN} bytes"));
    }
    if value
        .bytes()
        .any(|byte| !(byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'.'))
    {
        return Err("hostname may contain only letters, digits, '-' and '.'".to_owned());
    }
    for label in value.split('.') {
        if label.is_empty() || label.starts_with('-') || label.ends_with('-') {
            return Err("hostname labels must not be empty or start/end with '-'".to_owned());
        }
    }
    Ok(value.to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn cli_args_become_runtime_configuration() {
        let cli = parse_cli([
            "minictr",
            "run",
            "--rootfs",
            "./rootfs",
            "--hostname",
            "testbox",
            "--config",
            "resources.json",
            "--init",
            "--mount",
            "/host/data:/data",
            "--",
            "sh",
            "-c",
            "printf hello",
        ])
        .unwrap();

        assert_eq!(cli.operation, "run");
        assert_eq!(cli.rootfs, PathBuf::from("./rootfs"));
        assert_eq!(cli.hostname.as_deref(), Some("testbox"));
        assert_eq!(cli.config, Some(PathBuf::from("resources.json")));
        assert!(cli.init);
        assert_eq!(
            cli.bind_mounts,
            [BindMount {
                source: PathBuf::from("/host/data"),
                destination: PathBuf::from("/data"),
            }]
        );
        assert_eq!(cli.command_tokens, ["sh", "-c", "printf hello"]);
    }

    #[test]
    fn rootfs_is_parsed_before_the_command_separator() {
        let cli = parse_cli(["minictr", "run", "--rootfs", "./rootfs", "--", "/bin/sh"]).unwrap();

        assert_eq!(cli.rootfs, PathBuf::from("./rootfs"));
        assert_eq!(cli.hostname, None);
        assert_eq!(cli.config, None);
        assert!(!cli.init);
        assert!(cli.bind_mounts.is_empty());
        assert_eq!(cli.command_tokens, ["/bin/sh"]);
    }

    #[test]
    fn bind_mount_parser_requires_an_absolute_container_path() {
        assert_eq!(
            parse_bind_mount("/host/data:/data").unwrap(),
            BindMount {
                source: PathBuf::from("/host/data"),
                destination: PathBuf::from("/data"),
            }
        );
        assert!(parse_bind_mount("/host/data:data").is_err());
        assert!(parse_bind_mount("/host/data").is_err());
        assert!(parse_bind_mount(":/data").is_err());
        assert!(parse_bind_mount("/host/data:/../outside").is_err());
        assert!(parse_bind_mount("/host/data:/").is_err());
        assert!(parse_bind_mount("/host/data:/oldroot").is_err());
        assert!(parse_bind_mount("/host/data:/oldroot/nested").is_err());
        assert!(parse_bind_mount("/host/data:/oldroot-safe").is_ok());
    }

    #[test]
    fn hostname_validation_rejects_invalid_values() {
        assert!(validate_hostname("").is_err());
        assert!(validate_hostname("has space").is_err());
        assert!(validate_hostname("-starts-with-dash").is_err());
        assert!(validate_hostname(&"x".repeat(MAX_HOSTNAME_LEN + 1)).is_err());
        assert_eq!(
            validate_hostname("testbox-1.example").unwrap(),
            "testbox-1.example"
        );
    }
}
