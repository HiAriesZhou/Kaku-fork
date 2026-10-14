// Portions of this file are derived from code that is
// Copyright © 2015 Sebastian Thiel
// <https://github.com/Byron/open-rs>

pub fn open_url(url: &str) {
    let url = url.to_string();
    std::thread::spawn(move || {
        #[cfg(target_os = "macos")]
        let candidates: &[&[&str]] = &[&["/usr/bin/open", &url]];

        for candidate in candidates {
            let mut cmd = std::process::Command::new(candidate[0]);
            cmd.args(&candidate[1..]);

            if let Ok(status) = cmd.status() {
                if status.success() {
                    return;
                }
            }
        }
    });
}

pub fn open_with(url: &str, app: &str) {
    let url = url.to_string();
    let app = app.to_string();

    std::thread::spawn(move || {
        #[cfg(target_os = "macos")]
        let args: &[&str] = &["/usr/bin/open", "-a", &app, &url];

        let mut cmd = std::process::Command::new(args[0]);
        cmd.args(&args[1..]);

        if let Ok(status) = cmd.status() {
            if status.success() {}
        }
    });
}
