//! Support bundle written at the end of every `kaku doctor` run.
//!
//! Collectors run in parallel so the whole bundle costs about as long as the
//! slowest one (the 2 second `sample` of a running Kaku). Every external
//! command has a timeout, large files are summarized by size instead of
//! copied, and everything that lands in the zip goes through `redact` first.
//! A collector that fails leaves a line in `README.txt` instead of a file.

use regex::Regex;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

const BUNDLE_NAME: &str = "Kaku-Diagnose";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const SAMPLE_SECONDS: &str = "2";
const LOG_FILES: usize = 5;
const LOG_TAIL_BYTES: u64 = 1024 * 1024;
const SYSTEM_LOG_TAIL_BYTES: u64 = 2 * 1024 * 1024;
const CRASH_REPORTS: usize = 5;
const CRASH_REPORT_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Written by the GUI stall watchdog (`kaku-gui/src/stall_watchdog.rs`).
const HANG_CAPTURE_PREFIX: &str = "kaku-hang-";

/// Collect the bundle and return the zip path. `doctor_report` is the text
/// the user just saw, so the bundle and the terminal agree.
pub fn write_bundle(doctor_report: &str) -> anyhow::Result<PathBuf> {
    let root = config::RUNTIME_DIR.join("diagnostics");
    let work = root.join(BUNDLE_NAME);
    let zip = root.join(format!("{BUNDLE_NAME}.zip"));
    if work.exists() {
        fs::remove_dir_all(&work)?;
    }
    config::create_user_owned_dirs(&work)?;

    let redactor = Redactor::for_this_machine();
    let bundle = Bundle {
        dir: work.clone(),
        redactor,
        notes: Mutex::new(Vec::new()),
    };
    bundle.write("doctor.txt", doctor_report.as_bytes());

    let gui_pids = kaku_gui_pids();
    std::thread::scope(|s| {
        s.spawn(|| collect_environment(&bundle));
        s.spawn(|| collect_system_log(&bundle));
        s.spawn(|| collect_kaku_logs(&bundle));
        s.spawn(|| collect_crash_reports(&bundle));
        s.spawn(|| collect_config(&bundle));
        for &pid in &gui_pids {
            let bundle = &bundle;
            s.spawn(move || collect_process(bundle, pid));
        }
    });
    if gui_pids.is_empty() {
        bundle.note("Kaku was not running, so no live sample was taken");
    }
    bundle.write_readme();

    if zip.exists() {
        fs::remove_file(&zip)?;
    }
    let status = Command::new("/usr/bin/ditto")
        .args(["-c", "-k", "--keepParent"])
        .arg(&work)
        .arg(&zip)
        .status()?;
    anyhow::ensure!(status.success(), "ditto exited with {status}");
    let _ = fs::remove_dir_all(&work);
    Ok(zip)
}

struct Bundle {
    dir: PathBuf,
    redactor: Redactor,
    notes: Mutex<Vec<String>>,
}

impl Bundle {
    fn write(&self, name: &str, bytes: &[u8]) {
        let path = self.dir.join(name);
        if let Some(parent) = path.parent() {
            if let Err(err) = fs::create_dir_all(parent) {
                self.note(format!("{name}: {err}"));
                return;
            }
        }
        let text = self.redactor.redact(&String::from_utf8_lossy(bytes));
        if let Err(err) = fs::write(&path, text) {
            self.note(format!("{name}: {err}"));
        }
    }

    fn write_command(&self, name: &str, program: &str, args: &[&str]) {
        match run_with_timeout(program, args, COMMAND_TIMEOUT) {
            Ok(out) => self.write(name, &out),
            Err(err) => self.note(format!("{name}: {err:#}")),
        }
    }

    fn note(&self, line: impl Into<String>) {
        self.notes.lock().unwrap().push(line.into());
    }

    fn write_readme(&self) {
        let mut text = String::from(
            "Kaku diagnostic bundle. Home paths, host name, MAC addresses, serial\n\
             numbers and credential-looking values are replaced before writing.\n\
             assistant.toml is never collected.\n",
        );
        let notes = self.notes.lock().unwrap();
        if !notes.is_empty() {
            text.push_str("\nNot collected:\n");
            for line in notes.iter() {
                text.push_str("- ");
                text.push_str(line);
                text.push('\n');
            }
        }
        drop(notes);
        self.write("README.txt", text.as_bytes());
    }
}

fn collect_environment(bundle: &Bundle) {
    let mut text = format!(
        "Kaku: {}\nApp: {}\n",
        crate::doctor::doctor_version_string(),
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    );
    for (label, program, args) in [
        ("sw_vers", "/usr/bin/sw_vers", &[][..]),
        (
            "sysctl",
            "/usr/sbin/sysctl",
            &["hw.model", "machdep.cpu.brand_string", "hw.memsize"][..],
        ),
        ("memory_pressure", "/usr/bin/memory_pressure", &["-Q"][..]),
        (
            "displays",
            "/usr/sbin/system_profiler",
            &["SPDisplaysDataType", "-detailLevel", "mini"][..],
        ),
    ] {
        text.push_str(&format!("\n== {label}\n"));
        match run_with_timeout(program, args, COMMAND_TIMEOUT) {
            Ok(out) => text.push_str(&String::from_utf8_lossy(&out)),
            Err(err) => text.push_str(&format!("failed: {err:#}\n")),
        }
    }
    bundle.write("environment.txt", text.as_bytes());
}

fn collect_process(bundle: &Bundle, pid: u32) {
    let pid = pid.to_string();
    std::thread::scope(|s| {
        s.spawn(|| {
            let sample_path = bundle.dir.join(format!("sample-{pid}.txt"));
            let args = [pid.as_str(), SAMPLE_SECONDS, "-mayDie", "-file"];
            let mut all: Vec<&str> = args.to_vec();
            let path_str = sample_path.to_string_lossy().to_string();
            all.push(&path_str);
            match run_with_timeout("/usr/bin/sample", &all, COMMAND_TIMEOUT) {
                Ok(_) => match fs::read(&sample_path) {
                    Ok(bytes) => bundle.write(&format!("sample-{pid}.txt"), &bytes),
                    Err(err) => bundle.note(format!("sample-{pid}.txt: {err}")),
                },
                Err(err) => bundle.note(format!("sample-{pid}.txt: {err:#}")),
            }
        });
        s.spawn(|| {
            bundle.write_command(
                &format!("footprint-{pid}.txt"),
                "/usr/bin/footprint",
                &["--pid", pid.as_str()],
            )
        });
    });
}

fn collect_system_log(bundle: &Bundle) {
    match run_with_timeout(
        "/usr/bin/log",
        &[
            "show",
            "--last",
            "10m",
            "--style",
            "compact",
            "--predicate",
            "process == \"kaku-gui\"",
        ],
        COMMAND_TIMEOUT,
    ) {
        Ok(out) => bundle.write("system-log.txt", tail(&out, SYSTEM_LOG_TAIL_BYTES)),
        Err(err) => bundle.note(format!("system-log.txt: {err:#}")),
    }
}

fn collect_kaku_logs(bundle: &Bundle) {
    let dir = config::RUNTIME_DIR.as_path();
    for path in newest_files(
        dir,
        |n| n.starts_with("kaku-gui-log-") && n.ends_with(".txt"),
        LOG_FILES,
    ) {
        copy_tail(bundle, &path, "logs", LOG_TAIL_BYTES);
    }
    for path in newest_files(
        dir,
        |n| n.starts_with(HANG_CAPTURE_PREFIX) && n.ends_with(".txt"),
        usize::MAX,
    ) {
        copy_tail(bundle, &path, "hangs", LOG_TAIL_BYTES);
    }
}

fn collect_crash_reports(bundle: &Bundle) {
    let dir = config::HOME_DIR.join("Library/Logs/DiagnosticReports");
    let cutoff = SystemTime::now().checked_sub(CRASH_REPORT_MAX_AGE);
    let reports = newest_files(
        &dir,
        |n| (n.starts_with("Kaku") || n.starts_with("kaku")) && n.ends_with(".ips"),
        CRASH_REPORTS,
    );
    for path in reports {
        let recent = match (cutoff, modified(&path)) {
            (Some(cutoff), Some(at)) => at >= cutoff,
            _ => true,
        };
        if recent {
            copy_tail(bundle, &path, "crashes", LOG_TAIL_BYTES);
        }
    }
}

fn collect_config(bundle: &Bundle) {
    let config_file = config::user_config_path();
    match fs::read(&config_file) {
        Ok(bytes) => bundle.write("config/kaku.lua", &bytes),
        Err(err) => bundle.note(format!("config/kaku.lua: {err}")),
    }
    let Some(config_dir) = config_file.parent() else {
        return;
    };
    if let Ok(bytes) = fs::read(config_dir.join("state.json")) {
        bundle.write("config/state.json", &bytes);
    }
    // Names and sizes only: session content can be gigabytes (#559) and the
    // conversation history is private.
    let mut listing = String::new();
    list_sizes(config_dir, config_dir, &mut listing, 0);
    bundle.write("config/files.txt", listing.as_bytes());
}

fn list_sizes(root: &Path, dir: &Path, out: &mut String, depth: usize) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for path in paths {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => {
                out.push_str(&format!("{rel}/\n"));
                list_sizes(root, &path, out, depth + 1);
            }
            Ok(meta) => out.push_str(&format!("{rel}\t{}\n", meta.len())),
            Err(_) => {}
        }
    }
}

fn copy_tail(bundle: &Bundle, path: &Path, folder: &str, max: u64) {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let dest = format!("{folder}/{name}");
    match read_tail(path, max) {
        Ok(bytes) => bundle.write(&dest, &bytes),
        Err(err) => bundle.note(format!("{dest}: {err}")),
    }
}

fn read_tail(path: &Path, max: u64) -> std::io::Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len > max {
        file.seek(SeekFrom::Start(len - max))?;
    }
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

fn tail(bytes: &[u8], max: u64) -> &[u8] {
    let max = max as usize;
    if bytes.len() > max {
        &bytes[bytes.len() - max..]
    } else {
        bytes
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn newest_files(dir: &Path, keep_name: impl Fn(&str) -> bool, limit: usize) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(&keep_name)
        })
        .filter_map(|p| modified(&p).map(|at| (at, p)))
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    files.into_iter().take(limit).map(|(_, p)| p).collect()
}

fn kaku_gui_pids() -> Vec<u32> {
    run_with_timeout("/usr/bin/pgrep", &["-x", "kaku-gui"], COMMAND_TIMEOUT)
        .map(|out| {
            String::from_utf8_lossy(&out)
                .lines()
                .filter_map(|l| l.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Run a command with stdout captured to a temp file, killing it at the
/// deadline so one stuck tool cannot hold up the bundle.
fn run_with_timeout(program: &str, args: &[&str], timeout: Duration) -> anyhow::Result<Vec<u8>> {
    let mut out = tempfile::tempfile()?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            out.seek(SeekFrom::Start(0))?;
            let mut buf = Vec::new();
            out.read_to_end(&mut buf)?;
            if !status.success() && buf.is_empty() {
                anyhow::bail!("{program} exited with {status}");
            }
            return Ok(buf);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("{program} timed out after {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

struct Redactor {
    home: String,
    host: Option<String>,
}

static MAC_ADDRESS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:[0-9a-f]{2}:){5}[0-9a-f]{2}\b").unwrap());
static SERIAL_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?im)(serial[ _-]?(?:number)?[^:=\n]*[:=]\s*)\S+").unwrap());
static SECRET_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)((?:api[_-]?key|token|secret|password|passwd)[A-Za-z0-9_]*\s*[:=]\s*)(['"]?)[^'"\s,}]+(['"]?)"#)
        .unwrap()
});
static URL_USERINFO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\b([A-Za-z][A-Za-z0-9+.-]*://)[^/?#\s'"]+@"#).unwrap());
static TOKEN_LITERAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:sk-[A-Za-z0-9_-]{16,}|gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|xox[abprs]-[A-Za-z0-9-]{10,})|(?i:bearer\s+)[A-Za-z0-9._-]{16,}")
        .unwrap()
});

impl Redactor {
    fn for_this_machine() -> Self {
        Self {
            home: config::HOME_DIR.display().to_string(),
            host: hostname::get()
                .ok()
                .and_then(|h| h.into_string().ok())
                .map(|h| h.trim_end_matches(".local").to_string())
                .filter(|h| h.len() >= 3),
        }
    }

    fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        if self.home.len() > 1 {
            out = out.replace(&self.home, "~");
            out = out.replace(&self.home.replace('/', "\\/"), "~");
        }
        if let Some(host) = &self.host {
            out = out.replace(host.as_str(), "<host>");
        }
        out = MAC_ADDRESS.replace_all(&out, "<mac>").into_owned();
        out = SERIAL_NUMBER.replace_all(&out, "${1}<serial>").into_owned();
        out = SECRET_ASSIGNMENT
            .replace_all(&out, "${1}${2}<redacted>${3}")
            .into_owned();
        out = URL_USERINFO
            .replace_all(&out, "${1}<redacted>@")
            .into_owned();
        TOKEN_LITERAL.replace_all(&out, "<redacted>").into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redactor() -> Redactor {
        Redactor {
            home: "/Users/alice".into(),
            host: Some("alices-mbp".into()),
        }
    }

    #[test]
    fn redacts_every_identifier_class() {
        let input = "\
cwd /Users/alice/code and json \"\\/Users\\/alice\\/x\"
host alices-mbp.local
en0 a4:83:e7:1b:2c:3d
Serial Number (system): C02XK1ABCDEF
config.api_key = \"sk-live-abcdefghijklmnopqrstuv\"
set_environment_variables = { GITHUB_TOKEN = 'ghp_abcdefghijklmnopqrstuvwxyz0123' }
Authorization: Bearer abcdefghijklmnopqrstuvwxyz
";
        let out = redactor().redact(input);
        for leaked in [
            "/Users/alice",
            "\\/Users\\/alice",
            "alices-mbp",
            "a4:83:e7:1b:2c:3d",
            "C02XK1ABCDEF",
            "sk-live-abcdefghijklmnopqrstuv",
            "ghp_abcdefghijklmnopqrstuvwxyz0123",
            "abcdefghijklmnopqrstuvwxyz",
        ] {
            assert!(!out.contains(leaked), "leaked {:?} in:\n{}", leaked, out);
        }
        assert!(out.contains("cwd ~/code"));
        assert!(out.contains("config.api_key = \"<redacted>\""));
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        let input = "Kaku 0.21.0 on macOS 26.1, font_size = 17, line_height = 1.28\n";
        assert_eq!(redactor().redact(input), input);
    }

    #[test]
    fn redacts_url_userinfo_and_keeps_the_host() {
        let input = "https_proxy = \"http://alice:Hunter2pw@proxy:3128\"\n";
        let out = redactor().redact(input);
        assert!(!out.contains("Hunter2pw"), "leaked password in:\n{}", out);
        assert!(!out.contains("alice:"), "leaked user in:\n{}", out);
        assert!(out.contains("proxy:3128"), "lost host in:\n{}", out);
    }

    #[test]
    fn redacts_fine_grained_github_tokens() {
        let token = "github_pat_11ABCDEFG0aBcDeFgHiJkLmN_oPqRsTuVwXyZ0123456789";
        let input = format!("credential {token} stored\n");
        let out = redactor().redact(&input);
        assert!(!out.contains(token), "leaked token in:\n{}", out);
        assert!(
            !out.contains("11ABCDEFG0aBcDeFgHiJkLmN"),
            "leaked token in:\n{}",
            out
        );
    }

    #[test]
    fn leaves_urls_without_credentials_alone() {
        for input in [
            "see https://example.com/x for details\n",
            "remote origin git@github.com:o/r.git (fetch)\n",
        ] {
            assert_eq!(redactor().redact(input), input);
        }
    }

    #[test]
    fn redacts_url_userinfo_whose_user_name_contains_an_at_sign() {
        let input = "https_proxy = \"http://alice@example.com:Hunter2pw@proxy.local:3128\"\n";
        let out = redactor().redact(input);
        assert!(!out.contains("Hunter2pw"), "leaked password in:\n{}", out);
        assert!(
            !out.contains("alice@example.com"),
            "leaked user in:\n{}",
            out
        );
    }

    #[test]
    fn keeps_the_host_after_an_at_sign_user_name_and_leaves_path_at_signs_alone() {
        let input = "https_proxy = \"http://alice@example.com:Hunter2pw@proxy.local:3128\"\n";
        let out = redactor().redact(input);
        assert!(out.contains("proxy.local:3128"), "lost host in:\n{}", out);
        for input in [
            "see https://example.com/a@b for details\n",
            "remote origin git@github.com:o/r.git (fetch)\n",
        ] {
            assert_eq!(redactor().redact(input), input);
        }
    }

    #[test]
    fn tail_keeps_the_end_of_large_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        fs::write(&path, "0123456789").unwrap();
        assert_eq!(read_tail(&path, 4).unwrap(), b"6789");
        assert_eq!(read_tail(&path, 100).unwrap(), b"0123456789");
        assert_eq!(tail(b"0123456789", 3), b"789");
    }

    #[test]
    fn newest_files_filters_and_limits() {
        let dir = tempfile::tempdir().unwrap();
        for (i, name) in [
            "kaku-gui-log-1.txt",
            "kaku-gui-log-2.txt",
            "other.txt",
            "kaku-gui-log-3.txt",
        ]
        .iter()
        .enumerate()
        {
            let path = dir.path().join(name);
            fs::write(&path, "x").unwrap();
            let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000 + i as u64);
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(at)
                .unwrap();
        }
        let picked: Vec<String> = newest_files(dir.path(), |n| n.starts_with("kaku-gui-log-"), 2)
            .into_iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(picked, vec!["kaku-gui-log-3.txt", "kaku-gui-log-2.txt"]);
    }

    #[test]
    fn slow_commands_time_out() {
        let started = Instant::now();
        let err = run_with_timeout("/bin/sleep", &["5"], Duration::from_millis(200)).unwrap_err();
        assert_eq!(err.to_string(), "/bin/sleep timed out after 200ms");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            run_with_timeout("/bin/echo", &["ok"], COMMAND_TIMEOUT).unwrap(),
            b"ok\n"
        );
    }
}
