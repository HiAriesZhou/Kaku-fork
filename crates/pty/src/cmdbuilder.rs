#[cfg(unix)]
use anyhow::Context;
#[cfg(feature = "serde_support")]
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::path::Component;
use std::path::Path;

/// Used to deal with Windows having case-insensitive environment variables.
#[derive(Clone, Debug, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde_support", derive(Serialize, Deserialize))]
struct EnvEntry {
    /// Whether or not this environment variable came from the base environment,
    /// as opposed to having been explicitly set by the caller.
    is_from_base_env: bool,

    /// For case-insensitive platforms, the environment variable key in its preferred casing.
    preferred_key: OsString,

    /// The environment variable value.
    value: OsString,
}

impl EnvEntry {
    fn map_key(k: OsString) -> OsString {
        k
    }
}

#[cfg(unix)]
fn get_shell() -> String {
    use nix::unistd::{access, AccessFlags};
    use std::ffi::CStr;
    use std::str;

    let ent = unsafe { libc::getpwuid(libc::getuid()) };
    if !ent.is_null() {
        let shell = unsafe { CStr::from_ptr((*ent).pw_shell) };
        match shell.to_str().map(str::to_owned) {
            Err(err) => {
                log::warn!(
                    "passwd database shell could not be \
                     represented as utf-8: {err:#}, \
                     falling back to /bin/sh"
                );
            }
            Ok(shell) => {
                if let Err(err) = access(Path::new(&shell), AccessFlags::X_OK) {
                    log::warn!(
                        "passwd database shell={shell:?} which is \
                         not executable ({err:#}), falling back to /bin/sh"
                    );
                } else {
                    return shell;
                }
            }
        }
    }
    "/bin/sh".into()
}

fn get_base_env() -> BTreeMap<OsString, EnvEntry> {
    let mut env: BTreeMap<OsString, EnvEntry> = std::env::vars_os()
        .map(|(key, value)| {
            (
                EnvEntry::map_key(key.clone()),
                EnvEntry {
                    is_from_base_env: true,
                    preferred_key: key,
                    value,
                },
            )
        })
        .collect();

    #[cfg(unix)]
    {
        let key = EnvEntry::map_key("SHELL".into());
        // Only set the value of SHELL if it isn't already set
        if !env.contains_key(&key) {
            env.insert(
                EnvEntry::map_key("SHELL".into()),
                EnvEntry {
                    is_from_base_env: true,
                    preferred_key: "SHELL".into(),
                    value: get_shell().into(),
                },
            );
        }
    }


    env
}

/// `CommandBuilder` is used to prepare a command to be spawned into a pty.
/// The interface is intentionally similar to that of `std::process::Command`.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde_support", derive(Serialize, Deserialize))]
pub struct CommandBuilder {
    args: Vec<OsString>,
    envs: BTreeMap<OsString, EnvEntry>,
    cwd: Option<OsString>,
    #[cfg(unix)]
    pub(crate) umask: Option<libc::mode_t>,
    controlling_tty: bool,
}

impl CommandBuilder {
    /// Create a new builder instance with argv\[0\] set to the specified
    /// program.
    pub fn new<S: AsRef<OsStr>>(program: S) -> Self {
        Self {
            args: vec![program.as_ref().to_owned()],
            envs: get_base_env(),
            cwd: None,
            #[cfg(unix)]
            umask: None,
            controlling_tty: true,
        }
    }

    /// Create a new builder instance from a pre-built argument vector
    pub fn from_argv(args: Vec<OsString>) -> Self {
        Self {
            args,
            envs: get_base_env(),
            cwd: None,
            #[cfg(unix)]
            umask: None,
            controlling_tty: true,
        }
    }

    /// Set whether we should set the pty as the controlling terminal.
    /// The default is true, which is usually what you want, but you
    /// may need to set this to false if you are crossing container
    /// boundaries (eg: flatpak) to workaround issues like:
    /// <https://github.com/flatpak/flatpak/issues/3697>
    /// <https://github.com/flatpak/flatpak/issues/3285>
    pub fn set_controlling_tty(&mut self, controlling_tty: bool) {
        self.controlling_tty = controlling_tty;
    }

    pub fn get_controlling_tty(&self) -> bool {
        self.controlling_tty
    }

    /// Create a new builder instance that will run some idea of a default
    /// program.  Such a builder will panic if `arg` is called on it.
    pub fn new_default_prog() -> Self {
        Self {
            args: vec![],
            envs: get_base_env(),
            cwd: None,
            #[cfg(unix)]
            umask: None,
            controlling_tty: true,
        }
    }

    /// Returns true if this builder was created via `new_default_prog`
    pub fn is_default_prog(&self) -> bool {
        self.args.is_empty()
    }

    /// Append an argument to the current command line.
    /// Will panic if called on a builder created via `new_default_prog`.
    pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) {
        if self.is_default_prog() {
            panic!("attempted to add args to a default_prog builder");
        }
        self.args.push(arg.as_ref().to_owned());
    }

    /// If a builder is_default_prog, then this function can be used to
    /// set the actual prog that should be used.
    /// This is intended to facilitate plumbing through the handling
    /// of the underlying default prog when merging together supplemental
    /// env and cwd information.
    /// You will not typically use this method in your own code.
    pub fn replace_default_prog(&mut self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) {
        if !self.is_default_prog() {
            panic!("attempted to replace_default_prog on a non-default_prog builder");
        }

        for arg in args {
            self.args.push(arg.as_ref().to_owned());
        }
    }

    /// Append a sequence of arguments to the current command line
    pub fn args<I, S>(&mut self, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self.arg(arg);
        }
    }

    pub fn get_argv(&self) -> &Vec<OsString> {
        &self.args
    }

    pub fn get_argv_mut(&mut self) -> &mut Vec<OsString> {
        &mut self.args
    }

    /// Override the value of an environmental variable
    pub fn env<K, V>(&mut self, key: K, value: V)
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        let key: OsString = key.as_ref().into();
        let value: OsString = value.as_ref().into();
        self.envs.insert(
            EnvEntry::map_key(key.clone()),
            EnvEntry {
                is_from_base_env: false,
                preferred_key: key,
                value,
            },
        );
    }

    pub fn env_remove<K>(&mut self, key: K)
    where
        K: AsRef<OsStr>,
    {
        let key = key.as_ref().into();
        self.envs.remove(&EnvEntry::map_key(key));
    }

    pub fn env_clear(&mut self) {
        self.envs.clear();
    }

    pub fn get_env<K>(&self, key: K) -> Option<&OsStr>
    where
        K: AsRef<OsStr>,
    {
        let key = key.as_ref().into();
        self.envs.get(&EnvEntry::map_key(key)).map(
            |EnvEntry {
                 is_from_base_env: _,
                 preferred_key: _,
                 value,
             }| value.as_os_str(),
        )
    }

    pub fn cwd<D>(&mut self, dir: D)
    where
        D: AsRef<OsStr>,
    {
        self.cwd = Some(dir.as_ref().to_owned());
    }

    pub fn clear_cwd(&mut self) {
        self.cwd.take();
    }

    pub fn get_cwd(&self) -> Option<&OsString> {
        self.cwd.as_ref()
    }

    /// Iterate over the configured environment. Only includes environment
    /// variables set by the caller via `env`, not variables set in the base
    /// environment.
    pub fn iter_extra_env_as_str(&self) -> impl Iterator<Item = (&str, &str)> {
        self.envs.values().filter_map(
            |EnvEntry {
                 is_from_base_env,
                 preferred_key,
                 value,
             }| {
                if *is_from_base_env {
                    None
                } else {
                    let key = preferred_key.to_str()?;
                    let value = value.to_str()?;
                    Some((key, value))
                }
            },
        )
    }

    pub fn iter_full_env_as_str(&self) -> impl Iterator<Item = (&str, &str)> {
        self.envs.values().filter_map(
            |EnvEntry {
                 preferred_key,
                 value,
                 ..
             }| {
                let key = preferred_key.to_str()?;
                let value = value.to_str()?;
                Some((key, value))
            },
        )
    }

    /// Return the configured command and arguments as a single string,
    /// quoted per the unix shell conventions.
    pub fn as_unix_command_line(&self) -> anyhow::Result<String> {
        let mut strs = vec![];
        for arg in &self.args {
            let s = arg
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("argument cannot be represented as utf8"))?;
            strs.push(s);
        }
        Ok(shell_words::join(strs))
    }
}

#[cfg(unix)]
impl CommandBuilder {
    pub fn umask(&mut self, mask: Option<libc::mode_t>) {
        self.umask = mask;
    }

    fn resolve_path(&self) -> Option<&OsStr> {
        self.get_env("PATH")
    }

    fn search_path(&self, exe: &OsStr, cwd: &OsStr) -> anyhow::Result<OsString> {
        use nix::unistd::{access, AccessFlags};

        let exe_path: &Path = exe.as_ref();
        if exe_path.is_relative() {
            let cwd: &Path = cwd.as_ref();
            let mut errors = vec![];

            // If the requested executable is explicitly relative to cwd,
            // then check only there.
            if is_cwd_relative_path(exe_path) {
                let abs_path = cwd.join(exe_path);

                if abs_path.is_dir() {
                    anyhow::bail!(
                        "Unable to spawn {} because it is a directory",
                        abs_path.display()
                    );
                } else if access(&abs_path, AccessFlags::X_OK).is_ok() {
                    return Ok(abs_path.into_os_string());
                } else if access(&abs_path, AccessFlags::F_OK).is_ok() {
                    anyhow::bail!(
                        "Unable to spawn {} because it is not executable",
                        abs_path.display()
                    );
                }

                anyhow::bail!(
                    "Unable to spawn {} because it does not exist",
                    abs_path.display()
                );
            }

            if let Some(path) = self.resolve_path() {
                for path in std::env::split_paths(&path) {
                    let candidate = cwd.join(&path).join(exe);

                    if candidate.is_dir() {
                        errors.push(format!("{} exists but is a directory", candidate.display()));
                    } else if access(&candidate, AccessFlags::X_OK).is_ok() {
                        return Ok(candidate.into_os_string());
                    } else if access(&candidate, AccessFlags::F_OK).is_ok() {
                        errors.push(format!(
                            "{} exists but is not executable",
                            candidate.display()
                        ));
                    }
                }
                errors.push(format!("No viable candidates found in PATH {path:?}"));
            } else {
                errors.push("Unable to resolve the PATH".to_string());
            }
            anyhow::bail!(
                "Unable to spawn {} because:\n{}",
                exe_path.display(),
                errors.join(".\n")
            );
        } else if exe_path.is_dir() {
            anyhow::bail!(
                "Unable to spawn {} because it is a directory",
                exe_path.display()
            );
        } else {
            if let Err(err) = access(exe_path, AccessFlags::X_OK) {
                if access(exe_path, AccessFlags::F_OK).is_ok() {
                    anyhow::bail!(
                        "Unable to spawn {} because it is not executable ({err:#})",
                        exe_path.display()
                    );
                } else {
                    anyhow::bail!(
                        "Unable to spawn {} because it doesn't exist on the filesystem ({err:#})",
                        exe_path.display()
                    );
                }
            }

            Ok(exe.to_owned())
        }
    }

    /// Convert the CommandBuilder to a `std::process::Command` instance.
    pub(crate) fn as_command(&self) -> anyhow::Result<std::process::Command> {
        use std::os::unix::process::CommandExt;

        let home = self.get_home_dir()?;
        let dir: &OsStr = self
            .cwd
            .as_deref()
            .filter(|dir| std::path::Path::new(dir).is_dir())
            .unwrap_or(home.as_ref());
        let shell = self.get_shell();

        let mut cmd = if self.is_default_prog() {
            let mut cmd = std::process::Command::new(&shell);

            // Run the shell as a login shell by prefixing the shell's
            // basename with `-` and setting that as argv0
            let basename = shell.rsplit('/').next().unwrap_or(&shell);
            cmd.arg0(format!("-{}", basename));
            cmd
        } else {
            let resolved = self.search_path(&self.args[0], dir)?;
            let mut cmd = std::process::Command::new(&resolved);
            cmd.arg0(&self.args[0]);
            cmd.args(&self.args[1..]);
            cmd
        };

        cmd.current_dir(dir);

        cmd.env_clear();
        cmd.env("SHELL", shell);
        cmd.envs(self.envs.values().map(
            |EnvEntry {
                 is_from_base_env: _,
                 preferred_key,
                 value,
             }| (preferred_key.as_os_str(), value.as_os_str()),
        ));

        Ok(cmd)
    }

    /// Determine which shell to run.
    /// We take the contents of the $SHELL env var first, then
    /// fall back to looking it up from the password database.
    pub fn get_shell(&self) -> String {
        use nix::unistd::{access, AccessFlags};

        if let Some(shell) = self.get_env("SHELL").and_then(OsStr::to_str) {
            match access(shell, AccessFlags::X_OK) {
                Ok(()) => return shell.into(),
                Err(err) => log::warn!(
                    "$SHELL -> {shell:?} which is \
                     not executable ({err:#}), falling back to password db lookup"
                ),
            }
        }

        get_shell()
    }

    fn get_home_dir(&self) -> anyhow::Result<String> {
        if let Some(home_dir) = self.get_env("HOME").and_then(OsStr::to_str) {
            return Ok(home_dir.into());
        }

        let ent = unsafe { libc::getpwuid(libc::getuid()) };
        if ent.is_null() {
            Ok("/".into())
        } else {
            use std::ffi::CStr;
            use std::str;
            let home = unsafe { CStr::from_ptr((*ent).pw_dir) };
            home.to_str()
                .map(str::to_owned)
                .context("failed to resolve home dir")
        }
    }
}


#[cfg(unix)]
/// Returns true if the path begins with `./` or `../`
fn is_cwd_relative_path<P: AsRef<Path>>(p: P) -> bool {
    matches!(
        p.as_ref().components().next(),
        Some(Component::CurDir | Component::ParentDir)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn test_cwd_relative() {
        assert!(is_cwd_relative_path("."));
        assert!(is_cwd_relative_path("./foo"));
        assert!(is_cwd_relative_path("../foo"));
        assert!(!is_cwd_relative_path("foo"));
        assert!(!is_cwd_relative_path("/foo"));
    }

    #[test]
    fn test_env() {
        let mut cmd = CommandBuilder::new("dummy");
        let package_authors = cmd.get_env("CARGO_PKG_AUTHORS");
        println!("package_authors: {:?}", package_authors);
        assert!(package_authors == Some(OsStr::new("Wez Furlong")));

        cmd.env("foo key", "foo value");
        cmd.env("bar key", "bar value");

        let iterated_envs = cmd.iter_extra_env_as_str().collect::<Vec<_>>();
        println!("iterated_envs: {:?}", iterated_envs);
        assert!(iterated_envs == vec![("bar key", "bar value"), ("foo key", "foo value")]);

        {
            let mut cmd = cmd.clone();
            cmd.env_remove("foo key");

            let iterated_envs = cmd.iter_extra_env_as_str().collect::<Vec<_>>();
            println!("iterated_envs: {:?}", iterated_envs);
            assert!(iterated_envs == vec![("bar key", "bar value")]);
        }

        {
            let mut cmd = cmd.clone();
            cmd.env_remove("bar key");

            let iterated_envs = cmd.iter_extra_env_as_str().collect::<Vec<_>>();
            println!("iterated_envs: {:?}", iterated_envs);
            assert!(iterated_envs == vec![("foo key", "foo value")]);
        }

        {
            let mut cmd = cmd.clone();
            cmd.env_clear();

            let iterated_envs = cmd.iter_extra_env_as_str().collect::<Vec<_>>();
            println!("iterated_envs: {:?}", iterated_envs);
            assert!(iterated_envs.is_empty());
        }
    }

}
