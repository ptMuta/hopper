//! Prompts driven through a real pseudo-terminal: keys in, screen out, terminal restored.
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Pty {
    master: std::fs::File,
    slave: OwnedFd,
    child: Child,
    screen: Vec<u8>,
}

fn record(root: &Path, name: &str) {
    let dir = root.join("config/hopper/instances");
    std::fs::create_dir_all(&dir).unwrap();
    let record = serde_json::json!({
        "format": 1,
        "name": name,
        "scope": "user",
        "root": root.join("data/servers").join(name),
        "source": {"provider": "gtnh", "pack": null, "collection": null, "file": null, "url": null,
                   "channel": null, "pack_version": null, "mc": null, "loader": null},
        "runtime": {"java": null, "java_major": null, "java_vendor": null, "jvm_args_file": null,
                    "no_optional": false, "force_include": [], "force_exclude": [],
                    "allow_client_pack": false, "skip_blocked": false},
        "state": "ready",
        "update": "off",
        "restart": "off",
        "warn": 300,
        "started": false,
        "firewall_port": null,
        "backup": null,
        "original": null,
    });
    std::fs::write(
        dir.join(format!("{name}.json")),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
}

fn command(args: &[&str], root: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hopper"));
    cmd.args(args)
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("HOPPER_DATA_DIR", root.join("data"))
        .env("HOPPER_CACHE_DIR", root.join("cache"))
        .env("HOPPER_INSTANCE_ROOT", root.join("data/servers"))
        .env("TERM", "xterm-256color")
        .env("LANG", "C.UTF-8")
        .env("NO_COLOR", "1")
        .env_remove("CI")
        .env_remove("HOPPER_NO_INPUT")
        .env_remove("HOPPER_ACCESSIBLE")
        .current_dir(root);
    cmd
}

impl Pty {
    fn spawn(args: &[&str], root: &Path) -> Pty {
        Pty::spawn_with(command(args, root))
    }

    fn spawn_with(mut cmd: Command) -> Pty {
        let (mut master, mut slave) = (0, 0);
        let size = libc::winsize {
            ws_row: 24,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty fills two fds.
        unsafe {
            assert_eq!(
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &size
                ),
                0
            );
        }
        let master = unsafe { std::fs::File::from_raw_fd(master) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        let io = |fd: &OwnedFd| Stdio::from(fd.try_clone().unwrap());
        cmd.stdin(io(&slave)).stdout(io(&slave)).stderr(io(&slave));
        // SAFETY: setsid + TIOCSCTTY in the child makes the pty its controlling terminal and
        // the child its foreground job, as a shell would.
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY, 0);
                Ok(())
            });
        }
        let child = cmd.spawn().unwrap();
        // SAFETY: non-blocking reads from the master.
        unsafe {
            let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        Pty {
            master,
            slave,
            child,
            screen: vec![],
        }
    }

    fn pump(&mut self) {
        let mut buf = [0u8; 4096];
        while let Ok(n) = self.master.read(&mut buf) {
            if n == 0 {
                break;
            }
            self.screen.extend_from_slice(&buf[..n]);
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.screen).into_owned()
    }

    fn wait_for(&mut self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            self.pump();
            if self.text().contains(needle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("never saw {needle:?} in:\n{}", self.text());
    }

    fn send(&mut self, keys: &[u8]) {
        self.master.write_all(keys).unwrap();
        std::thread::sleep(Duration::from_millis(80));
    }

    fn termios(&self) -> libc::termios {
        // SAFETY: tcgetattr on the slave side.
        unsafe {
            let mut t = std::mem::zeroed();
            libc::tcgetattr(self.slave.as_raw_fd(), &mut t);
            t
        }
    }

    fn finish(mut self) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            self.pump();
            if let Some(status) = self.child.try_wait().unwrap() {
                self.pump();
                return status.code().unwrap_or(-1);
            }
            assert!(Instant::now() < deadline, "still running:\n{}", self.text());
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn plain(args: &[&str], root: &Path) -> Output {
    command(args, root).stdin(Stdio::null()).output().unwrap()
}

#[test]
fn picks_an_instance_with_arrow_keys() {
    let root = tempfile::tempdir().unwrap();
    record(root.path(), "alpha");
    record(root.path(), "beta");
    let mut pty = Pty::spawn(&["show", "--plain"], root.path());
    pty.wait_for("> alpha");
    assert!(pty.text().contains("? Instance"), "{}", pty.text());
    assert!(pty.text().contains("gtnh stable"), "{}", pty.text());
    pty.send(b"\x1b[B");
    pty.wait_for("> beta");
    pty.send(b"\r");
    pty.wait_for("  Instance    beta");
    pty.finish();
}

#[test]
fn ctrl_c_cancels_and_restores_the_terminal() {
    let root = tempfile::tempdir().unwrap();
    record(root.path(), "alpha");
    record(root.path(), "beta");
    let mut pty = Pty::spawn(&["status"], root.path());
    let before = pty.termios();
    pty.wait_for("> alpha");
    let during = pty.termios();
    assert_ne!(
        before.c_lflag & libc::ICANON,
        during.c_lflag & libc::ICANON,
        "raw while asking"
    );
    pty.send(b"\x03");
    pty.wait_for("cancelled");
    let after = pty.termios();
    let code = pty.finish();
    assert_eq!(code, 130);
    assert_eq!(before.c_lflag, after.c_lflag);
    assert_eq!(before.c_iflag, after.c_iflag);
}

#[test]
fn typing_filters_and_esc_clears_before_backing_out() {
    let root = tempfile::tempdir().unwrap();
    for name in ["alpha", "beta", "gamma"] {
        record(root.path(), name);
    }
    let mut pty = Pty::spawn(&["status"], root.path());
    pty.wait_for("> alpha");
    pty.send(b"gam");
    pty.wait_for("1/3");
    pty.send(b"\x1b");
    std::thread::sleep(Duration::from_millis(100));
    pty.send(b"\x1b");
    let code = pty.finish();
    assert_eq!(code, 130, "esc on an empty filter backs out");
}

#[test]
fn a_single_instance_is_taken_for_reading_but_shown_for_changes() {
    let root = tempfile::tempdir().unwrap();
    record(root.path(), "alpha");
    let mut pty = Pty::spawn(&["show", "--plain"], root.path());
    pty.wait_for("Instance    alpha (only one)");
    pty.finish();

    let mut pty = Pty::spawn(&["remove"], root.path());
    pty.wait_for("> alpha");
    pty.send(b"\x03");
    assert_eq!(pty.finish(), 130);
}

#[test]
fn line_mode_asks_numbered_questions() {
    let root = tempfile::tempdir().unwrap();
    record(root.path(), "alpha");
    record(root.path(), "beta");
    let mut cmd = command(&["show", "--plain"], root.path());
    cmd.env("HOPPER_ACCESSIBLE", "1");
    let mut pty = Pty::spawn_with(cmd);
    pty.wait_for("  2) beta");
    assert!(
        !pty.text().contains("\x1b["),
        "no cursor movement: {:?}",
        pty.text()
    );
    pty.send(b"2\r");
    pty.finish();
}

#[test]
fn without_a_terminal_missing_input_fails_once_with_exit_2() {
    let root = tempfile::tempdir().unwrap();
    record(root.path(), "alpha");
    for args in [&["status"][..], &["stop"], &["show"], &["disable", "alpha"]] {
        let out = plain(args, root.path());
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {err}");
        assert!(err.contains("error: missing"), "{err}");
        assert!(err.contains("run in a terminal to be asked"), "{err}");
    }
}

#[test]
fn no_input_and_ci_never_prompt_even_at_a_terminal() {
    let root = tempfile::tempdir().unwrap();
    record(root.path(), "alpha");
    record(root.path(), "beta");
    let pty = Pty::spawn(&["status", "--no-input"], root.path());
    assert_eq!(pty.finish(), 2);
    let mut cmd = command(&["status"], root.path());
    cmd.env("CI", "true");
    let pty = Pty::spawn_with(cmd);
    assert_eq!(pty.finish(), 2);
    let pty = Pty::spawn(&["status", "--json"], root.path());
    assert_eq!(pty.finish(), 2);
}

#[test]
fn configure_edits_settings_in_one_list() {
    let root = tempfile::tempdir().unwrap();
    record(root.path(), "alpha");
    let mut pty = Pty::spawn(&["configure", "alpha"], root.path());
    pty.wait_for("> Target");
    pty.send(b"\x1b[B\r");
    pty.wait_for("> off");
    pty.send(b"\x1b[B\x1b[B\r");
    pty.wait_for("daily  (was off)");
    pty.send(b"\x1b[6~\r");
    pty.wait_for("Update      off -> daily");
    pty.finish();
    let saved: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.path().join("config/hopper/instances/alpha.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["update"], "daily");
}

#[test]
fn completions_offer_the_login_shell() {
    let root = tempfile::tempdir().unwrap();
    let mut cmd = command(&["completions"], root.path());
    cmd.env("SHELL", "/usr/bin/fish");
    let mut pty = Pty::spawn_with(cmd);
    pty.wait_for("> fish");
    pty.send(b"\r");
    pty.wait_for("complete -c hopper");
    assert_eq!(pty.finish(), 0);
}

#[test]
fn sigterm_and_ctrl_z_give_the_terminal_back() {
    let root = tempfile::tempdir().unwrap();
    record(root.path(), "alpha");
    record(root.path(), "beta");
    let mut pty = Pty::spawn(&["status"], root.path());
    let cooked = pty.termios().c_lflag;
    pty.wait_for("> alpha");

    // Ctrl-Z gives the terminal back and stops. This session leader has no shell to stop
    // for (an orphaned group ignores SIGTSTP), so it carries on asking: what matters here
    // is that the prompt survives and redraws.
    pty.screen.clear();
    pty.send(b"\x1a");
    pty.wait_for("> alpha");
    assert_ne!(pty.termios().c_lflag, cooked, "raw again");

    unsafe { libc::kill(pty.child.id() as i32, libc::SIGTERM) };
    let slave = pty.slave.try_clone().unwrap();
    let code = pty.finish();
    assert_eq!(
        code, -1,
        "terminated by the signal, as before the prompt existed"
    );
    let after = unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        libc::tcgetattr(slave.as_raw_fd(), &mut t);
        t
    };
    assert_eq!(after.c_lflag, cooked, "cooked again after SIGTERM");
}

#[test]
fn configure_with_yes_does_not_open_the_editor() {
    let root = tempfile::tempdir().unwrap();
    record(root.path(), "alpha");
    let pty = Pty::spawn(&["configure", "alpha", "-y"], root.path());
    pty.finish();
}
