use crate::config::ResourceConfig;
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const MINICTR_CGROUP: &str = "minictr";

#[derive(Debug)]
pub(crate) struct Cgroup {
    path: PathBuf,
}

impl Cgroup {
    pub(crate) fn create(resources: &ResourceConfig) -> io::Result<Option<Self>> {
        if resources.is_empty() {
            return Ok(None);
        }

        let root = Path::new(CGROUP_ROOT);
        if resources.pids.is_some() {
            enable_controller(root, "pids")?;
        }

        let parent = root.join(MINICTR_CGROUP);
        fs::create_dir_all(&parent)?;
        if resources.pids.is_some() {
            enable_controller(&parent, "pids")?;
        }

        let path = parent.join(unique_name()?);
        fs::create_dir(&path)?;
        let cgroup = Self { path };
        if let Err(error) = cgroup.configure(resources) {
            let _ = fs::remove_dir(&cgroup.path);
            return Err(error);
        }
        Ok(Some(cgroup))
    }

    pub(crate) fn join_current(&self) -> io::Result<()> {
        fs::write(self.path.join("cgroup.procs"), "0")
    }

    pub(crate) fn remove(self) -> io::Result<()> {
        fs::remove_dir(self.path)
    }

    fn configure(&self, resources: &ResourceConfig) -> io::Result<()> {
        if let Some(pids) = &resources.pids {
            fs::write(self.path.join("pids.max"), pids.max.to_string())?;
        }
        Ok(())
    }
}

fn enable_controller(parent: &Path, controller: &str) -> io::Result<()> {
    let available = fs::read_to_string(parent.join("cgroup.controllers"))?;
    if !available
        .split_whitespace()
        .any(|candidate| candidate == controller)
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("cgroup v2 {controller} controller is not available"),
        ));
    }

    let subtree_control = parent.join("cgroup.subtree_control");
    let enabled = fs::read_to_string(&subtree_control)?;
    if !enabled
        .split_whitespace()
        .any(|candidate| candidate == controller)
    {
        fs::write(subtree_control, format!("+{controller}"))?;
    }
    Ok(())
}

fn unique_name() -> io::Result<String> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    Ok(format!("runtime-{}-{timestamp}", std::process::id()))
}
