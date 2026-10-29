#![cfg(target_os = "macos")]
use super::*;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

impl From<u32> for LocalProcessStatus {
    fn from(s: u32) -> Self {
        match s {
            1 => Self::Idle,
            2 => Self::Run,
            3 => Self::Sleep,
            4 => Self::Stop,
            5 => Self::Zombie,
            _ => Self::Unknown,
        }
    }
}

impl LocalProcessInfo {
    pub fn current_working_dir(pid: u32) -> Option<PathBuf> {
        let mut pathinfo: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of_val(&pathinfo) as libc::c_int;
        let ret = unsafe {
            libc::proc_pidinfo(
                pid as _,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                &mut pathinfo as *mut _ as *mut _,
                size,
            )
        };
        if ret != size {
            return None;
        }

        // Workaround a workaround for an old rustc version supported by libc;
        // the type of vip_path should just be [c_char; MAXPATHLEN] but it
        // is defined as a horrible nested array by the libc crate:
        // `[[c_char; 32]; 32]`.
        // Urgh.  Let's re-cast it as the correct kind of slice.
        let vip_path = unsafe {
            std::slice::from_raw_parts(
                pathinfo.pvi_cdir.vip_path.as_ptr() as *const u8,
                libc::MAXPATHLEN as usize,
            )
        };
        let nul = vip_path.iter().position(|&c| c == 0)?;
        Some(OsStr::from_bytes(&vip_path[0..nul]).into())
    }

    pub fn executable_path(pid: u32) -> Option<PathBuf> {
        let mut buffer: Vec<u8> = Vec::with_capacity(libc::PROC_PIDPATHINFO_MAXSIZE as _);
        let x = unsafe {
            libc::proc_pidpath(
                pid as _,
                buffer.as_mut_ptr() as *mut _,
                libc::PROC_PIDPATHINFO_MAXSIZE as _,
            )
        };
        if x <= 0 {
            return None;
        }

        unsafe { buffer.set_len(x as usize) };
        let path: PathBuf = OsString::from_vec(buffer).into();
        Some(kaku_multicall_path(pid, path))
    }

    pub fn with_root_pid(pid: u32) -> Option<Self> {
        /// Enumerate all current process identifiers
        fn all_pids() -> Vec<libc::pid_t> {
            let num_pids = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
            if num_pids < 1 {
                return vec![];
            }

            // Give a bit of padding to avoid looping if processes are spawning
            // rapidly while we're trying to collect this info
            const PADDING: usize = 32;
            let mut pids: Vec<libc::pid_t> = Vec::with_capacity(num_pids as usize + PADDING);
            loop {
                let n = unsafe {
                    libc::proc_listallpids(
                        pids.as_mut_ptr() as *mut _,
                        (pids.capacity() * std::mem::size_of::<libc::pid_t>()) as _,
                    )
                };

                if n < 1 {
                    return vec![];
                }

                let n = n as usize;

                if n > pids.capacity() {
                    pids.reserve(n + PADDING);
                    continue;
                }

                unsafe { pids.set_len(n) };
                return pids;
            }
        }

        /// Obtain info block for a pid.
        /// Note that the process could have gone away since we first
        /// observed the pid and the time we call this, so we must
        /// be able to tolerate this failing.
        fn info_for_pid(pid: libc::pid_t) -> Option<libc::proc_bsdinfo> {
            let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
            let wanted_size = std::mem::size_of::<libc::proc_bsdinfo>() as _;
            let res = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTBSDINFO,
                    0,
                    &mut info as *mut _ as *mut _,
                    wanted_size,
                )
            };

            if res == wanted_size {
                Some(info)
            } else {
                None
            }
        }

        fn cwd_for_pid(pid: libc::pid_t) -> PathBuf {
            LocalProcessInfo::current_working_dir(pid as _).unwrap_or_else(PathBuf::new)
        }

        fn exe_for_pid(pid: libc::pid_t) -> PathBuf {
            LocalProcessInfo::executable_path(pid as _).unwrap_or_else(PathBuf::new)
        }

        let procs: Vec<_> = all_pids().into_iter().filter_map(info_for_pid).collect();

        fn build_proc(
            info: &libc::proc_bsdinfo,
            procs: &[libc::proc_bsdinfo],
            visited: &mut HashSet<u32>,
        ) -> LocalProcessInfo {
            let mut children = HashMap::new();

            for kid in procs {
                if kid.pbi_ppid == info.pbi_pid && !visited.contains(&kid.pbi_pid) {
                    visited.insert(kid.pbi_pid);
                    children.insert(kid.pbi_pid, build_proc(kid, procs, visited));
                }
            }

            let (executable, argv) = exe_and_args_for_pid_sysctl(info.pbi_pid as _)
                .unwrap_or_else(|| (exe_for_pid(info.pbi_pid as _), vec![]));

            let name = unsafe { std::ffi::CStr::from_ptr(info.pbi_comm.as_ptr() as _) };
            let name = name.to_str().unwrap_or("").to_string();

            LocalProcessInfo {
                pid: info.pbi_pid,
                ppid: info.pbi_ppid,
                pgid: info.pbi_pgid,
                name,
                executable,
                cwd: cwd_for_pid(info.pbi_pid as _),
                argv,
                start_time: info.pbi_start_tvsec,
                status: LocalProcessStatus::from(info.pbi_status),
                children,
            }
        }

        if let Some(info) = procs.iter().find(|info| info.pbi_pid == pid) {
            let mut visited = HashSet::new();
            visited.insert(pid);
            Some(build_proc(info, &procs, &mut visited))
        } else {
            None
        }
    }
}

fn exe_and_args_for_pid_sysctl(pid: libc::pid_t) -> Option<(PathBuf, Vec<String>)> {
    use libc::c_int;
    let mut size = 64 * 1024;
    let mut buf: Vec<u8> = Vec::with_capacity(size);
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as c_int];

    let res = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as _,
            buf.as_mut_ptr() as *mut _,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if res == -1 {
        return None;
    }
    if size < (std::mem::size_of::<c_int>() * 2) {
        // Not big enough
        return None;
    }
    unsafe { buf.set_len(size) };

    parse_exe_and_argv_sysctl(buf)
}

/// Kaku's bundle ships one binary: `Contents/MacOS/kaku` and
/// `Contents/MacOS/k` are symlinks to `kaku-gui`, which runs the CLI or the
/// chat CLI by the name it was started under (`multicall_entry` in
/// `kaku-gui/src/main.rs`, whose rule this mirrors). proc_pidpath reports the
/// resolved file, so without this a running `kaku config` would show up as
/// `kaku-gui` to every foreground-process consumer (#583's opaque TUI
/// background, title fallbacks, Lua). Only that one binary is mapped; other
/// programs keep their resolved paths.
fn kaku_multicall_path(pid: u32, resolved: PathBuf) -> PathBuf {
    if resolved.file_name() != Some(OsStr::new("kaku-gui")) {
        return resolved;
    }
    // Unreadable arguments (another user's process, a zombie) keep the
    // resolved path rather than guessing the CLI.
    let Some((_, argv)) = exe_and_args_for_pid_sysctl(pid as _) else {
        return resolved;
    };
    match multicall_name(argv.first().map(String::as_str).unwrap_or_default()) {
        Some(name) => resolved.with_file_name(name),
        None => resolved,
    }
}

/// The bundle entry a `kaku-gui` process started as `argv0` runs: `None` for
/// the GUI itself.
fn multicall_name(argv0: &str) -> Option<&'static str> {
    let name = std::path::Path::new(argv0)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if name.eq_ignore_ascii_case("kaku-gui") {
        None
    } else if name.eq_ignore_ascii_case("k") {
        Some("k")
    } else {
        Some("kaku")
    }
}

fn parse_exe_and_argv_sysctl(buf: Vec<u8>) -> Option<(PathBuf, Vec<String>)> {
    use libc::c_int;

    // The data in our buffer is laid out like this:
    // argc - c_int
    // exe_path - NUL terminated string
    // argv[0] - NUL terminated string
    // argv[1] - NUL terminated string
    // ...
    // argv[n] - NUL terminated string
    // envp[0] - NUL terminated string
    // ...

    let mut ptr = &buf[0..buf.len()];

    let argc: c_int = unsafe { std::ptr::read(ptr.as_ptr() as *const c_int) };
    ptr = &ptr[std::mem::size_of::<c_int>()..];

    fn consume_cstr(ptr: &mut &[u8]) -> Option<String> {
        // Parse to the end of a null terminated string
        let nul = ptr.iter().position(|&c| c == 0)?;
        let s = String::from_utf8_lossy(&ptr[0..nul]).to_owned().to_string();
        *ptr = ptr.get(nul + 1..)?;

        // Find the position of the first non null byte. `.position()`
        // will return None if we run off the end.
        if let Some(not_nul) = ptr.iter().position(|&c| c != 0) {
            // If there are no trailing nulls, not_nul will be 0
            // and this call will be a noop
            *ptr = ptr.get(not_nul..)?;
        }

        Some(s)
    }

    let exe_path = consume_cstr(&mut ptr)?.into();

    let mut args = vec![];
    for _ in 0..argc {
        args.push(consume_cstr(&mut ptr)?);
    }

    Some((exe_path, args))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{multicall_name, parse_exe_and_argv_sysctl};

    #[test]
    fn multicall_name_matches_the_bundle_dispatch() {
        assert_eq!(
            multicall_name("/Applications/Kaku.app/Contents/MacOS/kaku-gui"),
            None
        );
        assert_eq!(multicall_name("/opt/homebrew/bin/kaku"), Some("kaku"));
        assert_eq!(multicall_name("Kaku"), Some("kaku"));
        assert_eq!(
            multicall_name("/Applications/Kaku.app/Contents/MacOS/k"),
            Some("k")
        );
        assert_eq!(multicall_name("/usr/local/bin/wezterm"), Some("kaku"));
        assert_eq!(multicall_name(""), Some("kaku"));
    }

    #[test]
    fn executable_path_reports_the_bundle_name_a_kaku_gui_process_started_as() {
        // A stand-in bundle: any binary named kaku-gui, linked as kaku and k
        // the way build.sh links them.
        let dir = tempfile::tempdir().unwrap();
        let gui = dir.path().join("kaku-gui");
        std::fs::copy("/bin/sleep", &gui).unwrap();
        for name in ["kaku", "k"] {
            std::os::unix::fs::symlink("kaku-gui", dir.path().join(name)).unwrap();
        }
        let resolved = std::fs::canonicalize(&gui).unwrap();
        for (started_as, expected) in [("kaku", "kaku"), ("k", "k"), ("kaku-gui", "kaku-gui")] {
            let mut child = std::process::Command::new(dir.path().join(started_as))
                .arg("5")
                .spawn()
                .unwrap();
            // Command::spawn uses posix_spawn here, which returns after exec.
            let path = super::LocalProcessInfo::executable_path(child.id());
            let _ = child.kill();
            let _ = child.wait();
            assert_eq!(
                path,
                Some(resolved.with_file_name(expected)),
                "{started_as}"
            );
        }
    }

    #[test]
    fn test_trailing_zeros() {
        // Example data generated from running 'sleep 5' on the commit author's local machine,
        let buf = vec![
            2, 0, 0, 0, 47, 98, 105, 110, 47, 115, 108, 101, 101, 112, 0, 0, 0, 0, 0, 0, 115, 108,
            101, 101, 112, 0, 53, 0,
        ];

        let (exe_path, argv) = parse_exe_and_argv_sysctl(buf).unwrap();

        assert_eq!(exe_path, Path::new("/bin/sleep").to_path_buf());
        assert_eq!(argv, vec!["sleep".to_string(), "5".to_string()]);
    }

    #[test]
    fn test_no_trailing_zeros() {
        // Example data generated from running 'sleep 5' on the commit author's local machine,
        // then modified to remove the trailing 0s between the exe_path and the argv
        let buf = vec![
            2, 0, 0, 0, 47, 98, 105, 110, 47, 115, 108, 101, 101, 112, 0, 115, 108, 101, 101, 112,
            0, 53, 0,
        ];

        let (exe_path, argv) = parse_exe_and_argv_sysctl(buf).unwrap();

        assert_eq!(exe_path, Path::new("/bin/sleep").to_path_buf());
        assert_eq!(argv, vec!["sleep".to_string(), "5".to_string()]);
    }

    #[test]
    fn test_multiple_trailing_zeros() {
        // Example data generated from running 'sleep 5' on the commit author's local machine,
        // then modified to add trailing 0s between argv items
        let buf = vec![
            2, 0, 0, 0, 47, 98, 105, 110, 47, 115, 108, 101, 101, 112, 0, 0, 0, 115, 108, 101, 101,
            112, 0, 0, 0, 53, 0,
        ];

        let (exe_path, argv) = parse_exe_and_argv_sysctl(buf).unwrap();

        assert_eq!(exe_path, Path::new("/bin/sleep").to_path_buf());
        assert_eq!(argv, vec!["sleep".to_string(), "5".to_string()]);
    }

    #[test]
    fn test_trailing_zeros_at_end() {
        // Example data generated from running 'sleep 5' on the commit author's local machine,
        // then modified to add zeroes to the end of the buffer
        let buf = vec![
            2, 0, 0, 0, 47, 98, 105, 110, 47, 115, 108, 101, 101, 112, 0, 0, 0, 115, 108, 101, 101,
            112, 0, 0, 0, 53, 0, 0, 0, 0, 0,
        ];

        let (exe_path, argv) = parse_exe_and_argv_sysctl(buf).unwrap();

        assert_eq!(exe_path, Path::new("/bin/sleep").to_path_buf());
        assert_eq!(argv, vec!["sleep".to_string(), "5".to_string()]);
    }

    #[test]
    fn test_malformed() {
        // Example data generated from running 'sleep 5' on the commit author's local machine,
        // then modified to remove the last 0, making a malformed null-terminated string
        let buf = vec![
            2, 0, 0, 0, 47, 98, 105, 110, 47, 115, 108, 101, 101, 112, 0, 0, 0, 115, 108, 101, 101,
            112, 0, 0, 0, 53,
        ];

        assert!(parse_exe_and_argv_sysctl(buf).is_none());
    }
}
