/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Self-applied OS confinement backend for the content process.
//!
//! On Linux x86-64 this replaces gaol with two maintained, *self-applied*
//! mechanisms that require neither `unshare(CLONE_NEWUSER)`, a namespace, a
//! chroot, nor `capset` (so the AppArmor mediation that EPERM-panics gaol is
//! never on the code path):
//!
//! * **Landlock** (rust-landlock 0.4.5) in enforce mode: filesystem READ rules
//!   plus `AccessNet::{BindTcp, ConnectTcp}` *handled with no allow rules*, which
//!   denies all TCP bind/connect (content does no sockets; net is brokered to the
//!   parent). `CompatLevel::BestEffort` means old kernels silently downgrade
//!   instead of erroring.
//! * **seccomp** (seccompiler 0.5.0) installed with a DEFAULT (mismatch) action of
//!   `SeccompAction::Log` (`SECCOMP_RET_LOG`, audit-not-kill). A reasonable
//!   allow-list is built, but because nothing is killed yet this is bring-up /
//!   harvest posture; the Errno/Kill ramp is a later, out-of-scope tail.
//!
//! Every fallible step logs and continues -- this module **never panics** (the
//! anti-gaol requirement). [`apply_sandbox`] returns a [`SandboxOutcome`] that the
//! caller inspects (and the `--sandbox-selftest` harness asserts on); it is never
//! `.expect()`ed.
//!
//! The non-Linux-x86-64 fallbacks here are intentionally inert; macOS/other cfg
//! branches keep using gaol via `sandboxing.rs` for now.

use std::path::PathBuf;

/// A backend-neutral description of the confinement to install in the content
/// process. Built once by [`content_process_policy`] and consumed by
/// [`apply_sandbox`]; the `--sandbox-selftest` harness applies the *same* value
/// so the proof has no policy drift.
#[derive(Clone, Debug, Default)]
pub struct Policy {
    /// Paths to which read (file + dir listing) access is granted.
    pub fs_read: Vec<PathBuf>,
    /// Paths to which read + execute access is granted (self-exe, `.so` dirs,
    /// GStreamer plugin dir): needed for re-exec and `dlopen`.
    pub fs_exec: Vec<PathBuf>,
    // Network: all TCP bind/connect is denied (deny-all-TCP is implied; there is
    // no allow-list because the content process opens no sockets).
}

/// The result of attempting to install the sandbox. Inspected and logged by the
/// caller; never `.expect()`ed. `degraded == true` means at least one layer was
/// not fully enforced (old kernel, missing LSM, container restriction, or a
/// fallible step that logged-and-continued).
#[derive(Debug)]
pub struct SandboxOutcome {
    /// Landlock enforcement status, if a ruleset was created and `restrict_self`
    /// was reached. `None` means Landlock was not attempted or errored before a
    /// status could be produced. On non-Linux-x86-64 builds this is always
    /// `None` (Landlock is a `u8` placeholder type, see below).
    pub landlock: Option<LandlockStatus>,
    /// Whether the seccomp filter was successfully installed.
    pub seccomp_applied: bool,
    /// `true` if confinement is not fully enforced (see struct docs).
    pub degraded: bool,
}

// On linux x86-64 `LandlockStatus` is the real `landlock::RulesetStatus`. On
// every other target we expose a trivial placeholder so the public
// `SandboxOutcome` type (and any caller match) still compiles cross-platform
// while the actual confinement keeps using gaol via `sandboxing.rs`.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub use landlock::RulesetStatus as LandlockStatus;
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub type LandlockStatus = u8;

// ---------------------------------------------------------------------------
// Policy construction (platform-independent: just gathers candidate paths).
// ---------------------------------------------------------------------------

/// Build the content-process policy from the embedder resource hooks plus the
/// §4.2 baseline. Missing paths are *not* an error here; they are filtered out
/// at apply time (`PathFd::new` fails on absent paths and is dropped).
///
/// Sourced from `embedder_traits::resources::sandbox_access_files()` /
/// `sandbox_access_files_dirs()` (the same hooks gaol's profile used, so the
/// embedder resource plumbing needs zero change) plus built-in baseline paths.
pub fn content_process_policy() -> Policy {
    use embedder_traits::resources;

    let mut fs_read: Vec<PathBuf> = Vec::new();
    let mut fs_exec: Vec<PathBuf> = Vec::new();

    // Randomness. (getrandom(2) is also allowed by the seccomp list; this covers
    // the /dev/urandom fallback path.)
    fs_read.push(PathBuf::from("/dev/urandom"));

    // Embedder resources: HSTS list, public-suffix list, bad-cert HTML, UA
    // stylesheets, etc. Literal files are read-only; dirs get read + listing.
    fs_read.extend(resources::sandbox_access_files());
    fs_read.extend(resources::sandbox_access_files_dirs());

    // The running binary's own directory: re-exec of self + dlopen of co-located
    // `.so`s. Needs execute.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            fs_exec.push(dir.to_path_buf());
        }
    }

    // System library dirs (freetype/fontconfig/harfbuzz/ICU/GStreamer dlopen).
    // Needs execute. Multiarch dir included for Debian/Ubuntu layouts.
    for lib in [
        "/usr/lib",
        "/lib",
        "/usr/lib/x86_64-linux-gnu",
        "/lib/x86_64-linux-gnu",
        "/usr/lib64",
        "/lib64",
    ] {
        fs_exec.push(PathBuf::from(lib));
    }

    // GStreamer plugin dir (media-gstreamer is ON in NavGator's build): plugin
    // `.so` dlopen. Needs execute.
    fs_exec.push(PathBuf::from("/usr/lib/x86_64-linux-gnu/gstreamer-1.0"));
    fs_exec.push(PathBuf::from("/usr/lib/gstreamer-1.0"));
    fs_exec.push(PathBuf::from("/usr/lib64/gstreamer-1.0"));

    // System font dirs: local font byte-loading happens in-content (design A).
    // Read + listing only (no execute).
    for font_dir in [
        "/usr/share/fonts",
        "/usr/local/share/fonts",
        "/var/cache/fontconfig",
        "/etc/fonts",
    ] {
        fs_read.push(PathBuf::from(font_dir));
    }
    if let Some(home) = home_dir() {
        fs_read.push(home.join(".fonts"));
        fs_read.push(home.join(".local/share/fonts"));
        fs_read.push(home.join(".cache/fontconfig"));
    }

    Policy { fs_read, fs_exec }
}

/// Best-effort `$HOME` lookup without pulling in extra deps. Returns `None` if
/// unset/empty rather than guessing, so we never widen the policy to a wrong dir.
fn home_dir() -> Option<PathBuf> {
    match std::env::var_os("HOME") {
        Some(h) if !h.is_empty() => Some(PathBuf::from(h)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Apply: Linux x86-64 -- real Landlock + seccomp.
// ---------------------------------------------------------------------------

/// Install the sandbox in the *calling thread/process*. Must be invoked at the
/// existing child hook (after IPC bootstrap, before `script::init()`), so JIT
/// bring-up and all spawned threads run inside the cage.
///
/// Never panics: every fallible step logs and continues, marking the outcome
/// `degraded` when confinement is not fully enforced.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn apply_sandbox(policy: &Policy) -> SandboxOutcome {
    let (landlock, landlock_degraded) = apply_landlock(policy);
    let seccomp_applied = apply_seccomp();

    // Degraded if Landlock did not fully enforce, or seccomp failed to install.
    let landlock_full = matches!(landlock, Some(LandlockStatus::FullyEnforced));
    let degraded = landlock_degraded || !landlock_full || !seccomp_applied;

    SandboxOutcome {
        landlock,
        seccomp_applied,
        degraded,
    }
}

/// Build + enforce the Landlock ruleset. Returns the enforcement status (if a
/// status was produced) and whether anything degraded along the way.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn apply_landlock(policy: &Policy) -> (Option<LandlockStatus>, bool) {
    use landlock::{
        ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset,
        RulesetAttr, RulesetCreatedAttr, RulesetStatus,
    };

    // Request the highest ABI we know about; BestEffort downgrades on older
    // kernels (e.g. AccessNet is dropped below v4 / kernel 6.7).
    let abi = ABI::V4;
    let read_access = AccessFs::from_read(abi); // Execute | ReadFile | ReadDir

    let mut degraded = false;

    // 1. Handle the FS read access types and the two TCP net access types. With
    //    NO net allow-rules added below, all TCP bind/connect is denied.
    let ruleset = match Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(read_access)
        .and_then(|r| r.handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp))
    {
        Ok(r) => r,
        Err(e) => {
            log::warn!("Landlock: failed to configure handled accesses: {e}");
            return (None, true);
        },
    };

    // 2. Create the in-kernel ruleset.
    let mut created = match ruleset.create() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("Landlock: failed to create ruleset (continuing unconfined by FS/net): {e}");
            return (None, true);
        },
    };

    // 3. Read-only rules (file read + dir listing).
    for path in &policy.fs_read {
        let fd = match PathFd::new(path) {
            Ok(fd) => fd,
            // Absent/unreadable path: skip gracefully, do not fail the sandbox.
            Err(_) => continue,
        };
        created = match created.add_rule(PathBeneath::new(fd, read_access)) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("Landlock: failed to add read rule for {}: {e}", path.display());
                degraded = true;
                // `add_rule` consumes `self`; recreate the ruleset cheaply by
                // restarting is not worth it. Best-effort: abandon and enforce
                // what we have so far via a fresh restrict on the empty handled
                // set is impossible here, so just stop adding and proceed.
                // (We cannot recover `created`; bail to restrict_self below is
                // not reachable, so return degraded with no status.)
                return (None, true);
            },
        };
    }

    // 4. Read + execute rules (self-exe dir, lib dirs, gstreamer plugin dir).
    //    `from_read(abi)` already includes Execute, so the same access set
    //    grants read+exec; keeping the lists separate is documentation of
    //    intent (and lets a future tightening drop ReadDir from exec dirs).
    let exec_access = AccessFs::Execute | AccessFs::ReadFile | AccessFs::ReadDir;
    for path in &policy.fs_exec {
        let fd = match PathFd::new(path) {
            Ok(fd) => fd,
            Err(_) => continue,
        };
        created = match created.add_rule(PathBeneath::new(fd, exec_access)) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("Landlock: failed to add exec rule for {}: {e}", path.display());
                return (None, true);
            },
        };
    }

    // 5. Enforce. Inspect RestrictionStatus.ruleset; never `.expect()`.
    match created.restrict_self() {
        Ok(status) => {
            match status.ruleset {
                RulesetStatus::FullyEnforced => {
                    log::info!("Landlock: fully enforced (FS read + TCP denied)");
                },
                RulesetStatus::PartiallyEnforced => {
                    log::warn!(
                        "Landlock: partially enforced (kernel older than requested ABI; \
                         some access types dropped -- seccomp must carry the rest)"
                    );
                    degraded = true;
                },
                RulesetStatus::NotEnforced => {
                    log::warn!(
                        "Landlock: not enforced (kernel <5.13 or LSM not active); \
                         relying on seccomp only"
                    );
                    degraded = true;
                },
            }
            if !status.no_new_privs {
                log::warn!("Landlock: PR_SET_NO_NEW_PRIVS not set");
                degraded = true;
            }
            (Some(status.ruleset), degraded)
        },
        Err(e) => {
            log::warn!("Landlock: restrict_self failed (continuing): {e}");
            (None, true)
        },
    }
}

/// Build + install the seccomp filter with DEFAULT (mismatch) action = `Log`.
/// Allowed syscalls map to an empty rule vec -> they *match* -> `Allow`; every
/// other syscall hits the mismatch action `Log` (audit-not-kill). Returns
/// whether the filter was installed.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn apply_seccomp() -> bool {
    use std::collections::BTreeMap;
    use std::convert::TryInto;

    use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, SeccompRule, TargetArch};

    // Allow-list (§4.2). Empty rule vec == "allow regardless of args". `socket`
    // is allowed unconditionally here in Log mode (arg-restriction to AF_UNIX is
    // a later, enforce-mode tightening); Landlock already denies TCP bind/connect.
    let allow: &[libc::c_long] = &[
        // --- IPC / shared memory (ipc-channel transport) ---
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_sendmsg,
        libc::SYS_sendmmsg,
        libc::SYS_recvmsg,
        libc::SYS_recvmmsg,
        libc::SYS_recvfrom,
        libc::SYS_sendto,
        libc::SYS_memfd_create,
        libc::SYS_ftruncate,
        libc::SYS_mmap,
        libc::SYS_munmap,
        libc::SYS_mremap,
        libc::SYS_mprotect, // incl. PROT_EXEC: SpiderMonkey JIT W^X needs this
        libc::SYS_madvise,
        libc::SYS_close,
        libc::SYS_dup,
        libc::SYS_dup2,
        libc::SYS_dup3,
        libc::SYS_fcntl,
        // --- threading / runtime ---
        libc::SYS_clone,
        libc::SYS_clone3,
        libc::SYS_futex,
        libc::SYS_set_robust_list,
        libc::SYS_get_robust_list,
        libc::SYS_rseq,
        libc::SYS_membarrier,
        libc::SYS_sched_getaffinity,
        libc::SYS_sched_setaffinity,
        libc::SYS_sched_yield,
        libc::SYS_nanosleep,
        libc::SYS_clock_nanosleep,
        libc::SYS_clock_gettime,
        libc::SYS_clock_getres,
        libc::SYS_gettimeofday,
        libc::SYS_epoll_create1,
        libc::SYS_epoll_ctl,
        libc::SYS_epoll_wait,
        libc::SYS_epoll_pwait,
        libc::SYS_eventfd2,
        libc::SYS_poll,
        libc::SYS_ppoll,
        libc::SYS_pipe2,
        libc::SYS_brk,
        libc::SYS_getrandom,
        libc::SYS_getuid,
        libc::SYS_geteuid,
        libc::SYS_getgid,
        libc::SYS_getegid,
        libc::SYS_getpid,
        libc::SYS_gettid,
        libc::SYS_getppid,
        libc::SYS_prctl,
        libc::SYS_arch_prctl,
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_sigaltstack,
        libc::SYS_tgkill, // BHM sampler (if compiled in) / panic abort
        libc::SYS_restart_syscall,
        libc::SYS_exit,
        libc::SYS_exit_group,
        // --- file read set (Landlock gates *which* paths) ---
        libc::SYS_openat,
        libc::SYS_openat2,
        libc::SYS_read,
        libc::SYS_pread64,
        libc::SYS_readv,
        libc::SYS_preadv,
        libc::SYS_lseek,
        libc::SYS_fstat,
        libc::SYS_stat,
        libc::SYS_lstat,
        libc::SYS_statx,
        libc::SYS_newfstatat,
        libc::SYS_readlink,
        libc::SYS_readlinkat,
        libc::SYS_access,
        libc::SYS_faccessat,
        libc::SYS_faccessat2,
        libc::SYS_getdents64,
        libc::SYS_getcwd,
        libc::SYS_ioctl, // narrowed to a device-control set in enforce mode
        libc::SYS_write, // stderr/log + IPC fds (Landlock/parent gate targets)
        libc::SYS_writev,
        libc::SYS_pwrite64,
        libc::SYS_fsync,
        libc::SYS_fdatasync,
    ];

    let rules: BTreeMap<i64, Vec<SeccompRule>> =
        allow.iter().map(|&nr| (nr as i64, Vec::new())).collect();

    let target_arch: TargetArch = match std::env::consts::ARCH.try_into() {
        Ok(a) => a,
        Err(e) => {
            log::warn!("seccomp: unsupported target arch {}: {e}", std::env::consts::ARCH);
            return false;
        },
    };

    // DEFAULT (mismatch) action = Log: audit, do NOT kill. Matched (allowed)
    // syscalls => Allow. Log != Allow, so SeccompFilter::validate() is satisfied.
    let filter = match SeccompFilter::new(
        rules,
        SeccompAction::Log,   // mismatch_action: default for un-listed syscalls
        SeccompAction::Allow, // match_action: listed syscalls
        target_arch,
    ) {
        Ok(f) => f,
        Err(e) => {
            log::warn!("seccomp: failed to build filter (continuing): {e}");
            return false;
        },
    };

    let program: BpfProgram = match filter.try_into() {
        Ok(p) => p,
        Err(e) => {
            log::warn!("seccomp: failed to compile BPF program (continuing): {e}");
            return false;
        },
    };

    // apply_filter sets PR_SET_NO_NEW_PRIVS then seccomp(SECCOMP_SET_MODE_FILTER).
    match seccompiler::apply_filter(&program) {
        Ok(()) => {
            log::info!("seccomp: filter installed in Log (audit) mode");
            true
        },
        Err(e) => {
            log::warn!("seccomp: apply_filter failed (continuing): {e}");
            false
        },
    }
}

// ---------------------------------------------------------------------------
// Apply: non-Linux-x86-64 -- inert (macOS/other keep gaol via sandboxing.rs).
// ---------------------------------------------------------------------------

/// On targets other than Linux x86-64 this backend does nothing; OS confinement
/// there continues to flow through gaol in `sandboxing.rs`. Reports a fully
/// degraded, no-op outcome so callers stay uniform.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub fn apply_sandbox(_policy: &Policy) -> SandboxOutcome {
    log::warn!("sandbox_backend: Landlock/seccomp backend is Linux x86-64 only; no-op here");
    SandboxOutcome {
        landlock: None,
        seccomp_applied: false,
        degraded: true,
    }
}
