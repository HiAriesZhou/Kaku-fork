#![cfg(all(feature = "remote", unix))]

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

const ORIGINAL: &str = "local config = {}\nconfig.remote = { enabled = true }\nreturn config\n";

fn command(home: &Path) -> Command {
    let temp = home.join("tmp");
    fs::create_dir_all(&temp).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_kaku"));
    command
        .env("HOME", home)
        .env("TMPDIR", temp)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .env_remove("KAKU_CONFIG_FILE");
    command
}

fn default_config(home: &Path) -> PathBuf {
    let path = home.join(".config/kaku/kaku.lua");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, ORIGINAL).unwrap();
    path
}

#[test]
fn remote_off_updates_only_the_loaded_config() {
    for explicit in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let default = default_config(home.path());
        let selected = home.path().join("selected.lua");
        let inherited = home.path().join("inherited.lua");
        fs::write(&selected, ORIGINAL).unwrap();
        fs::write(&inherited, ORIGINAL).unwrap();

        // KAKU_CONFIG_FILE reports the loaded config. An inherited value
        // must not override either the default or an explicit CLI path.
        let mut cli = command(home.path());
        cli.env("KAKU_CONFIG_FILE", &inherited);
        if explicit {
            cli.arg("--config-file").arg(&selected);
        }
        let output = cli.args(["remote", "off"]).output().unwrap();
        assert!(output.status.success(), "{:?}", output);
        let (changed, untouched) = if explicit {
            (&selected, &default)
        } else {
            (&default, &selected)
        };
        assert_eq!(
            fs::read_to_string(changed).unwrap(),
            ORIGINAL.replace("true", "false")
        );
        assert_eq!(fs::read_to_string(untouched).unwrap(), ORIGINAL);
        assert_eq!(fs::read_to_string(inherited).unwrap(), ORIGINAL);
    }
}

#[test]
fn remote_off_keeps_the_config_symlink_and_target_permissions() {
    let home = tempfile::tempdir().unwrap();
    let config = default_config(home.path());
    let target = home.path().join("dotfiles.lua");
    fs::rename(&config, &target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&target, &config).unwrap();

    let output = command(home.path())
        .args(["remote", "off"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(fs::read_link(&config).unwrap(), target);
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        ORIGINAL.replace("true", "false")
    );
    assert_eq!(
        fs::metadata(target).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn remote_off_write_failure_keeps_the_original_config() {
    let home = tempfile::tempdir().unwrap();
    let config = default_config(home.path());
    let original = format!("-- {}\n{ORIGINAL}", "saved setting ".repeat(512));
    fs::write(&config, &original).unwrap();
    let target = home.path().join("dotfiles.lua");
    fs::rename(&config, &target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
    symlink(&target, &config).unwrap();
    let cli = command(home.path());
    // Limit only the child process. The write reaches the filesystem and
    // fails partway through, without changing the test runner's limits.
    let mut child = Command::new("/bin/sh");
    for (key, value) in cli.get_envs() {
        match value {
            Some(value) => {
                child.env(key, value);
            }
            None => {
                child.env_remove(key);
            }
        }
    }
    let output = child
        .args([
            "-c",
            "ulimit -f 1 || exit 125; trap '' XFSZ; exec \"$@\"",
            "remote-config-test",
        ])
        .arg(cli.get_program())
        .args(["remote", "off"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "write should fail: {output:?}"
    );
    assert_eq!(fs::read_link(&config).unwrap(), target);
    let remaining = fs::read_to_string(config).unwrap();
    assert_eq!(remaining.len(), original.len(), "config was truncated");
    assert_eq!(remaining, original);
    assert_eq!(
        fs::metadata(target).unwrap().permissions().mode() & 0o777,
        0o640
    );
}

#[test]
fn remote_errors_reach_the_terminal() {
    let home = tempfile::tempdir().unwrap();
    default_config(home.path());

    // Remote access is on in the config, but no Kaku has written its state.
    let output = command(home.path()).arg("remote").output().unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Kaku is not serving it right now"),
        "kaku remote failed silently: {:?}",
        output
    );

    let output = command(home.path())
        .args(["remote", "revoke", "missing"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no paired browser missing"),
        "{:?}",
        output
    );
}
