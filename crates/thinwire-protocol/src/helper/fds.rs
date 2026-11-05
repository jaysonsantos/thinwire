//! Start a helper with its standard streams only (ADR 0013).
//!
//! TDLib opens its sockets without close-on-exec. A child process thus gets
//! a copy of each open Telegram socket. The helper parses untrusted
//! WhatsApp and Signal data, so it must not hold the descriptors of the
//! app. A copy also keeps a connection open after TDLib closes it.
//!
//! Between fork and exec, the child marks every descriptor above 2
//! close-on-exec, so exec closes them. A mark, not a close, keeps the pipe
//! that the standard library uses to report an exec error.
//!
//! This is the one module of the workspace with `unsafe` code.
//! `scripts/check-unsafe.sh` keeps it that way.

#![allow(unsafe_code)]

/// Highest descriptor number that the child marks, when the open-file
/// limit is higher or unknown. A process of this app opens far fewer.
#[cfg(unix)]
const MARK_LIMIT: libc::c_int = 65_536;

/// Make `command` start its child with descriptors 0, 1 and 2 only.
#[cfg(unix)]
pub(crate) fn standard_streams_only(command: &mut tokio::process::Command) {
    // The parent reads the limit: the child calls nothing but `fcntl` and
    // `close_range`.
    let limit = open_file_limit();
    // SAFETY: the closure runs in the child after fork and before exec.
    // It calls only the async-signal-safe `fcntl` and the raw
    // `close_range` system call, and it allocates no memory and takes no
    // lock. It reads only the copied `limit`.
    unsafe {
        command.pre_exec(move || {
            mark_close_on_exec(limit);
            Ok(())
        });
    }
}

#[cfg(not(unix))]
pub(crate) fn standard_streams_only(_command: &mut tokio::process::Command) {}

/// The soft open-file limit, at most [`MARK_LIMIT`].
#[cfg(unix)]
fn open_file_limit() -> libc::c_int {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` writes into the valid `limit` value only.
    let read = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) };
    if read != 0 || limit.rlim_cur == libc::RLIM_INFINITY {
        return MARK_LIMIT;
    }
    libc::c_int::try_from(limit.rlim_cur)
        .unwrap_or(MARK_LIMIT)
        .min(MARK_LIMIT)
}

/// Mark the descriptors 3 to `limit` close-on-exec. Async-signal-safe.
#[cfg(unix)]
fn mark_close_on_exec(limit: libc::c_int) {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: a raw system call with plain integer arguments. Linux
        // 5.11 and later mark the whole range in one call.
        let marked = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                3_u32,
                u32::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            )
        };
        if marked == 0 {
            return;
        }
    }
    for fd in 3..limit {
        // SAFETY: `fcntl` on a number that is not open fails with EBADF
        // and changes nothing.
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::collections::BTreeSet;
    use std::os::fd::AsRawFd;
    use std::process::Stdio;

    use super::*;

    /// Put a copy of an open file at `number`, with no close-on-exec mark.
    fn plant(number: libc::c_int) -> libc::c_int {
        let null = std::fs::File::open("/dev/null").expect("/dev/null");
        // SAFETY: `dup2` on two plain descriptor numbers. The copy has no
        // close-on-exec mark: that is the leak this module stops.
        let copy = unsafe { libc::dup2(null.as_raw_fd(), number) };
        assert_eq!(copy, number, "dup2");
        copy
    }

    fn close(number: libc::c_int) {
        // SAFETY: closes a descriptor that this test planted.
        unsafe {
            libc::close(number);
        }
    }

    /// The descriptor numbers that a child sees, from `ls /dev/fd`.
    async fn child_descriptors(mark: bool) -> BTreeSet<libc::c_int> {
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .args(["-c", "ls /dev/fd/"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if mark {
            standard_streams_only(&mut command);
        }
        let output = command.output().await.expect("child");
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .filter_map(|word| word.parse().ok())
            .collect()
    }

    /// A planted descriptor reaches a plain child. With the mark, the child
    /// sees none of it.
    #[tokio::test]
    async fn a_helper_gets_no_descriptor_of_the_app() {
        let planted = plant(517);
        let plain = child_descriptors(false).await;
        let marked = child_descriptors(true).await;
        close(planted);
        assert!(plain.contains(&517), "the leak is real: {plain:?}");
        assert!(!marked.contains(&517), "the mark stops it: {marked:?}");
        for standard in [0, 1, 2] {
            assert!(marked.contains(&standard), "{standard} stays: {marked:?}");
        }
    }

    #[test]
    fn the_limit_is_positive_and_bounded() {
        let limit = open_file_limit();
        assert!(limit > 3);
        assert!(limit <= MARK_LIMIT);
    }

    /// A small fast random source. The seed is in the failure message.
    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, end: u64) -> u64 {
            self.next() % end
        }
    }

    /// Stress run of the mark. Run it from time to time with
    /// `scripts/fuzz-helper-fds.sh`. Each round plants a random set of
    /// descriptors at random numbers. Some have the mark already, some do
    /// not. Several threads start children at the same time while other
    /// threads open and close files. No child may see a planted number.
    /// `THINWIRE_FD_STRESS_ROUNDS` sets the rounds, `THINWIRE_FD_STRESS_SEED`
    /// repeats a run.
    #[test]
    #[ignore = "stress run: scripts/fuzz-helper-fds.sh"]
    fn fd_stress() {
        let rounds: u64 = std::env::var("THINWIRE_FD_STRESS_ROUNDS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(200);
        let seed: u64 = std::env::var("THINWIRE_FD_STRESS_SEED")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(1, |time| u64::from(time.subsec_nanos()) | 1)
            });
        eprintln!("fd_stress: seed {seed}, {rounds} rounds");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("runtime");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Noise: threads that open and close files while children start.
        let noise: Vec<_> = (0..2)
            .map(|_| {
                let stop = std::sync::Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let files: Vec<_> = (0..8)
                            .filter_map(|_| std::fs::File::open("/dev/null").ok())
                            .collect();
                        drop(files);
                    }
                })
            })
            .collect();
        let mut random = XorShift(seed);
        for round in 0..rounds {
            let count = random.below(24) + 1;
            let mut planted = Vec::new();
            for _ in 0..count {
                // Numbers from 40: the low numbers are for the noise and
                // for the files that `ls` opens itself.
                let number = 40 + random.below(900) as libc::c_int;
                if planted.contains(&number) {
                    continue;
                }
                plant(number);
                if random.below(2) == 0 {
                    // SAFETY: sets the mark on a descriptor this test owns.
                    unsafe {
                        libc::fcntl(number, libc::F_SETFD, libc::FD_CLOEXEC);
                    }
                }
                planted.push(number);
            }
            let children = 1 + random.below(4);
            let seen: Vec<BTreeSet<libc::c_int>> = runtime.block_on(async {
                let starts: Vec<_> = (0..children)
                    .map(|_| tokio::spawn(child_descriptors(true)))
                    .collect();
                let mut seen = Vec::new();
                for start in starts {
                    seen.push(start.await.expect("child task"));
                }
                seen
            });
            for number in &planted {
                close(*number);
            }
            for child in &seen {
                let leaked: Vec<_> = planted
                    .iter()
                    .filter(|number| child.contains(number))
                    .collect();
                assert!(
                    leaked.is_empty(),
                    "seed {seed}, round {round}: a child saw {leaked:?}"
                );
                assert!(
                    child.contains(&1),
                    "seed {seed}, round {round}: the child lost stdout"
                );
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for thread in noise {
            let _ = thread.join();
        }
    }
}
