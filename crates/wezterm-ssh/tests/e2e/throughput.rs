//! Local throughput probe for the sftp data path, run against a real
//! sshd on loopback so the numbers show per-packet cost rather than the
//! network's round trip.
//!
//! Report-only: it prints MB/s per chunk size and never asserts a rate,
//! because CI machines vary.  It exists so a change to the transfer
//! engine can be compared against a known baseline.
//!
//! Skips itself when no sshd binary is available, like the rest of the
//! e2e suite.

use crate::sshd::*;
use assert_fs::prelude::*;
use assert_fs::TempDir;
use rstest::*;
use smol::io::{AsyncReadExt, AsyncWriteExt};
use std::time::Instant;
use wezterm_ssh::{OpenFileType, OpenOptions, Sftp, WriteMode};

const MB: usize = 1024 * 1024;
/// Payload per measurement; big enough to swamp setup costs.
const SIZE: usize = 64 * MB;

fn payload() -> Vec<u8> {
    (0..SIZE).map(|i| (i % 251) as u8).collect()
}

fn write_with_chunks(sftp: &Sftp, path: &str, data: &[u8], chunk: usize) -> f64 {
    smol::block_on(async {
        let start = Instant::now();
        let mut file = sftp
            .open_with_mode(
                path,
                OpenOptions {
                    read: false,
                    write: Some(WriteMode::Write),
                    mode: 0o644,
                    ty: OpenFileType::File,
                },
            )
            .await
            .expect("open for write");
        for piece in data.chunks(chunk) {
            file.write_all(piece).await.expect("write");
        }
        file.close().await.expect("close");
        data.len() as f64 / MB as f64 / start.elapsed().as_secs_f64()
    })
}

fn read_with_chunks(sftp: &Sftp, path: &str, chunk: usize) -> f64 {
    smol::block_on(async {
        let start = Instant::now();
        let mut file = sftp.open(path).await.expect("open for read");
        let mut buf = vec![0u8; chunk];
        let mut total = 0usize;
        loop {
            let n = file.read(&mut buf).await.expect("read");
            if n == 0 {
                break;
            }
            total += n;
        }
        total as f64 / MB as f64 / start.elapsed().as_secs_f64()
    })
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
fn sftp_throughput_by_chunk_size(#[future] session: SessionWithSshd) {
    if !sshd_available() {
        return;
    }
    let session: SessionWithSshd = smol::block_on(session);
    let data = payload();
    let sftp = session.sftp();
    let dir = TempDir::new().unwrap();
    let path = dir.child("probe.bin").path().to_string_lossy().to_string();

    // 32KiB is what a server without limits@openssh.com caps at, 261120
    // is OpenSSH's advertised max_write_length, 1MiB is beyond it.
    for chunk in [32 * 1024, 64 * 1024, 128 * 1024, 261_120, MB] {
        let up = write_with_chunks(&sftp, &path, &data, chunk);
        let down = read_with_chunks(&sftp, &path, chunk);
        eprintln!(
            "THROUGHPUT chunk={:>7} up={:>8.1} MB/s down={:>8.1} MB/s",
            chunk, up, down
        );
    }
    eprintln!("THROUGHPUT payload={} MB", SIZE / MB);
}
