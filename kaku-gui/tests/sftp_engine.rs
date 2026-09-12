//! End-to-end tests for the transfer engine against a real sshd on
//! loopback: the same code the file browser and Finder drops run.
//!
//! Skips itself when no sshd binary is present, like the wezterm-ssh
//! e2e suite.  Nothing here talks to a machine other than 127.0.0.1.

use kaku_gui_lib::sftp_transfer::{TransferManager, TransferState};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use wezterm_ssh::{Config, Session, SessionEvent};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const SSHD: &str = "/usr/sbin/sshd";

/// Captures the log output so a session with `wezterm_ssh_verbose` can
/// be inspected for the exact SFTP requests it sent.
static LOG_LINES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

struct Logger;

impl log::Log for Logger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        if let Ok(mut lines) = LOG_LINES.lock() {
            lines.push(record.args().to_string());
        }
    }
    fn flush(&self) {}
}

static LOGGER: Logger = Logger;

static SET_LOGGER_OK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn capture_logging() {
    let ok = log::set_logger(&LOGGER).is_ok();
    SET_LOGGER_OK.store(ok, std::sync::atomic::Ordering::Relaxed);
    log::set_max_level(log::LevelFilter::Trace);
}

fn sshd_available() -> bool {
    Path::new(SSHD).exists() && Command::new("ssh-keygen").arg("-?").output().is_ok()
}

struct Server {
    child: Child,
    tmp: TempDir,
    port: u16,
    key: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn keygen(path: &Path) -> bool {
    Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-f"])
        .arg(path)
        .args(["-N", "", "-q"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

impl Server {
    fn spawn() -> Option<Self> {
        let tmp = TempDir::new().ok()?;
        let host_key = tmp.path().join("host_key");
        let user_key = tmp.path().join("id_ed25519");
        if !keygen(&host_key) || !keygen(&user_key) {
            return None;
        }

        let authorized = tmp.path().join("authorized_keys");
        std::fs::copy(user_key.with_extension("pub"), &authorized).ok()?;
        let mut perms = std::fs::metadata(&authorized).ok()?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&authorized, perms).ok()?;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
        let port = listener.local_addr().ok()?.port();
        drop(listener);

        let config_path = tmp.path().join("sshd_config");
        let mut config = std::fs::File::create(&config_path).ok()?;
        write!(
            config,
            "Port {port}\n\
             ListenAddress 127.0.0.1\n\
             HostKey {host}\n\
             PidFile {pid}\n\
             AuthorizedKeysFile {auth}\n\
             StrictModes no\n\
             UsePAM yes\n\
             PasswordAuthentication no\n\
             KbdInteractiveAuthentication no\n\
             PubkeyAuthentication yes\n\
             Subsystem sftp internal-sftp\n\
             LogLevel VERBOSE\n",
            host = host_key.display(),
            pid = tmp.path().join("sshd.pid").display(),
            auth = authorized.display(),
        )
        .ok()?;

        let log_path = tmp.path().join("sshd.log");
        let child = Command::new(SSHD)
            .args(["-D", "-f"])
            .arg(&config_path)
            .args(["-E"])
            .arg(&log_path)
            .spawn()
            .ok()?;

        let mut server = Self {
            child,
            tmp,
            port,
            key: user_key,
        };

        // Wait for the port to accept connections.
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(50));
            if std::net::TcpStream::connect(("127.0.0.1", server.port)).is_ok() {
                return Some(server);
            }
        }
        let _ = server.child.kill();
        None
    }

    fn connect_verbose(&self) -> Option<Session> {
        self.connect_inner(true)
    }

    fn connect(&self) -> Option<Session> {
        self.connect_inner(false)
    }

    fn connect_inner(&self, verbose: bool) -> Option<Session> {
        let config = Config::new();
        let mut config = config.for_host("localhost");
        config.insert("hostname".into(), "127.0.0.1".into());
        config.insert("port".into(), self.port.to_string());
        config.insert(
            "user".into(),
            std::env::var("USER").unwrap_or_else(|_| "root".into()),
        );
        config.insert("identityfile".into(), self.key.display().to_string());
        config.insert("identitiesonly".into(), "yes".into());
        config.insert("stricthostkeychecking".into(), "no".into());
        config.insert("userknownhostsfile".into(), "/dev/null".into());
        config.insert("wezterm_ssh_backend".into(), "libssh".into());
        if verbose {
            config.insert("wezterm_ssh_verbose".into(), "true".into());
        }

        let (session, events) = Session::connect(config).ok()?;
        let authenticated = smol::block_on(async {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return false;
                }
                match smol::future::or(async { events.recv().await.ok() }, async {
                    smol::Timer::after(remaining).await;
                    None
                })
                .await
                {
                    Some(SessionEvent::Authenticated) => return true,
                    Some(SessionEvent::HostVerify(ev)) => {
                        if ev.answer(true).await.is_err() {
                            return false;
                        }
                    }
                    Some(SessionEvent::Error(_)) | None => return false,
                    Some(_) => {}
                }
            }
        });
        if authenticated {
            Some(session)
        } else {
            None
        }
    }
}

/// Drain the manager until every transfer is terminal, or time out.
fn wait_for_all(manager: &TransferManager) -> Vec<TransferState> {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let statuses = manager.statuses();
        if !statuses.is_empty() && statuses.iter().all(|status| status.state.is_terminal()) {
            return statuses.into_iter().map(|status| status.state).collect();
        }
        assert!(
            Instant::now() < deadline,
            "transfers did not finish: {:?}",
            statuses
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn assert_all_done(states: &[TransferState]) {
    for state in states {
        match state {
            TransferState::Done => {}
            other => panic!("transfer did not succeed: {:?}", other),
        }
    }
}

fn read_local(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

fn sftp_read_dir(sftp: &wezterm_ssh::Sftp, dir: &str) -> Vec<String> {
    smol::block_on(async {
        sftp.read_dir(dir)
            .await
            .unwrap_or_else(|err| panic!("read_dir {}: {}", dir, err))
            .into_iter()
            .map(|(path, _)| path.file_name().unwrap_or_default().to_string())
            .collect()
    })
}

#[test]
fn upload_then_download_a_nested_tree() {
    if !sshd_available() {
        eprintln!("skipping: no sshd binary");
        return;
    }
    let Some(server) = Server::spawn() else {
        eprintln!("skipping: could not start sshd");
        return;
    };
    let Some(session) = server.connect() else {
        eprintln!("skipping: could not authenticate against the test sshd");
        return;
    };
    let sftp = session.sftp();

    // Local source tree: a nested directory, a file with the execute bit,
    // and a payload big enough to need more than one chunk.
    let work = TempDir::new().unwrap();
    let src = work.path().join("src");
    std::fs::create_dir_all(src.join("nested/inner")).unwrap();
    std::fs::write(src.join("top.txt"), b"top\n").unwrap();
    std::fs::write(src.join("nested/inner/deep.txt"), b"deep\n").unwrap();
    let script = src.join("run.sh");
    std::fs::write(&script, b"#!/bin/sh\necho hi\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let blob = src.join("blob.bin");
    let payload: Vec<u8> = (0..600_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&blob, &payload).unwrap();

    // Remote destination, in a fresh directory under the same tmp tree
    // the sshd fixtures already own.
    let remote_root = format!("{}", server.tmp.path().join("remote").display());
    smol::block_on(async {
        sftp.create_dir(&remote_root, 0o755).await.unwrap();
    });

    let manager = TransferManager::new(session.clone());
    // Upload each file the way the browser's recursive scan queues them,
    // including a path whose parent directory does not exist yet.
    let uploads: Vec<(PathBuf, String)> = vec![
        (src.join("top.txt"), format!("{remote_root}/top.txt")),
        (
            src.join("nested/inner/deep.txt"),
            format!("{remote_root}/nested/inner/deep.txt"),
        ),
        (script.clone(), format!("{remote_root}/run.sh")),
        (blob.clone(), format!("{remote_root}/blob.bin")),
    ];
    for (local, remote) in &uploads {
        manager.upload(local.clone(), remote.clone(), false);
    }
    assert_all_done(&wait_for_all(&manager));

    // Content landed.
    for name in ["top.txt", "nested/inner/deep.txt", "blob.bin"] {
        let remote = format!("{remote_root}/{name}");
        let bytes = smol::block_on(async {
            use smol::io::AsyncReadExt;
            let mut file = sftp.open(remote.as_str()).await.unwrap();
            let mut out = Vec::new();
            file.read_to_end(&mut out).await.unwrap();
            out
        });
        let expected = read_local(&work.path().join("src").join(name));
        assert_eq!(bytes, expected, "content mismatch for {name}");
    }

    // The execute bit rode the OPEN request.
    let mode = smol::block_on(async {
        sftp.metadata(format!("{remote_root}/run.sh").as_str())
            .await
            .unwrap()
            .permissions
            .unwrap()
            .to_unix_mode()
            & 0o777
    });
    assert_eq!(mode & 0o111, 0o111, "execute bits lost, mode {mode:o}");

    // No part files left behind: the publish rename consumed them.
    let entries = sftp_read_dir(&sftp, &remote_root);
    assert!(
        !entries.iter().any(|name| name.starts_with(".kaku-part.")),
        "part files left behind: {:?}",
        entries
    );

    // Download the same tree shape back: this exercises the local
    // parent-directory creation for nested destinations.
    let dest = work.path().join("back");
    std::fs::create_dir_all(&dest).unwrap();
    let downloads: Vec<(String, PathBuf)> = vec![
        (format!("{remote_root}/top.txt"), dest.join("top.txt")),
        (
            format!("{remote_root}/nested/inner/deep.txt"),
            dest.join("nested/inner/deep.txt"),
        ),
        (format!("{remote_root}/blob.bin"), dest.join("blob.bin")),
    ];
    for (remote, local) in &downloads {
        manager.download(remote.clone(), local.clone(), false);
    }
    assert_all_done(&wait_for_all(&manager));

    assert_eq!(read_local(&dest.join("top.txt")), b"top\n");
    assert_eq!(read_local(&dest.join("nested/inner/deep.txt")), b"deep\n");
    assert_eq!(read_local(&dest.join("blob.bin")), payload);
    assert!(
        !dest.join(".kaku-part.blob.bin").exists(),
        "download left a part file"
    );
}

/// One small file must cost four SFTP requests: OPEN, WRITE, CLOSE and
/// the publish RENAME.  Every request is one round trip, so a per-file
/// setstat or fsync is exactly what makes a folder of small files feel
/// slow over a link with any latency.
#[test]
fn small_upload_uses_four_round_trips() {
    if !sshd_available() {
        return;
    }
    capture_logging();
    let Some(server) = Server::spawn() else {
        return;
    };
    let Some(session) = server.connect_verbose() else {
        return;
    };
    let sftp = session.sftp();
    let work = TempDir::new().unwrap();
    let remote_root = format!("{}", work.path().join("remote").display());
    smol::block_on(async { sftp.create_dir(&remote_root, 0o755).await.unwrap() });
    let remote = format!("{remote_root}/one.txt");

    let src = work.path().join("one.txt");
    std::fs::write(&src, b"hello\n").unwrap();

    // Warm up: the first upload also probes the parent directories and
    // initializes the sftp subsystem, which are one-off costs.
    let manager = TransferManager::new(session.clone());
    manager.upload(src.clone(), remote.clone(), false);
    assert_all_done(&wait_for_all(&manager));

    let before = channel_writes();
    manager.upload(src.clone(), remote.clone(), false);
    assert_all_done(&wait_for_all(&manager));
    // The manager's own worker thread has finished by now, so the packet
    // count is stable.
    std::thread::sleep(Duration::from_millis(200));
    let sent = channel_writes() - before;

    // Uploading again over the same path must not add a setstat or an
    // fsync: the mode rides on OPEN and the writes are already acked.
    let before = channel_writes();
    manager.upload(src.clone(), remote.clone(), false);
    assert_all_done(&wait_for_all(&manager));
    std::thread::sleep(Duration::from_millis(200));
    let sent_again = channel_writes() - before;

    assert_eq!(
        (sent, sent_again),
        (4, 4),
        "expected four requests (open, write, close, rename) per file"
    );
}

/// Channel data packets the client has sent.  One SFTP request of this
/// size is one packet, so this counts round trips.
fn channel_writes() -> usize {
    LOG_LINES
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains("packet: wrote [type=94"))
        .count()
}
