use crate::cli::{BindMount, OLD_ROOT_PATH};
use nix::mount::{MntFlags, MsFlags, mount, umount2};
use nix::unistd::pivot_root;
use std::{fs, io, path::Path, path::PathBuf};

pub(crate) fn make_mount_tree_private() -> io::Result<()> {
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        private_mount_flags(),
        None::<&str>,
    )
    .map_err(nix_error_to_io)
}

pub(crate) fn make_rootfs_mount_point(rootfs: &Path) -> io::Result<()> {
    mount(
        Some(rootfs),
        rootfs,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(nix_error_to_io)
}

pub(crate) fn mount_bind(rootfs: &Path, bind_mount: &BindMount) -> io::Result<()> {
    let target = prepare_bind_target(rootfs, bind_mount)?;
    let mut flags = MsFlags::MS_BIND;
    if bind_mount.source.is_dir() {
        flags |= MsFlags::MS_REC;
    }

    mount(
        Some(bind_mount.source.as_path()),
        target.as_path(),
        None::<&str>,
        flags,
        None::<&str>,
    )
    .map_err(nix_error_to_io)
}

pub(crate) fn pivot_into_rootfs(rootfs: &Path) -> io::Result<()> {
    let put_old = rootfs.join(OLD_ROOT_PATH.trim_start_matches('/'));
    prepare_old_root_directory(&put_old)?;

    std::env::set_current_dir(rootfs)?;
    pivot_root(Path::new("."), Path::new("oldroot")).map_err(nix_error_to_io)?;
    std::env::set_current_dir("/")?;
    umount2(OLD_ROOT_PATH, MntFlags::MNT_DETACH).map_err(nix_error_to_io)?;
    fs::remove_dir(OLD_ROOT_PATH)
}

pub(crate) fn mount_procfs() -> io::Result<()> {
    fs::create_dir_all("/proc")?;
    mount(
        Some("proc"),
        "/proc",
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        None::<&str>,
    )
    .map_err(nix_error_to_io)
}

fn prepare_bind_target(rootfs: &Path, bind_mount: &BindMount) -> io::Result<PathBuf> {
    let relative_destination = bind_mount
        .destination
        .strip_prefix("/")
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "mount target is not absolute"))?;
    let target = rootfs.join(relative_destination);
    ensure_existing_ancestor_is_in_rootfs(rootfs, &target)?;

    if bind_mount.source.is_dir() {
        fs::create_dir_all(&target)?;
    } else {
        let parent = target.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "mount target has no parent")
        })?;
        fs::create_dir_all(parent)?;
        if !target.exists() {
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)?;
        }
    }

    ensure_existing_ancestor_is_in_rootfs(rootfs, &target)?;
    Ok(target)
}

fn ensure_existing_ancestor_is_in_rootfs(rootfs: &Path, target: &Path) -> io::Result<()> {
    let mut existing = target;
    while !existing.exists() {
        existing = existing.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "mount target has no existing ancestor",
            )
        })?;
    }

    let resolved = fs::canonicalize(existing)?;
    if !resolved.starts_with(rootfs) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "mount target resolves outside rootfs through {}",
                existing.display()
            ),
        ));
    }
    Ok(())
}

fn prepare_old_root_directory(put_old: &Path) -> io::Result<()> {
    match fs::symlink_metadata(put_old) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            if fs::read_dir(put_old)?.next().is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "reserved old-root directory is not empty: {}",
                        put_old.display()
                    ),
                ));
            }
            Ok(())
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "reserved old-root path is not a directory: {}",
                put_old.display()
            ),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(put_old),
        Err(error) => Err(error),
    }
}

fn private_mount_flags() -> MsFlags {
    MsFlags::MS_PRIVATE | MsFlags::MS_REC
}

fn nix_error_to_io(error: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_tree_is_configured_as_recursively_private() {
        let flags = private_mount_flags();

        assert!(flags.contains(MsFlags::MS_PRIVATE));
        assert!(flags.contains(MsFlags::MS_REC));
    }
}
