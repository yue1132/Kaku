//! Manual benchmark of the transfer engine against a real ssh host.
//!
//! Ignored by default and host-agnostic: it only runs when the address is
//! supplied, so no machine name or path lives in the repository.
//!
//! ```text
//! KAKU_BENCH_HOST=user@host cargo test -p kaku-gui --test remote_bench -- \
//!     --ignored --nocapture
//! ```
//!
//! It reports the round trip the library actually sees, the throughput of
//! one large file (both the single-stream path and the parallel one), and
//! the cost of a folder of small files, which is dominated by round trips
//! rather than bandwidth.

use kaku_gui_lib::sftp_transfer::{TransferManager, TransferState};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use wezterm_ssh::{Session, SessionEvent};

const MB: usize = 1024 * 1024;

fn bench_host() -> Option<String> {
    std::env::var("KAKU_BENCH_HOST")
        .ok()
        .filter(|s| !s.is_empty())
}

fn connect(target: &str) -> Option<Session> {
    let (user, host) = match target.split_once('@') {
        Some((user, host)) => (Some(user), host),
        None => (None, target),
    };
    let config = kaku_gui_lib::sftp_sessions::build_config_for_target(user, host);
    let (session, events) = Session::connect(config).ok()?;
    let authenticated = smol::block_on(async {
        let deadline = Instant::now() + Duration::from_secs(30);
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
                Some(SessionEvent::Authenticate(auth)) => {
                    let answers = vec![String::new(); auth.prompts.len()];
                    if auth.answer(answers).await.is_err() {
                        return false;
                    }
                }
                Some(SessionEvent::Error(err)) => {
                    eprintln!("BENCH error: {err}");
                    return false;
                }
                None => return false,
                Some(_) => {}
            }
        }
    });
    authenticated.then_some(session)
}

/// Mean round trip for one sftp request, as the library sees it.
fn measure_rtt(session: &Session) -> f64 {
    let sftp = session.sftp();
    smol::block_on(async {
        // One warm-up call so the subsystem is initialized.
        sftp.metadata(".").await.ok();
        let start = Instant::now();
        const N: u32 = 20;
        for _ in 0..N {
            let _ = sftp.metadata(".").await;
        }
        start.elapsed().as_secs_f64() / N as f64 * 1000.0
    })
}

fn wait_all(manager: &TransferManager) -> Vec<TransferState> {
    let deadline = Instant::now() + Duration::from_secs(900);
    loop {
        let statuses = manager.statuses();
        if !statuses.is_empty() && statuses.iter().all(|s| s.state.is_terminal()) {
            return statuses.into_iter().map(|s| s.state).collect();
        }
        assert!(Instant::now() < deadline, "transfer timed out");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn check_done(states: &[TransferState]) {
    for state in states {
        assert!(matches!(state, TransferState::Done), "{:?}", state);
    }
}

#[test]
#[ignore]
fn bench_remote_engine() {
    let Some(target) = bench_host() else {
        panic!("set KAKU_BENCH_HOST=user@host");
    };
    let Some(session) = connect(&target) else {
        panic!("could not authenticate against {}", target);
    };
    let sftp = session.sftp();

    // Can we open the auxiliary connections the parallel path wants?
    let helpers = kaku_gui_lib::sftp_sessions::connect_noninteractive(&target);
    eprintln!(
        "BENCH helpers: {}",
        match &helpers {
            Ok(_) => "available (parallel path can use extra connections)",
            Err(err) => err.as_str(),
        }
    );

    let rtt_ms = measure_rtt(&session);
    eprintln!("BENCH round trip: {rtt_ms:.1} ms per sftp request");

    let dir = PathBuf::from(
        std::env::var("KAKU_BENCH_DIR").unwrap_or_else(|_| "/tmp/kaku-engine-bench".into()),
    );
    let remote = dir.display().to_string();
    smol::block_on(async {
        sftp.create_dir(&remote, 0o755).await.ok();
    });

    let work = TempDir::new().unwrap();
    let manager = TransferManager::new(session.clone());
    manager.set_helper_target(target.clone());

    // One large file, above the parallel threshold (default 16MiB).
    let big = work.path().join("big.bin");
    let payload: Vec<u8> = (0..64 * MB).map(|i| (i % 251) as u8).collect();
    std::fs::write(&big, &payload).unwrap();
    let remote_big = format!("{remote}/big.bin");
    let start = Instant::now();
    manager.upload(big.clone(), remote_big.clone(), false);
    check_done(&wait_all(&manager));
    let up = 64.0 / start.elapsed().as_secs_f64();
    eprintln!("BENCH upload 64MiB: {up:.1} MB/s");

    let back = work.path().join("big.back.bin");
    let start = Instant::now();
    manager.download(remote_big.clone(), back.clone(), false);
    check_done(&wait_all(&manager));
    let down = 64.0 / start.elapsed().as_secs_f64();
    eprintln!("BENCH download 64MiB: {down:.1} MB/s");
    assert_eq!(std::fs::read(&back).unwrap(), payload, "download corrupt");

    // A folder of small files: this is round-trip bound, not bandwidth
    // bound, so the per-file request count is what matters.
    const COUNT: usize = 200;
    let small = work.path().join("small");
    std::fs::create_dir_all(&small).unwrap();
    for i in 0..COUNT {
        std::fs::write(small.join(format!("f{i}.txt")), b"hello\n").unwrap();
    }
    let start = Instant::now();
    let mut ids = Vec::new();
    for i in 0..COUNT {
        let local = small.join(format!("f{i}.txt"));
        let remote_file = format!("{remote}/small/f{i}.txt");
        ids.push(manager.upload(local, remote_file, false));
    }
    assert_eq!(ids.len(), COUNT);
    let mut remaining = COUNT;
    let deadline = Instant::now() + Duration::from_secs(900);
    while remaining > 0 {
        remaining = manager
            .statuses()
            .iter()
            .filter(|s| !s.state.is_terminal())
            .count();
        assert!(Instant::now() < deadline, "small files timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
    let elapsed = start.elapsed().as_secs_f64();
    eprintln!(
        "BENCH {COUNT} small files: {elapsed:.1} s ({:.0} files/s, {:.0} ms per file)",
        COUNT as f64 / elapsed,
        elapsed / COUNT as f64 * 1000.0
    );

    // Clean up the remote scratch directory.
    smol::block_on(async {
        let _ = sftp.remove_file(remote_big.to_string()).await;
        for i in 0..COUNT {
            let _ = sftp.remove_file(format!("{remote}/small/f{i}.txt")).await;
        }
        let _ = sftp.remove_dir(format!("{remote}/small")).await;
        let _ = sftp.remove_dir(&remote).await;
    });
    eprintln!("BENCH cleaned up {remote}");
}

/// The same small-file batch spread over several ssh connections, which
/// is what FileZilla/WinSCP do for a queue of files.  Measures the
/// ceiling a multi-lane engine could reach before any code changes.
#[test]
#[ignore]
fn bench_multi_session_small_files() {
    let Some(target) = bench_host() else {
        panic!("set KAKU_BENCH_HOST=user@host");
    };
    let Some(session) = connect(&target) else {
        panic!("could not authenticate against {}", target);
    };
    let mut sessions = vec![session.clone()];
    for _ in 0..3 {
        if let Ok(extra) = kaku_gui_lib::sftp_sessions::connect_noninteractive(&target) {
            sessions.push(extra);
        }
    }
    let lanes = sessions.len();
    let sftp = session.sftp();
    let remote = "/tmp/kaku-engine-bench-lanes".to_string();
    smol::block_on(async {
        sftp.create_dir(&remote, 0o755).await.ok();
    });

    const COUNT: usize = 200;
    let work = TempDir::new().unwrap();
    let small = work.path().join("small");
    std::fs::create_dir_all(&small).unwrap();
    for i in 0..COUNT {
        std::fs::write(small.join(format!("f{i}.txt")), b"hello\n").unwrap();
    }

    let managers: Vec<TransferManager> = sessions
        .iter()
        .map(|s| TransferManager::new(s.clone()))
        .collect();
    let start = Instant::now();
    let mut counts = vec![0usize; lanes];
    for i in 0..COUNT {
        let lane = i % lanes;
        let local = small.join(format!("f{i}.txt"));
        let remote_file = format!("{remote}/f{i}.txt");
        managers[lane].upload(local, remote_file, false);
        counts[lane] += 1;
    }
    let deadline = Instant::now() + Duration::from_secs(900);
    loop {
        let pending: usize = managers
            .iter()
            .map(|m| {
                m.statuses()
                    .iter()
                    .filter(|s| !s.state.is_terminal())
                    .count()
            })
            .sum();
        if pending == 0 {
            break;
        }
        assert!(Instant::now() < deadline, "small files timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
    let elapsed = start.elapsed().as_secs_f64();
    eprintln!(
        "BENCH lanes={lanes} {COUNT} small files: {elapsed:.1} s ({:.0} files/s, {:.0} ms per file)",
        COUNT as f64 / elapsed,
        elapsed / COUNT as f64 * 1000.0
    );

    smol::block_on(async {
        for i in 0..COUNT {
            let _ = sftp.remove_file(format!("{remote}/f{i}.txt")).await;
        }
        let _ = sftp.remove_dir(&remote).await;
    });
    eprintln!("BENCH cleaned up {remote}");
}
