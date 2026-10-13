use anyhow::Context;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use wezterm_uds::UnixStream;


#[cfg(unix)]
mod unix {
    use super::*;

    pub struct NameHolder {
        published: PathBuf,
        name: PathBuf,
    }

    impl Drop for NameHolder {
        fn drop(&mut self) {
            // If it still points to us, remove the symlink
            if let Ok(target) = std::fs::read_link(&self.name) {
                if target == self.published {
                    log::trace!("removing {}", self.name.display());
                    std::fs::remove_file(&self.name).ok();
                }
            }
        }
    }

    impl NameHolder {
        fn compute_name(class_name: &str) -> String {
            #[cfg(not(target_os = "macos"))]
            {
                let config = config::configuration();
                if config.enable_wayland {
                    if let Ok(wayland) = std::env::var("WAYLAND_DISPLAY") {
                        return format!("wayland-{}-{}", wayland, class_name);
                    }
                    // We don't assume a default WAYLAND_DISPLAY here because
                    // we don't know if the default should be used or if we
                    // should fall back to X11 without connecting to wayland.
                    // We cannot introduce a dep on a wayland client library
                    // here, but we could potentially try to construct a
                    // unix domain socket client to see if our assumed default
                    // is a working unix socket.
                    // Something to fill in later as/when that question arises!
                }
                let x11 = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
                return format!("x11-{}-{}", x11, class_name);
            }
            #[cfg(target_os = "macos")]
            {
                format!("default-{}", class_name)
            }
        }

        fn compute_path(class_name: &str) -> PathBuf {
            config::RUNTIME_DIR.join(Self::compute_name(class_name))
        }

        pub fn new(path: &Path, class_name: &str) -> anyhow::Result<Self> {
            let name = Self::compute_path(class_name);
            std::fs::remove_file(&name).ok();
            std::os::unix::fs::symlink(path, &name)
                .with_context(|| format!("pointing {} -> {}", name.display(), path.display()))?;
            Ok(Self {
                published: path.to_path_buf(),
                name,
            })
        }

        pub fn resolve(class_name: &str) -> anyhow::Result<PathBuf> {
            let name = Self::compute_path(class_name);
            std::fs::read_link(&name).with_context(|| format!("reading symlink {}", name.display()))
        }
    }
}


#[cfg(unix)]
pub use self::unix::NameHolder;

/// Unconditionally update the published path to match the provided path,
/// even if there is a running instance with a legitimate published path.
pub fn publish_gui_sock_path(path: &Path, class_name: &str) -> anyhow::Result<NameHolder> {
    NameHolder::new(path, class_name)
}

/// Resolve the last published path for `class_name`.
/// If successful, there is NO guarantee that the returned path references
/// a running instance; it is just the last published path.
pub fn resolve_gui_sock_path(class_name: &str) -> anyhow::Result<PathBuf> {
    NameHolder::resolve(class_name)
}

/// This function returns a list of the gui-sock- paths in
/// the runtime dir.  These represent the locally running
/// instances of wezterm-gui.
/// The list is pruned of any entries that are not live
/// and then sorted with the eldest instance first.
pub fn discover_gui_socks() -> Vec<PathBuf> {
    let mut socks = vec![];

    #[derive(Debug)]
    struct Entry {
        path: PathBuf,
        age: Duration,
    }

    /// Get an idea of the age of the entry.
    /// Some filesystems don't support reporting `created`,
    /// so fall back on `modified`.
    fn meta_age(meta: &std::fs::Metadata) -> Duration {
        let t = if let Ok(created) = meta.created() {
            created
        } else if let Ok(changed) = meta.modified() {
            changed
        } else {
            return Duration::from_millis(300);
        };
        if let Ok(d) = SystemTime::now().duration_since(t) {
            d
        } else {
            Duration::from_millis(300)
        }
    }

    if let Ok(dir) = std::fs::read_dir(&*config::RUNTIME_DIR) {
        for entry in dir {
            if let Ok(entry) = entry {
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with("gui-sock-") {
                        let path = entry.path();
                        if let Ok(meta) = entry.metadata() {
                            let age = meta_age(&meta);
                            if is_sock_dead(&path) && age > Duration::from_secs(1) {
                                let _ = std::fs::remove_file(&path);
                            } else {
                                socks.push(Entry { path, age });
                            }
                        }
                    }
                }
            }
        }
    }

    socks.sort_by(|a, b| a.age.cmp(&b.age).reverse());
    log::trace!("{:?}", socks);
    socks.into_iter().map(|e| e.path).collect()
}

fn is_sock_dead(sock: &std::path::Path) -> bool {
    UnixStream::connect(sock).is_err()
}
