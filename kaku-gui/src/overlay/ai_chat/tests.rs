//! Tests for the Cmd+L AI chat overlay.
//!
//! Hosted in this sibling file so the ~5k-line `mod.rs` stays focused on
//! production code. The two submodules are kept (markdown / inline rendering
//! vs input-undo) to mirror the two distinct concerns; both reach into the
//! parent module's items via `super::super`.

#[cfg(test)]
mod markdown_tests {
    use super::super::*;
    use crate::ai_chat_engine::approval::approval_summary;

    fn test_palette() -> ChatPalette {
        ChatPalette {
            bg: SrgbaTuple::default(),
            fg: SrgbaTuple::default(),
            accent: SrgbaTuple::default(),
            border: SrgbaTuple::default(),
            user_header: SrgbaTuple::default(),
            user_text: SrgbaTuple::default(),
            ai_text: SrgbaTuple::default(),
            selection_fg: SrgbaTuple::default(),
            selection_bg: SrgbaTuple::default(),
        }
    }

    fn light_palette() -> ChatPalette {
        ChatPalette {
            bg: SrgbaTuple(1.0, 0.99, 0.94, 1.0),
            fg: SrgbaTuple(0.25, 0.24, 0.23, 1.0),
            accent: SrgbaTuple(0.0, 1.0, 1.0, 1.0),
            border: SrgbaTuple(0.45, 0.43, 0.40, 1.0),
            user_header: SrgbaTuple(0.8, 0.6, 0.0, 1.0),
            user_text: SrgbaTuple(0.25, 0.24, 0.23, 1.0),
            ai_text: SrgbaTuple(0.25, 0.24, 0.23, 1.0),
            selection_fg: SrgbaTuple(1.0, 1.0, 1.0, 1.0),
            selection_bg: SrgbaTuple(0.2, 0.4, 0.8, 1.0),
        }
    }

    pub(super) fn test_context() -> TerminalContext {
        TerminalContext {
            cwd: "/tmp".to_string(),
            remote_host: None,
            visible_lines: vec!["line 1".to_string()],
            tab_snapshot: "cargo test\nerror: boom".to_string(),
            selected_text: "selected snippet".to_string(),
            colors: test_palette(),
            panel_cols: 80,
            panel_rows: 24,
            last_exit_code: None,
            last_command_output: None,
        }
    }

    fn plain(text: &str) -> InlineSpan {
        InlineSpan {
            text: text.to_string(),
            style: InlineStyle::Plain,
        }
    }
    fn bold(text: &str) -> InlineSpan {
        InlineSpan {
            text: text.to_string(),
            style: InlineStyle::Bold,
        }
    }
    fn italic(text: &str) -> InlineSpan {
        InlineSpan {
            text: text.to_string(),
            style: InlineStyle::Italic,
        }
    }
    fn code(text: &str) -> InlineSpan {
        InlineSpan {
            text: text.to_string(),
            style: InlineStyle::Code,
        }
    }

    fn assert_spans(got: Vec<InlineSpan>, want: Vec<InlineSpan>) {
        assert_eq!(
            got.len(),
            want.len(),
            "span count mismatch: {:?} vs {:?}",
            got,
            want
        );
        for (g, w) in got.iter().zip(want.iter()) {
            assert_eq!(g.style, w.style, "style mismatch: {:?} vs {:?}", g, w);
            assert_eq!(g.text, w.text, "text mismatch: {:?} vs {:?}", g, w);
        }
    }

    #[test]
    fn inline_bold_basic() {
        assert_spans(
            tokenize_inline("hello **world** end"),
            vec![plain("hello "), bold("world"), plain(" end")],
        );
    }

    #[test]
    fn inline_bold_underscores() {
        assert_spans(tokenize_inline("__ok__"), vec![bold("ok")]);
    }

    #[test]
    fn inline_italic_single_star() {
        assert_spans(
            tokenize_inline("an *emph* word"),
            vec![plain("an "), italic("emph"), plain(" word")],
        );
    }

    #[test]
    fn inline_italic_ignores_leading_space() {
        // "* not emphasis" (* followed by space) should stay plain.
        let out = tokenize_inline("a * b * c");
        let joined: String = out.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, "a * b * c");
        assert!(out.iter().all(|s| s.style == InlineStyle::Plain));
    }

    #[test]
    fn inline_code_span() {
        assert_spans(
            tokenize_inline("run `ls -la` now"),
            vec![plain("run "), code("ls -la"), plain(" now")],
        );
    }

    #[test]
    fn inline_strike_strips_markers() {
        assert_spans(tokenize_inline("~~gone~~"), vec![plain("gone")]);
    }

    #[test]
    fn inline_link_keeps_label() {
        assert_spans(
            tokenize_inline("see [docs](http://x)"),
            vec![plain("see docs")],
        );
    }

    #[test]
    fn inline_unclosed_bold_is_literal() {
        assert_spans(tokenize_inline("start **open"), vec![plain("start **open")]);
    }

    #[test]
    fn inline_preserves_snake_case() {
        // Underscore-flanked words must not become italic.
        assert_spans(
            tokenize_inline("call my_var here"),
            vec![plain("call my_var here")],
        );
    }

    #[test]
    fn block_heading_levels() {
        let blocks = parse_markdown_blocks("# Top\n## Mid\n### Low\n#### Tiny");
        let levels: Vec<u8> = blocks
            .iter()
            .filter_map(|b| match b {
                MdBlock::Heading { level, .. } => Some(*level),
                _ => None,
            })
            .collect();
        assert_eq!(levels, vec![1, 2, 3, 4]);
    }

    #[test]
    fn heading_marker_prefixes_body_text() {
        let mut segs = vec![InlineSpan {
            text: "## ".to_string(),
            style: InlineStyle::HeadingMarker,
        }];
        segs.extend(tokenize_inline("Section"));
        assert_eq!(segs[0].style, InlineStyle::HeadingMarker);
        assert_eq!(segs[0].text, "## ");
        assert_eq!(segs[1].text, "Section");
    }

    #[test]
    fn light_theme_heading_text_falls_back_to_foreground() {
        let pal = light_palette();
        assert!(pal.is_light());
        let marker = pal.heading_marker_cell();
        let text = pal.heading_text_cell();
        assert_ne!(
            format!("{:?}", marker.foreground()),
            format!("{:?}", text.foreground()),
            "marker and body should use distinct colors"
        );
    }

    #[test]
    fn running_chat_applies_latest_palette_and_invalidates_display_cache() {
        let (palette_tx, palette_rx) = std::sync::mpsc::channel();
        let mut colors = test_palette();
        let mut display_lines_dirty = false;

        palette_tx.send(light_palette()).unwrap();
        assert!(drain_palette_updates(
            &palette_rx,
            &mut colors,
            &mut display_lines_dirty
        ));
        assert!(colors.is_light());
        assert!(display_lines_dirty);

        display_lines_dirty = false;
        palette_tx.send(light_palette()).unwrap();
        palette_tx.send(test_palette()).unwrap();
        assert!(drain_palette_updates(
            &palette_rx,
            &mut colors,
            &mut display_lines_dirty
        ));
        assert!(!colors.is_light());
        assert!(display_lines_dirty);
        assert!(!drain_palette_updates(
            &palette_rx,
            &mut colors,
            &mut display_lines_dirty
        ));
    }

    #[test]
    fn block_fenced_code_captures_inner() {
        let blocks = parse_markdown_blocks("```rust\nfn main() {}\n```");
        let code_lines: Vec<&str> = blocks
            .iter()
            .filter_map(|b| match b {
                MdBlock::CodeLine { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(code_lines, vec!["fn main() {}"]);
    }

    #[test]
    fn block_hr_variants() {
        let blocks = parse_markdown_blocks("---\n***\n___");
        let hr_count = blocks.iter().filter(|b| matches!(b, MdBlock::Hr)).count();
        assert_eq!(hr_count, 3);
    }

    #[test]
    fn block_list_markers_normalized() {
        let blocks = parse_markdown_blocks("- one\n* two\n+ three\n1. four");
        let markers: Vec<String> = blocks
            .iter()
            .filter_map(|b| match b {
                MdBlock::ListItem { marker, .. } => Some(marker.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(markers, vec!["• ", "• ", "• ", "1. "]);
    }

    #[test]
    fn wrap_preserves_styles_across_lines() {
        let segs = vec![plain("hello "), bold("bold word"), plain(" after text")];
        let wrapped = wrap_segments(&segs, 10);
        assert!(wrapped.len() > 1);
        // Verify bold span survives somewhere in the output.
        let has_bold = wrapped
            .iter()
            .flatten()
            .any(|s| s.style == InlineStyle::Bold);
        assert!(has_bold, "bold span lost during wrap: {:?}", wrapped);
    }

    #[test]
    fn wrap_width_zero_returns_input() {
        let segs = vec![plain("anything")];
        let wrapped = wrap_segments(&segs, 0);
        assert_eq!(wrapped.len(), 1);
    }

    #[test]
    fn wrap_oversized_cjk_token_does_not_exceed_width() {
        // A run of CJK characters with no whitespace must be cut into lines
        // where each line's visual width is <= width (each CJK char = 2 cols).
        let text = "这是一段很长的中文内容不包含任何空格直接连续输出测试换行功能是否正确";
        let segs = vec![plain(text)];
        let wrapped = wrap_segments(&segs, 10);
        assert!(
            wrapped.len() > 1,
            "expected multiple wrapped lines for wide CJK run"
        );
        for line in &wrapped {
            let w: usize = line
                .iter()
                .map(|s| unicode_column_width(&s.text, None))
                .sum();
            assert!(
                w <= 10,
                "line exceeds width=10: w={w} text={:?}",
                line.iter().map(|s| s.text.as_str()).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn wrap_oversized_url_does_not_exceed_width() {
        // A long URL (no spaces) must be hard-broken so no line exceeds width.
        let url = "https://example.com/very/long/path/to/some/resource?query=param&other=value";
        let segs = vec![plain(url)];
        let wrapped = wrap_segments(&segs, 20);
        assert!(
            wrapped.len() > 1,
            "expected multiple wrapped lines for long URL"
        );
        for line in &wrapped {
            let w: usize = line
                .iter()
                .map(|s| unicode_column_width(&s.text, None))
                .sum();
            assert!(
                w <= 20,
                "line exceeds width=20: w={w} text={:?}",
                line.iter().map(|s| s.text.as_str()).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn segments_to_plain_roundtrip() {
        let segs = tokenize_inline("**a** *b* `c`");
        assert_eq!(segments_to_plain(&segs), "a b c");
    }

    #[test]
    fn resolve_input_attachments_strips_known_tokens_and_keeps_unknown() {
        let (text, attachments) =
            resolve_input_attachments("please inspect @cwd @foo and @tab @cwd", &test_context())
                .expect("attachments");
        assert_eq!(text, "please inspect @foo and");
        assert_eq!(attachments.len(), 2);
        assert_eq!(attachments[0].label, "@cwd");
        assert_eq!(attachments[1].label, "@tab");
    }

    #[test]
    fn resolve_input_attachments_requires_question_after_tokens() {
        let err = resolve_input_attachments("@cwd @tab", &test_context()).unwrap_err();
        assert!(err.contains("Add a question"));
    }

    #[test]
    fn slash_command_options_include_waza_skills() {
        let labels: Vec<&str> = slash_command_options_for_token("/ch")
            .into_iter()
            .map(|(label, _)| label)
            .collect();
        assert_eq!(labels, vec!["/check"]);

        let labels: Vec<&str> = slash_command_options_for_token("/")
            .into_iter()
            .map(|(label, _)| label)
            .collect();
        assert!(labels.contains(&"/new"));
        assert!(labels.contains(&"/resume"));
        assert!(labels.contains(&"/hunt"));
        assert!(labels.contains(&"/write"));
    }

    #[test]
    fn only_control_slash_commands_submit_immediately() {
        assert!(slash_command_submits_immediately("/new"));
        assert!(slash_command_submits_immediately("/resume"));
        assert!(!slash_command_submits_immediately("/check"));
        assert!(!slash_command_submits_immediately("/write"));
    }

    #[test]
    fn push_waza_instruction_is_optional() {
        let mut out = Vec::new();
        push_waza_instruction(&mut out, None);
        assert!(out.is_empty());

        let skill = waza::find("/check").expect("check skill");
        push_waza_instruction(&mut out, Some(skill));
        assert_eq!(out.len(), 1);
        let content = out[0].0["content"].as_str().unwrap_or("");
        assert!(content.contains("Active skill: /check"));
        assert!(content.contains("current user turn only"));
    }

    #[test]
    fn resolve_input_attachments_requires_selection_for_selection_token() {
        let mut context = test_context();
        context.selected_text.clear();
        let err = resolve_input_attachments("explain @selection", &context).unwrap_err();
        assert!(err.contains("@selection"));
    }

    #[test]
    fn format_user_message_wraps_attached_context() {
        let msg = format_user_message(
            "what failed?",
            &[MessageAttachment::new(
                "tab",
                "@tab",
                "Current pane terminal snapshot.\nTreat this as read-only context.\n\nerror".into(),
            )],
        );
        assert!(msg.contains("Attached context:"));
        assert!(msg.contains("[@tab]"));
        assert!(msg.contains("User request:\nwhat failed?"));
    }

    #[test]
    fn build_cwd_attachment_summarizes_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "# Demo\nhello\n").unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname='demo'\n").unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let context = TerminalContext {
            cwd: dir.path().to_string_lossy().into_owned(),
            remote_host: None,
            visible_lines: vec![],
            tab_snapshot: String::new(),
            selected_text: String::new(),
            colors: test_palette(),
            panel_cols: 80,
            panel_rows: 24,
            last_exit_code: None,
            last_command_output: None,
        };

        let attachment = build_cwd_attachment(&context).expect("cwd attachment");
        assert_eq!(attachment.label, "@cwd");
        assert!(attachment.payload.contains("Directory summary"));
        assert!(attachment.payload.contains("README.md"));
        assert!(attachment.payload.contains("Cargo.toml"));
        assert!(attachment.payload.contains("src/"));
    }

    #[test]
    fn approval_summary_mutating_tools() {
        let args = serde_json::json!({"command": "rm -rf /tmp/foo"});
        assert!(approval_summary("shell_exec", &args).is_some());
        assert!(approval_summary("shell_bg", &args).is_some());
        let args = serde_json::json!({"path": "/tmp/foo.txt"});
        assert!(approval_summary("fs_write", &args).is_some());
        assert!(approval_summary("fs_patch", &args).is_some());
        assert!(approval_summary("fs_mkdir", &args).is_some());
        assert!(approval_summary("fs_delete", &args).is_some());
        // Every direct request requires approval, including GET, because the
        // destination can expose data outside the model conversation.
        for method in ["GET", "POST", "PUT", "PATCH", "DELETE"] {
            let args = serde_json::json!({"method": method, "url": "https://api.example.com/data"});
            assert!(
                approval_summary("http_request", &args).is_some(),
                "expected {} to require approval",
                method
            );
        }
    }

    #[test]
    fn approval_summary_readonly_tools_return_none() {
        let args = serde_json::json!({"path": "/tmp"});
        assert!(approval_summary("fs_read", &args).is_none());
        assert!(approval_summary("fs_list", &args).is_none());
        assert!(approval_summary("fs_search", &args).is_none());
        assert!(approval_summary("pwd", &serde_json::json!({})).is_none());
        assert!(approval_summary("shell_poll", &serde_json::json!({"pid": 123})).is_none());
        assert!(approval_summary("unknown_tool", &args).is_none());
    }

    #[test]
    fn shell_exec_read_only_commands_skip_approval() {
        for command in [
            "pwd",
            "ls -la",
            "cat Cargo.toml",
            "head -20 README.md",
            "tail -5 foo.log",
            "wc -l src/main.rs",
            "rg TODO src",
            "grep main Cargo.toml",
            "which cargo",
            "whereis git",
            "cut -d: -f1 Cargo.toml",
            "sort Cargo.toml",
            "uniq Cargo.toml",
            "nl Cargo.toml",
            "stat Cargo.toml",
            "file Cargo.toml",
            "realpath Cargo.toml",
            "readlink Cargo.toml",
            "basename src/main.rs",
            "dirname src/main.rs",
            "find . -name '*.rs'",
            // git commands: read-only and previously restricted ones are all now allowed
            "git status",
            "git diff HEAD~1",
            "git diff --output-indicator-new=+",
            "git show HEAD",
            "git log --oneline -5",
            "git grep main",
            "git ls-files",
            "git branch",
            "git branch -a",
            "git branch --list 'feat/*'",
            "git branch --show-current",
            "git remote -v",
            "git tag -l 'V0.*'",
            "git stash list",
            "git rev-parse --show-toplevel",
            // gh (GitHub CLI) read-only operations
            "gh issue list",
            "gh issue list --state open",
            "gh issue view 123",
            "gh pr list",
            "gh pr view 456",
            "gh pr diff 456",
            "gh pr checks 456",
            "gh repo view tw93/Kaku",
            "gh release list",
            "gh release view v0.10.0",
            "gh workflow list",
            "gh run list",
            "gh search issues kaku",
            "gh search prs --repo tw93/Kaku",
            "gh auth status",
            "gh status",
            "gh api repos/tw93/Kaku",
            "gh api -X GET repos/tw93/Kaku",
            "gh api --method=GET repos/tw93/Kaku",
            // other common dev commands (read-only)
            "cargo build",
            "cargo test",
            "make",
            "make test",
            "echo hello",
            // system info (read-only)
            "date",
            "date +%Y-%m-%d",
            "uname -a",
            "hostname",
            "whoami",
            "id",
            "groups",
            "uptime",
            "df -h",
            "du -sh .",
            "ps aux",
            "lsof -i :8080",
            "printenv PATH",
            // data processing (read-only)
            "jq .name package.json",
            "base64 Cargo.toml",
            "md5 Cargo.toml",
            "shasum Cargo.toml",
            "sha256sum Cargo.toml",
            "diff a.txt b.txt",
            "cmp a.bin b.bin",
            "printf 'hi\\n'",
            "seq 1 10",
            "od -c Cargo.toml",
            "hexdump -C Cargo.toml",
            "strings /bin/ls",
            "rev Cargo.toml",
            "tac Cargo.toml",
            // network queries (read-only)
            "dig example.com",
            "nslookup example.com",
            "host example.com",
            "ping -c 1 example.com",
            // curl: GET is default; no write flags
            "curl https://api.github.com/repos/tw93/Kaku",
            "curl -s https://api.github.com/repos/tw93/Kaku",
            "curl -sL https://api.github.com/repos/tw93/Kaku/issues",
            "curl -X GET https://api.github.com/repos/tw93/Kaku",
            "curl --request=GET https://api.github.com/repos/tw93/Kaku",
            "curl -I https://example.com",
            "curl -X HEAD https://example.com",
            // git extended read-only subcommands
            "git blame src/main.rs",
            "git reflog",
            "git shortlog -sn",
            "git describe --tags",
            "git merge-base main HEAD",
            "git ls-tree HEAD",
            "git cat-file -p HEAD",
            "git rev-list --count HEAD",
            "git name-rev HEAD",
            "git check-ignore -v foo.log",
            "git check-attr --all README.md",
            "git for-each-ref refs/heads",
            "git whatchanged -5",
            "git count-objects -v",
            "git worktree list",
            "git stash show",
            "git config --get user.email",
            "git config --list",
            "git config -l",
            "git config --get-all remote.origin.fetch",
            // gh extension read-only
            "gh extension list",
            "gh extension view gh-copilot",
            // brew read-only
            "brew list",
            "brew ls",
            "brew info wget",
            "brew search wget",
            "brew outdated",
            "brew home git",
            "brew doctor",
            "brew deps --tree wget",
            "brew leaves",
            "brew --prefix",
            "brew --cellar",
            "brew --cache",
            "brew --version",
            // misc shell/binary helpers
            "true",
            "false",
            "sleep 1",
            "tty",
            "locale",
            "nm -D /usr/bin/true",
            "otool -L /usr/bin/true",
            "addr2line -e /usr/bin/true 0x1000",
            "objdump -d /usr/bin/true",
            // syntax-check only interpreters (read-only)
            "perl -c script.pl",
            "ruby -c script.rb",
            "node --check script.js",
            // piped safe commands
            "grep 'foo|bar' Cargo.toml",
            "cat Cargo.toml | tr a-z A-Z",
            "rg TODO src | sort | uniq",
            "git diff HEAD~1 | head -20",
            "find . -name '*.rs' | wc -l",
            // `cd` is a harmless no-op in a one-shot shell_exec, but the common
            // `cd dir && <read-only cmd>` pattern must pass without prompting.
            "cd /tmp",
            "cd ~/www/Kaku",
            "cd ~/www/Kaku && grep -irA 5 -B 5 correction kaku-gui/src",
            "cd /tmp && ls -la",
            "cd ~/www/Kaku && pwd && ls",
            // `&&`, `||`, `;` chaining of read-only segments is safe
            "pwd && ls",
            "ls || echo nope",
            "ls; pwd",
            "pwd && ls && whoami",
            "cat Cargo.toml | grep name && pwd",
            // Safe redirections: stderr silenced and fd duplication.
            "ls -la ~/www/kaku 2>/dev/null",
            "ls 2> /dev/null",
            "cat foo 2>&1 | grep bar",
            "ls -la ~/www/kaku 2>/dev/null || echo \"Not found\"",
        ] {
            assert!(
                approval_summary("shell_exec", &serde_json::json!({ "command": command }))
                    .is_none(),
                "expected command to skip approval: {}",
                command
            );
        }
    }

    #[test]
    fn shell_exec_dangerous_commands_require_approval() {
        for command in [
            // privilege escalation
            "sudo rm -rf /",
            "sudo anything",
            // rm with recursive or force flags
            "rm important.txt",
            "rm -rf /tmp/x",
            "rm -r src/",
            "rm -f important.txt",
            "rm -Rf ./dist",
            // shells/interpreters, both inline and script execution paths
            "bash ./scripts/release.sh",
            "bash -c 'rm -rf /'",
            "sh ./scripts/nightly.sh",
            "sh -c 'pwd'",
            "python3 ./scripts/check_release_config.sh",
            "python3 -c 'print(1)'",
            "awk 'BEGIN{system(\"touch /tmp/pwn\")}'",
            "perl -e 'print 1'",
            "ruby -e 'print 1'",
            "node -e 'console.log(1)'",
            // xargs (pipes to arbitrary command)
            "rg TODO src | xargs rm",
            "find . | xargs echo",
            // disk operations
            "dd if=/dev/zero of=/dev/sda",
            "mkfs.ext4 /dev/sda1",
            "diskutil eraseDisk",
            // find with write/exec flags
            "find . -delete",
            "find . -fprint out.txt",
            "find . -exec rm {} \\;",
            // output flags on sort/tree
            "sort -o out.txt Cargo.toml",
            "tree -o out.txt .",
            // git dangerous operations
            "git push --force origin main",
            "git push -f",
            "git reset --hard HEAD",
            "git clean -fd",
            "git branch -D feature",
            "git checkout -f main",
            // git with --output
            "git diff --output=out.patch",
            // git write operations (modify local state)
            "git checkout main",
            "git branch new-feature",
            "git tag V0.9.0",
            "git remote add origin https://example.com/repo.git",
            "git stash push -m test",
            "git add .",
            "git commit -m 'fix: update config'",
            "git push origin main",
            // gh (GitHub CLI) mutating operations
            "gh issue create --title hi --body bye",
            "gh issue close 123",
            "gh issue comment 123 --body hi",
            "gh pr create --title hi",
            "gh pr merge 456",
            "gh pr close 456",
            "gh pr comment 456 --body hi",
            "gh repo create new-repo",
            "gh release create v1.0.0",
            "gh auth login",
            "gh auth logout",
            "gh api -X POST repos/tw93/Kaku/issues",
            "gh api --method POST repos/tw93/Kaku/issues",
            "gh api repos/tw93/Kaku/issues -F title=hi",
            // filesystem write operations
            "touch file.txt",
            "mkdir -p src/new",
            "cp Cargo.toml Cargo.toml.bak",
            "mv old.txt new.txt",
            // package managers (install/modify dependencies)
            "npm install",
            "npm run build",
            // git mutating worktree / stash / config
            "git worktree add ../new main",
            "git worktree remove ../old",
            "git stash push -m foo",
            "git stash pop",
            "git stash drop",
            "git config user.email foo@bar.com",
            "git config --unset user.email",
            // brew mutating operations
            "brew install wget",
            "brew uninstall wget",
            "brew upgrade",
            "brew cleanup",
            "brew tap homebrew/cask",
            "brew link wget",
            // curl with write flags or non-GET method
            "curl -o out.html https://example.com",
            "curl --output out.html https://example.com",
            "curl -O https://example.com/file.zip",
            "curl --remote-name https://example.com/file.zip",
            "curl -T upload.bin https://example.com/api",
            "curl -d 'a=1' https://example.com/api",
            "curl --data 'a=1' https://example.com/api",
            "curl -F file=@x.txt https://example.com/api",
            "curl -X POST https://example.com/api",
            "curl -X DELETE https://example.com/api/123",
            "curl --request=POST https://example.com/api",
            "curl -sO https://example.com/file.zip",
            "curl -so out.html https://example.com",
            "curl -sX POST https://example.com/api",
            // shell hazards: output redirections, backgrounding, command substitution
            "cat a > b",
            "echo hi >> log.txt",
            "cat < input.txt",
            "sleep 100 &",
            "echo `whoami`",
            "echo $(pwd)",
            // sensitive paths must not bypass approval through read-only commands
            "cat ~/.ssh/id_rsa",
            "cat $HOME/.ssh/id_rsa",
            "grep secret ~/.aws/credentials",
            // chain containing any dangerous segment still requires approval
            "ls && rm -rf /tmp/x",
            "cd /tmp && touch foo",
            "pwd; git push",
        ] {
            assert!(
                approval_summary("shell_exec", &serde_json::json!({ "command": command }))
                    .is_some(),
                "expected command to require approval: {}",
                command
            );
        }
    }

    #[test]
    fn visible_snapshot_message_prefixes_each_line() {
        let msg = build_visible_snapshot_message(&TerminalContext {
            cwd: "/tmp".to_string(),
            remote_host: None,
            visible_lines: vec![
                "line 1".to_string(),
                "```".to_string(),
                "sudo rm -rf /".to_string(),
            ],
            tab_snapshot: String::new(),
            selected_text: String::new(),
            colors: test_palette(),
            panel_cols: 80,
            panel_rows: 24,
            last_exit_code: None,
            last_command_output: None,
        })
        .expect("snapshot message");

        let serde_json::Value::Object(obj) = msg.0 else {
            panic!("expected object");
        };
        let content = obj["content"].as_str().expect("content");
        assert!(content.contains("TERM| line 1"));
        assert!(content.contains("TERM| ```"));
        assert!(content.contains("TERM| sudo rm -rf /"));
        assert!(!content.contains("```terminal"));
    }
}

#[cfg(test)]
mod undo_tests {
    use super::super::{push_input_snapshot, InputSnapshot, INPUT_UNDO_MAX};

    #[test]
    fn empty_input_skipped() {
        let mut stack: Vec<InputSnapshot> = Vec::new();
        push_input_snapshot(&mut stack, "", 0);
        assert!(stack.is_empty(), "empty input should not snapshot");
    }

    #[test]
    fn push_records_input_and_cursor() {
        let mut stack = Vec::new();
        push_input_snapshot(&mut stack, "hello", 3);
        assert_eq!(stack.len(), 1);
        assert_eq!(stack[0].input, "hello");
        assert_eq!(stack[0].cursor, 3);
    }

    #[test]
    fn fifo_evicts_oldest_when_cap_reached() {
        let mut stack = Vec::new();
        for i in 0..INPUT_UNDO_MAX {
            push_input_snapshot(&mut stack, &format!("v{i}"), i);
        }
        assert_eq!(stack.len(), INPUT_UNDO_MAX);
        assert_eq!(stack[0].input, "v0");
        push_input_snapshot(&mut stack, "overflow", 0);
        assert_eq!(stack.len(), INPUT_UNDO_MAX);
        assert_eq!(stack[0].input, "v1", "oldest should be dropped");
        assert_eq!(stack.last().unwrap().input, "overflow");
    }

    #[test]
    fn pop_returns_last_pushed() {
        let mut stack = Vec::new();
        push_input_snapshot(&mut stack, "a", 1);
        push_input_snapshot(&mut stack, "ab", 2);
        let snap = stack.pop().expect("non-empty");
        assert_eq!(snap.input, "ab");
        assert_eq!(snap.cursor, 2);
        let snap = stack.pop().expect("non-empty");
        assert_eq!(snap.input, "a");
    }
}

#[cfg(test)]
mod interaction_tests {
    use super::super::state::App;
    use super::super::*;
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::Instant;

    fn app() -> App {
        let context = super::markdown_tests::test_context();
        let messages = vec![];
        let cols = 80;
        let rows = 24;
        let available_models = vec!["test".to_string()];
        let model_index = 0;
        let model_fetch = ModelFetch::Loaded;
        let model_fetch_rx = None;
        let active_id = String::new();
        let onboarding_pending = false;
        let client = AiClient::new(AssistantConfig {
            api_key: String::new(),
            chat_model: "test".into(),
            chat_model_choices: vec![],
            base_url: "http://127.0.0.1:1/v1".into(),
            custom_headers: vec![],
            provider: "test".into(),
            api_mode: crate::ai_client::ApiMode::Responses,
            auth_type: "api_key".into(),
            chat_tools_enabled: true,
            native_web_search: false,
            web_search_provider: None,
            web_search_api_key: None,
            web_fetch_script: None,
            fast_model: None,
            memory_curator_model: None,
        });
        App {
            mode: AppMode::Chat,
            messages,
            input: String::new(),
            input_cursor: 0,
            scroll_offset: 0,
            viewport_height: 0,
            display_width: 0,
            approval_scroll: 0,
            pending_insert: None,
            pressed_control: None,
            focused_control: None,
            drafts: HashMap::new(),
            is_streaming: false,
            available_models,
            model_index,
            model_fetch,
            model_fetch_rx,
            suggest_rx: None,
            model_status_flash: None,
            token_rx: None,
            grapheme_queue: VecDeque::new(),
            stream_pending_done: false,
            stream_pending_err: None,
            pending_responses_items: Vec::new(),
            cancel_flag: Arc::new(AtomicBool::new(false)),
            client,
            cols,
            rows,
            context,
            cached_display_lines: Vec::new(),
            display_lines_dirty: true,
            assistant_render_memo: HashMap::new(),
            selection: None,
            selecting: false,
            drag_origin: None,
            left_was_pressed: false,
            pending_approval: None,
            active_id,
            attachment_picker_index: 0,
            spinner_frame: 0,
            spinner_tick: Instant::now(),
            onboarding_pending,
            input_clicked_this_stream: false,
            input_undo_stack: Vec::new(),
            stream_is_transient: false,
            next_assistant_is_context: false,
            show_thinking: false,
        }
    }

    #[test]
    fn poll_timeout_redraws_only_for_a_changed_visible_spinner() {
        assert!(!redraw_after_poll_timeout(false, false));
        assert!(redraw_after_poll_timeout(true, false));
        assert!(!redraw_after_poll_timeout(true, true));
    }

    #[test]
    fn reading_position_stays_fixed_when_stream_appends_lines() {
        let mut app = app();
        app.messages.push(Message::text(
            Role::Assistant,
            (0..60).map(|i| format!("line {i}\n")).collect::<String>(),
            false,
            false,
        ));
        app.rebuild_display_cache();
        app.scroll_offset = 12;
        let before = app.display_lines().len() - app.msg_area_height() - app.scroll_offset;
        app.messages[0]
            .content
            .push_str("another line\nanother line\n");
        app.display_lines_dirty = true;
        app.rebuild_display_cache();
        let after = app.display_lines().len() - app.msg_area_height() - app.scroll_offset;
        assert_eq!(before, after);
    }

    #[test]
    fn composer_reserves_padding_in_regular_panes() {
        for rows in [12, 24, 40] {
            let mut app = app();
            app.rows = rows;
            for input in ["", "@tab", "first\nsecond\nthird"] {
                app.input = input.into();
                assert_eq!(
                    app.msg_area_height() + app.picker_height() + app.input_visible_rows() + 5,
                    rows
                );
            }
        }
    }

    #[test]
    fn composer_toolbar_click_uses_padded_input_layout() {
        use termwiz::input::{Modifiers, MouseButtons, MouseEvent};
        let mut app = app();
        for buttons in [MouseButtons::LEFT, MouseButtons::NONE] {
            super::super::input::handle_mouse(
                &MouseEvent {
                    x: 14,
                    y: (app.rows - 5) as u16,
                    mouse_buttons: buttons,
                    modifiers: Modifiers::NONE,
                },
                &mut app,
            );
        }
        assert_eq!(app.input, "@");
    }

    #[test]
    fn picker_rows_are_excluded_from_message_hit_area() {
        let mut app = app();
        app.input = "/".into();
        let picker = app.slash_picker_options().len().min(MAX_PICKER_ROWS);
        assert_eq!(
            app.msg_area_height() + app.input_visible_rows() + picker + 5,
            app.rows
        );
    }
    #[test]
    fn paste_preserves_composed_text_and_normalizes_crlf_once() {
        let mut app = app();
        app.input = "before after".into();
        app.input_cursor = 7;
        app.is_streaming = true;
        app.paste_input("中文🙂\r\nnext\rline\tend\u{1b}");
        assert_eq!(app.input, "before 中文🙂\nnext\nline endafter");
        assert!(app.input_clicked_this_stream);
        app.undo_input();
        assert_eq!(app.input, "before after");
        assert_eq!(app.input_cursor, 7);
    }

    #[test]
    fn permission_details_scroll_to_end_and_paste_cannot_change_draft() {
        let mut app = app();
        app.cols = 32;
        app.rows = 8;
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let text = format!("delete: /tmp/{}END.txt", "目录/".repeat(80));
        app.pending_approval = Some((text.clone(), tx));
        app.input = "draft".into();
        app.paste_input("unwanted");
        assert_eq!(app.input, "draft");
        let lines = app.approval_lines();
        assert_eq!(lines.concat(), text);
        super::super::input::handle_key(
            &termwiz::input::KeyEvent {
                key: termwiz::input::KeyCode::End,
                modifiers: termwiz::input::Modifiers::NONE,
            },
            &mut app,
        );
        assert_eq!(app.approval_scroll + app.rows - 4, lines.len());
        assert!(lines.last().unwrap().ends_with("END.txt"));
        app.activate_control(ChatControl::Deny);
        assert!(!rx.recv().unwrap());
        assert!(app.pending_approval.is_none());
    }

    #[test]
    fn permission_cannot_be_approved_in_an_unreadable_pane() {
        let mut app = app();
        app.cols = 10;
        app.rows = 4;
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        app.pending_approval = Some(("delete target".into(), tx));
        super::super::input::handle_key(
            &termwiz::input::KeyEvent {
                key: termwiz::input::KeyCode::Enter,
                modifiers: termwiz::input::Modifiers::NONE,
            },
            &mut app,
        );
        assert!(rx.try_recv().is_err());
        super::super::input::handle_key(
            &termwiz::input::KeyEvent {
                key: termwiz::input::KeyCode::Escape,
                modifiers: termwiz::input::Modifiers::NONE,
            },
            &mut app,
        );
        assert!(!rx.recv().unwrap());
    }

    #[test]
    fn code_actions_keep_original_lines_and_separate_adjacent_fences() {
        let mut app = app();
        app.cols = 28;
        let long = format!("printf '{}'", "x".repeat(120));
        app.messages.push(Message::text(
            Role::Assistant,
            format!("```sh\n{long}\n```\n```sh\necho second\n```\n```sh\necho one\necho two\n```"),
            true,
            false,
        ));
        app.rebuild_display_cache();
        let blocks: Vec<_> = app
            .display_lines()
            .iter()
            .filter_map(|line| match line {
                DisplayLine::CodeActions { text, can_insert } => Some((text.as_str(), *can_insert)),
                _ => None,
            })
            .collect();
        assert_eq!(
            blocks,
            vec![
                (long.as_str(), true),
                ("echo second", true),
                ("echo one\necho two", false)
            ]
        );
        app.messages[0].complete = false;
        app.display_lines_dirty = true;
        app.rebuild_display_cache();
        assert!(app.display_lines().iter().all(|line| !matches!(
            line,
            DisplayLine::CodeActions {
                can_insert: true,
                ..
            }
        )));
    }

    #[test]
    fn conversation_drafts_restore_cursor_without_crossing_undo_history() {
        let mut app = app();
        app.active_id = "one".into();
        app.input = "中文 draft".into();
        app.input_cursor = 2;
        app.remember_draft();
        app.active_id = "two".into();
        app.restore_draft();
        assert!(app.input.is_empty());
        app.input = "second".into();
        app.input_cursor = 3;
        app.remember_draft();
        app.active_id = "one".into();
        app.restore_draft();
        assert_eq!(app.input, "中文 draft");
        assert_eq!(app.input_cursor, 2);
        assert!(app.input_undo_stack.is_empty());
    }

    #[test]
    fn viewport_tracks_bottom_until_user_scrolls_and_survives_input_growth() {
        let mut app = app();
        app.messages.push(Message::text(
            Role::Assistant,
            "line\n".repeat(60),
            false,
            false,
        ));
        app.rebuild_display_cache();
        let start = app.visible_start();
        app.messages[0].content.push_str("more\n");
        app.display_lines_dirty = true;
        app.rebuild_display_cache();
        assert!(app.visible_start() > start);
        assert_eq!(app.scroll_offset, 0);
        app.scroll_offset = 8;
        let start = app.visible_start();
        app.input = "draft\n".repeat(8);
        app.rebuild_display_cache();
        assert_eq!(app.visible_start(), start);
    }

    #[test]
    fn small_panes_reserve_input_and_picker_inside_the_viewport() {
        for cols in [24, 40, 80, 180] {
            for rows in [6, 8, 12, 36] {
                let mut app = app();
                app.cols = cols;
                app.rows = rows;
                app.input = "/".into();
                assert!(
                    app.msg_area_height()
                        + app.picker_height()
                        + app.input_visible_rows()
                        + 3
                        + 2 * app.input_padding_rows()
                        <= rows
                );
                for (_, _, span) in super::super::layout::control_spans(&app.controls(), cols) {
                    assert!(span.end < cols);
                }
            }
        }
    }
    #[test]
    fn approval_click_requires_press_and_release_on_same_control() {
        use termwiz::input::{Modifiers, MouseButtons, MouseEvent};
        let mut app = app();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        app.pending_approval = Some(("delete target".into(), tx));
        let click = |app: &mut App, x, buttons| {
            super::super::input::handle_mouse(
                &MouseEvent {
                    x,
                    y: (app.rows - 2) as u16,
                    mouse_buttons: buttons,
                    modifiers: Modifiers::NONE,
                },
                app,
            )
        };
        click(&mut app, 3, MouseButtons::LEFT);
        click(&mut app, 12, MouseButtons::NONE);
        assert!(rx.try_recv().is_err());
        click(&mut app, 3, MouseButtons::LEFT);
        click(&mut app, 3, MouseButtons::NONE);
        assert!(rx.recv().unwrap());
    }

    #[test]
    fn keyboard_can_focus_controls_without_sending_a_message() {
        use termwiz::input::{KeyCode, KeyEvent, Modifiers};
        let mut app = app();
        super::super::input::handle_key(
            &KeyEvent {
                key: KeyCode::Tab,
                modifiers: Modifiers::NONE,
            },
            &mut app,
        );
        assert!(app.focused_control == Some(ChatControl::History));
        super::super::input::handle_key(
            &KeyEvent {
                key: KeyCode::Tab,
                modifiers: Modifiers::NONE,
            },
            &mut app,
        );
        assert!(app.focused_control == Some(ChatControl::Context));
        super::super::input::handle_key(
            &KeyEvent {
                key: KeyCode::Enter,
                modifiers: Modifiers::NONE,
            },
            &mut app,
        );
        assert_eq!(app.input, "@");
        assert!(app.messages.is_empty());
    }
    #[test]
    fn resize_cannot_retarget_a_focused_code_action() {
        let mut app = app();
        app.cols = 104;
        app.messages.push(Message::text(
            Role::Assistant,
            &format!(
                "{}\n```sh\necho first\n```\n```sh\necho second\n```",
                "x".repeat(100)
            ),
            true,
            false,
        ));
        app.rebuild_display_cache();
        let index = app
            .display_lines()
            .iter()
            .rposition(|l| matches!(l, DisplayLine::CodeActions { .. }))
            .unwrap();
        app.focused_control = Some(ChatControl::InsertCode(index));
        app.cols = 38;
        app.display_lines_dirty = true;
        app.rebuild_display_cache();
        assert!(app.focused_control.is_none());
    }

    #[test]
    fn attachment_click_never_submits_exact_match() {
        let mut app = app();
        // Unknown /model selection avoids network or persistence if this regresses.
        app.input = "/model @cwd".into();
        app.input_cursor = app.input.chars().count();
        assert!(!app.attachment_picker_options().is_empty());
        app.activate_control(ChatControl::Pick(0));
        assert_eq!(app.input, "/model @cwd ");
        assert!(app.messages.is_empty());
    }

    #[test]
    fn an_approval_press_cannot_cross_requests() {
        use termwiz::input::{KeyCode, KeyEvent, Modifiers, MouseButtons, MouseEvent};
        for key in [KeyCode::Escape, KeyCode::Enter] {
            let mut app = app();
            let mouse = |app: &mut App, buttons| {
                super::super::input::handle_mouse(
                    &MouseEvent {
                        x: 3,
                        y: (app.rows - 2) as u16,
                        mouse_buttons: buttons,
                        modifiers: Modifiers::NONE,
                    },
                    app,
                )
            };
            let (tx, _rx) = std::sync::mpsc::sync_channel(1);
            app.pending_approval = Some(("request A".into(), tx));
            mouse(&mut app, MouseButtons::LEFT);
            super::super::input::handle_key(
                &KeyEvent {
                    key,
                    modifiers: Modifiers::NONE,
                },
                &mut app,
            );
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            app.pending_approval = Some(("request B".into(), tx));
            mouse(&mut app, MouseButtons::LEFT);
            mouse(&mut app, MouseButtons::NONE);
            assert!(rx.try_recv().is_err());
            mouse(&mut app, MouseButtons::LEFT);
            mouse(&mut app, MouseButtons::NONE);
            assert!(rx.recv().unwrap());
        }
    }

    #[test]
    fn stopping_keeps_partial_answer_but_never_inserts_unclosed_code() {
        let mut app = app();
        app.is_streaming = true;
        app.messages.push(Message::text(
            Role::Assistant,
            "Partial answer\n```sh\necho unfinished",
            false,
            false,
        ));
        app.cancel_stream();
        app.rebuild_display_cache();
        assert_eq!(app.collect_persisted_messages().len(), 1);
        assert!(app.display_lines().iter().any(|l| matches!(
            l,
            DisplayLine::CodeActions {
                can_insert: false,
                ..
            }
        )));
    }
    #[test]
    fn history_geometry_ignores_hidden_draft_and_switch_clears_selection() {
        let mut app = app();
        app.rows = 8;
        app.cols = 40;
        app.input = "one\ntwo\nthree\nfour\nfive".into();
        app.input_cursor = app.input.chars().count();
        assert_eq!(app.msg_area_height(), 0);
        assert_eq!(app.resume_picker_height(), 4);
        app.selection = Some((0, 0, 1, 1));
        app.selecting = true;
        app.drag_origin = Some((0, 0));
        app.restore_draft();
        assert!(app.selection.is_none());
        assert!(!app.selecting);
        assert!(app.drag_origin.is_none());
    }
    #[test]
    fn passive_pointer_motion_preserves_keyboard_focus() {
        use termwiz::input::{Modifiers, MouseButtons, MouseEvent};
        let mut app = app();
        app.focused_control = Some(ChatControl::Context);
        super::super::input::handle_mouse(
            &MouseEvent {
                x: 1,
                y: 1,
                mouse_buttons: MouseButtons::NONE,
                modifiers: Modifiers::NONE,
            },
            &mut app,
        );
        assert!(app.focused_control == Some(ChatControl::Context));
    }
}
