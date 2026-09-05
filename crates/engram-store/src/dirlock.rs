//! The data-directory lock — one writer per store, enforced.
//!
//! # What it prevents
//!
//! Two processes opening the same `--data-dir` both open the same WAL, both
//! attach a sink at its end, and both append. Their records interleave, the
//! BLAKE3 hash chain no longer verifies, and recovery refuses the log — after
//! the fact, having already accepted and acknowledged writes from both. There
//! is no repair for that beyond a restore.
//!
//! This is not a hypothetical: a restart script that does not wait for the old
//! process, a container restarting while the previous one drains, a systemd
//! unit with the wrong `Restart=` — every one of them produces two live
//! writers, and nothing in the database noticed.
//!
//! # Why a lock FILE and not an OS lock
//!
//! `flock`/`LockFileEx` would release automatically when the holder dies, which
//! is strictly better behaviour. Both need `unsafe` (or a dependency that wraps
//! it), and this workspace denies `unsafe` and keeps its dependency graph
//! small; buying automatic release with either would be a poor trade for a file
//! that is created once per process start.
//!
//! So: `create_new`, which is atomic — the OS guarantees exactly one creator —
//! and removed on clean shutdown. The cost is a STALE lock after a crash, which
//! is why the refusal message names the recorded pid and the file to delete.
//! That is the same trade, and very nearly the same message, as Postgres's
//! `postmaster.pid`.
//!
//! The failure direction is deliberate: a stale lock stops a server that could
//! have started, and the operator reads a message and deletes a file. The other
//! direction silently corrupts a database.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The lock file's name inside the data directory.
pub const LOCK_FILE: &str = "LOCK";

/// Why a data directory could not be locked.
#[derive(Debug)]
pub enum LockError {
    /// Another process holds it (or left it behind). Carries what the file
    /// said, so the operator can check whether that process is still alive.
    Held {
        /// The lock file's path.
        path: PathBuf,
        /// Whatever the holder recorded — normally its pid.
        holder: String,
    },
    /// The directory could not be created or the file could not be written.
    Io(std::io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Held { path, holder } => write!(
                f,
                "the data directory is locked by another process ({holder}).\n\
                 Two servers writing one store interleave their log records and \
                 leave a hash chain that no recovery can verify.\n\
                 If that process is NOT running, the lock is stale — remove {} and \
                 start again.",
                path.display()
            ),
            LockError::Io(e) => write!(f, "could not lock the data directory: {e}"),
        }
    }
}

impl std::error::Error for LockError {}

impl From<std::io::Error> for LockError {
    fn from(e: std::io::Error) -> Self {
        LockError::Io(e)
    }
}

/// An acquired data-directory lock. Released when dropped.
///
/// Hold it for as long as the store is open — binding it to `_` drops it
/// immediately and locks nothing, which is the one mistake this type invites.
#[derive(Debug)]
pub struct DirLock {
    path: PathBuf,
    /// Cleared by [`DirLock::leak`], so a deliberate hand-off does not delete
    /// a lock the caller still wants held.
    release: bool,
}

impl DirLock {
    /// Take the lock on `dir`, creating the directory if needed.
    ///
    /// `holder` is recorded in the file and echoed back in the refusal — pass
    /// something that identifies this process to a human reading it, normally
    /// the pid.
    pub fn acquire(dir: &Path, holder: &str) -> Result<DirLock, LockError> {
        fs::create_dir_all(dir)?;
        let path = dir.join(LOCK_FILE);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true) // ATOMIC: exactly one process wins
            .open(&path)
        {
            Ok(mut f) => {
                // Best effort — the lock is held by the file EXISTING, not by
                // its contents, so a failure to write the pid must not leave a
                // lock nobody holds.
                let _ = writeln!(f, "{holder}");
                let _ = f.sync_all();
                Ok(DirLock {
                    path,
                    release: true,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let recorded = fs::read_to_string(&path)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                // A STALE lock — left by a holder that is provably gone — is
                // taken over, where the platform lets us prove it.
                //
                // Without this, every unclean stop needs a human: a container
                // that is OOM-killed and restarted by its orchestrator would
                // come back, find its own previous incarnation's lock on the
                // persistent volume, and refuse for ever. That is not a
                // safety property, it is an outage with a good excuse.
                //
                // "Provably gone" is the bar. On Linux a pid is alive iff
                // `/proc/<pid>` exists, which std can check without `unsafe`.
                // Elsewhere there is no such check in std, so the lock is
                // refused with instructions — the conservative direction. Pid
                // reuse can make a dead holder look alive; that yields a
                // refusal, which is the safe error.
                if let Some(pid) = recorded
                    .strip_prefix("pid ")
                    .and_then(|p| p.trim().parse::<u32>().ok())
                {
                    if holder_is_provably_gone(pid) {
                        let _ = fs::remove_file(&path);
                        if let Ok(mut f) = fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&path)
                        {
                            let _ = writeln!(f, "{holder}");
                            let _ = f.sync_all();
                            return Ok(DirLock {
                                path,
                                release: true,
                            });
                        }
                        // Lost the race to another starter: fall through and
                        // report whoever holds it now.
                    }
                }
                let recorded = fs::read_to_string(&path)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                Err(LockError::Held {
                    path,
                    holder: if recorded.is_empty() {
                        "no holder recorded".into()
                    } else {
                        recorded
                    },
                })
            }
            Err(e) => Err(LockError::Io(e)),
        }
    }

    /// The lock file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Give up ownership WITHOUT releasing, for a caller that means to hold the
    /// lock past this value's lifetime.
    pub fn leak(mut self) -> PathBuf {
        self.release = false;
        self.path.clone()
    }
}

/// Whether the process that recorded `pid` is PROVABLY no longer running.
///
/// `false` means "cannot prove it", not "it is alive": the lock is then refused
/// and the operator decides. Only Linux offers a proof std can perform without
/// `unsafe` — `/proc/<pid>` exists iff the process does.
fn holder_is_provably_gone(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        !Path::new("/proc").join(pid.to_string()).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        false
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        if self.release {
            // Best effort. A failure here leaves a stale lock, which refuses a
            // later start with a message that says how to clear it — the safe
            // direction.
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("engram-dirlock-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    /// The holder is THIS process, which is provably alive on every platform.
    ///
    /// It used to be a literal `pid 4242`, which made the test
    /// platform-dependent for the exact reason
    /// `a_lock_held_by_a_live_pid_is_refused_not_stolen` warns about: on Linux
    /// that pid is almost certainly dead, so the stale-takeover path correctly
    /// reclaimed the lock and the second acquire SUCCEEDED. It passed only on
    /// Windows, where takeover is unimplemented — passing for the wrong reason,
    /// which is the failure mode this file is otherwise careful about.
    #[test]
    fn a_second_acquire_is_REFUSED_and_names_the_holder() {
        let d = tmp("second");
        let me = format!("pid {}", std::process::id());
        let first = DirLock::acquire(&d, &me).expect("first acquire");
        match DirLock::acquire(&d, "pid 9999") {
            Err(LockError::Held { holder, path }) => {
                assert_eq!(
                    holder, me,
                    "the refusal must name the RECORDED holder, so an operator can \
                     check whether it is still alive"
                );
                assert_eq!(path, d.join(LOCK_FILE));
            }
            Ok(_) => panic!(
                "a second process acquired the same data directory — two writers on \
                 one WAL interleave their records and leave an unverifiable chain"
            ),
            Err(e) => panic!("expected Held, got {e:?}"),
        }
        drop(first);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn the_lock_is_released_on_drop_so_a_restart_works() {
        let d = tmp("restart");
        {
            let _l = DirLock::acquire(&d, "pid 1").expect("first");
            assert!(d.join(LOCK_FILE).is_file(), "the lock file must exist while held");
        }
        assert!(
            !d.join(LOCK_FILE).exists(),
            "a clean shutdown must remove the lock, or every restart needs manual \
             intervention and operators learn to delete it reflexively"
        );
        DirLock::acquire(&d, "pid 2").expect("a restart must be able to re-acquire");
        let _ = fs::remove_dir_all(&d);
    }

    /// A lock whose recorded holder is ALIVE must refuse, on every platform.
    ///
    /// The holder used here is this test's own pid — the one process guaranteed
    /// to be running — so this passes for the right reason everywhere, rather
    /// than passing on Windows because takeover is unimplemented there.
    #[test]
    fn a_lock_held_by_a_live_pid_is_refused_not_stolen() {
        let d = tmp("live");
        fs::create_dir_all(&d).expect("mkdir");
        fs::write(d.join(LOCK_FILE), format!("pid {}\n", std::process::id()))
            .expect("plant a lock naming a live process");
        assert!(
            matches!(DirLock::acquire(&d, "pid 1"), Err(LockError::Held { .. })),
            "a lock naming a RUNNING process must refuse; taking it would put two \
             writers on one store"
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// A lock whose recorded holder is PROVABLY dead is taken over — on Linux,
    /// the platform where that can be proven.
    ///
    /// This is the case an orchestrator produces on every OOM-kill: the
    /// restarted container finds its own previous incarnation's lock on the
    /// volume. Refusing there is not safety, it is an outage that needs a
    /// human to type `rm`.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_lock_from_a_dead_pid_is_taken_over_on_linux() {
        let d = tmp("dead");
        fs::create_dir_all(&d).expect("mkdir");
        // Above Linux's default pid_max; no such process exists.
        fs::write(d.join(LOCK_FILE), "pid 4000000\n").expect("plant a stale lock");
        let l = DirLock::acquire(&d, "pid 1").expect(
            "a lock left by a process that provably no longer exists must be taken \
             over, or every unclean stop needs manual intervention",
        );
        assert_eq!(
            fs::read_to_string(l.path()).expect("read").trim(),
            "pid 1",
            "the taken-over lock must record the NEW holder"
        );
        drop(l);
        let _ = fs::remove_dir_all(&d);
    }

    /// Where liveness cannot be proven, a stale-looking lock still refuses —
    /// the conservative direction, with instructions in the message.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn a_stale_lock_refuses_where_liveness_cannot_be_proven() {
        let d = tmp("stale");
        fs::create_dir_all(&d).expect("mkdir");
        fs::write(d.join(LOCK_FILE), "pid 4000000\n").expect("plant a stale lock");
        match DirLock::acquire(&d, "pid 1") {
            Err(LockError::Held { .. }) => {}
            other => panic!("expected a refusal on a platform with no pid liveness check, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn acquire_creates_the_directory() {
        let d = tmp("mkdir").join("nested").join("deeper");
        let l = DirLock::acquire(&d, "pid 1").expect("acquire creates the path");
        assert!(l.path().is_file());
        drop(l);
        let _ = fs::remove_dir_all(tmp("mkdir"));
    }
}
