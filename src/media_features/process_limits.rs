//! Child-only resource limits and Linux process isolation for media tools.

use std::{
    fs::{self, File, OpenOptions},
    io,
    os::fd::RawFd,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
use tokio::process::{Child, Command};
use tokio::sync::OwnedSemaphorePermit;

use crate::ApiError;

const CHILD_INPUT_FD: RawFd = 3;
const CHILD_DESCRIPTOR_LIMIT: u64 = 64;

/// Ask FFmpeg to finish its current output, then reap it before releasing a job
/// slot. A stuck decoder gets two seconds before forced termination.
pub(super) async fn stop_child(child: &mut Child) -> io::Result<std::process::ExitStatus> {
    if let Some(status) = child.try_wait()? {
        return Ok(status);
    }
    if let Some(pid) = child.id() {
        let pid = i32::try_from(pid).map_err(|_| io::Error::other("invalid media child PID"))?;
        let signaled = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        );
        if signaled.is_err_and(|error| error != nix::errno::Errno::ESRCH) {
            child.kill().await?;
            return child.wait().await;
        }
    }
    match tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await {
        Ok(status) => {
            if status.is_ok() {
                tracing::info!("media child exited after termination request");
            }
            status
        }
        Err(_) => {
            tracing::warn!("media child required forced termination after two seconds");
            child.kill().await?;
            child.wait().await
        }
    }
}

const LANDLOCK_FS_EXECUTE: u64 = 1 << 0;
const LANDLOCK_FS_WRITE_FILE: u64 = 1 << 1;
const LANDLOCK_FS_READ_FILE: u64 = 1 << 2;
const LANDLOCK_FS_READ_DIR: u64 = 1 << 3;
const LANDLOCK_FS_REMOVE_FILE: u64 = 1 << 5;
const LANDLOCK_FS_MAKE_REG: u64 = 1 << 8;
const LANDLOCK_FS_REFER: u64 = 1 << 13;
const LANDLOCK_FS_TRUNCATE: u64 = 1 << 14;
const RUNTIME_READ_ACCESS: u64 = LANDLOCK_FS_EXECUTE | LANDLOCK_FS_READ_FILE | LANDLOCK_FS_READ_DIR;
const SCRATCH_ACCESS: u64 = LANDLOCK_FS_READ_FILE
    | LANDLOCK_FS_WRITE_FILE
    | LANDLOCK_FS_READ_DIR
    | LANDLOCK_FS_REMOVE_FILE
    | LANDLOCK_FS_MAKE_REG
    | LANDLOCK_FS_REFER
    | LANDLOCK_FS_TRUNCATE;

struct LandlockRule {
    allowed_access: u64,
}

/// Paths opened by the server and converted to Landlock rules. The source
/// media remains an already-open file descriptor; no parent `/proc` path is
/// made visible to the decoder.
pub(super) struct MediaChildSandbox {
    executable: PathBuf,
    rules: Vec<LandlockRule>,
    _path_handles: Vec<File>,
    // Keep the exact validated input descriptor alive until the command has
    // completed its child spawn and inherited descriptor remap.
    _input: Option<File>,
}

impl MediaChildSandbox {
    pub(super) async fn prepare_bounded(
        executable: PathBuf,
        input: File,
        scratch: Option<PathBuf>,
        permit: OwnedSemaphorePermit,
    ) -> Result<Self, ApiError> {
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            Self::prepare_with_input(&executable, Some(input), scratch.as_deref())
        })
        .await
        .map_err(|_| ApiError::Unavailable)?
        .map_err(|_| ApiError::Unavailable)
    }

    /// Prepare a decoder that reads only from an inherited stdin pipe. This
    /// keeps network fetching in the server process while applying the same
    /// filesystem and syscall restrictions as file-backed media jobs.
    pub(super) async fn prepare_output_bounded(
        executable: PathBuf,
        scratch: Option<PathBuf>,
        permit: OwnedSemaphorePermit,
    ) -> Result<Self, ApiError> {
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            Self::prepare_with_input(&executable, None, scratch.as_deref())
        })
        .await
        .map_err(|_| ApiError::Unavailable)?
        .map_err(|_| ApiError::Unavailable)
    }

    #[cfg(test)]
    pub(super) fn prepare(
        executable: &Path,
        input: File,
        scratch: Option<&Path>,
    ) -> io::Result<Self> {
        Self::prepare_with_input(executable, Some(input), scratch)
    }

    fn prepare_with_input(
        executable: &Path,
        input: Option<File>,
        scratch: Option<&Path>,
    ) -> io::Result<Self> {
        require_landlock()?;
        if let Some(file) = &input
            && !file.metadata()?.is_file()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "media input descriptor is not a regular file",
            ));
        }

        let executable = resolve_executable(executable)?;
        let mut rules = Vec::new();
        let mut path_handles = Vec::new();

        if let Some(scratch) = scratch {
            let handle = open_path_handle(scratch)?;
            if !handle.metadata()?.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "media scratch path is not a directory",
                ));
            }
            push_rule(&mut rules, &mut path_handles, handle, SCRATCH_ACCESS);
        }

        let executable_handle = open_path_handle(&executable)?;
        if !executable_handle.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "media executable path is not a regular file",
            ));
        }
        push_rule(
            &mut rules,
            &mut path_handles,
            executable_handle,
            LANDLOCK_FS_EXECUTE | LANDLOCK_FS_READ_FILE,
        );

        for runtime_dir in [
            Path::new("/lib"),
            Path::new("/lib64"),
            Path::new("/usr/lib"),
            Path::new("/usr/local/lib"),
        ] {
            if let Ok(handle) = open_path_handle(runtime_dir)
                && handle.metadata().is_ok_and(|metadata| metadata.is_dir())
            {
                push_rule(&mut rules, &mut path_handles, handle, RUNTIME_READ_ACCESS);
            }
        }
        if let Ok(cache) = open_path_handle(Path::new("/etc/ld.so.cache"))
            && cache.metadata().is_ok_and(|metadata| metadata.is_file())
        {
            push_rule(&mut rules, &mut path_handles, cache, LANDLOCK_FS_READ_FILE);
        }

        Ok(Self {
            executable,
            rules,
            _path_handles: path_handles,
            _input: input,
        })
    }

    pub(super) fn executable(&self) -> &Path {
        &self.executable
    }

    pub(super) fn input_fd(&self) -> RawFd {
        CHILD_INPUT_FD
    }
}

fn push_rule(
    rules: &mut Vec<LandlockRule>,
    handles: &mut Vec<File>,
    handle: File,
    allowed_access: u64,
) {
    rules.push(LandlockRule { allowed_access });
    handles.push(handle);
}

fn open_path_handle(path: &Path) -> io::Result<File> {
    let canonical = fs::canonicalize(path)?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW);
    options.open(canonical)
}

fn resolve_executable(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() || path.components().count() > 1 {
        return fs::canonicalize(path);
    }
    let search_path = std::env::var_os("PATH").unwrap_or_default();
    for directory in std::env::split_paths(&search_path) {
        let candidate = directory.join(path);
        if candidate.is_file() {
            return fs::canonicalize(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "media executable was not found on the server PATH",
    ))
}

fn require_landlock() -> io::Result<landlock::RulesetCreated> {
    use landlock::{ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr};
    Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI::V3))
        .and_then(|rules| rules.create())
        .map_err(io::Error::other)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct InheritedRule {
    descriptor: RawFd,
    allowed_access: u64,
    device: u64,
    inode: u64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct WorkerLimits {
    cpu_seconds: u64,
    address_space_bytes: u64,
    file_size_bytes: u64,
    rules: Option<Vec<InheritedRule>>,
    valid: bool,
}

fn worker_executable() -> io::Result<PathBuf> {
    let current = std::env::current_exe()?;
    let parent = current
        .parent()
        .ok_or_else(|| io::Error::other("missing executable directory"))?;
    if parent.file_name().is_some_and(|name| name == "deps") {
        Ok(parent
            .parent()
            .ok_or_else(|| io::Error::other("missing build directory"))?
            .join("puffinbox-server"))
    } else {
        Ok(current)
    }
}

/// Re-enter the server's single-threaded worker mode before launching a tool.
pub(super) fn media_command(executable: impl AsRef<std::ffi::OsStr>) -> Command {
    let worker = worker_executable()
        .unwrap_or_else(|_| PathBuf::from("/puffinbox-unavailable-media-worker"));
    let mut command = Command::new(worker);
    command.arg("--media-worker").arg(executable);
    command
}

/// Pass owned file handles and policy to a worker, without a project pre_exec hook.
/// Call immediately after media_command, before adding tool arguments.
pub(super) fn apply_child_limits(
    command: &mut Command,
    cpu_seconds: u64,
    address_space_bytes: u64,
    file_size_bytes: u64,
    sandbox: Option<MediaChildSandbox>,
) {
    use command_fds::{CommandFdExt, FdMapping};
    use std::os::unix::fs::MetadataExt;
    let mut mappings = Vec::new();
    let mut limits = WorkerLimits {
        cpu_seconds,
        address_space_bytes,
        file_size_bytes,
        rules: None,
        valid: true,
    };
    if let Some(sandbox) = sandbox {
        let mut rules = Vec::new();
        if sandbox.rules.len() != sandbox._path_handles.len() || sandbox.rules.len() > 32 {
            limits.valid = false;
        }
        for (index, (rule, handle)) in sandbox
            .rules
            .into_iter()
            .zip(sandbox._path_handles)
            .enumerate()
        {
            let descriptor = 4 + index as RawFd;
            match handle.metadata() {
                Ok(metadata) => rules.push(InheritedRule {
                    descriptor,
                    allowed_access: rule.allowed_access,
                    device: metadata.dev(),
                    inode: metadata.ino(),
                }),
                Err(_) => limits.valid = false,
            }
            mappings.push(FdMapping {
                parent_fd: handle.into(),
                child_fd: descriptor,
            });
        }
        if let Some(input) = sandbox._input {
            mappings.push(FdMapping {
                parent_fd: input.into(),
                child_fd: CHILD_INPUT_FD,
            });
        }
        limits.rules = Some(rules);
    }
    if command.as_std_mut().fd_mappings(mappings).is_err() {
        limits.valid = false;
    }
    command
        .arg(serde_json::to_string(&limits).unwrap_or_default())
        .arg("--");
}

/// Internal worker entry. This runs before the server creates threads or loads credentials.
#[doc(hidden)]
pub fn run_media_worker() -> io::Result<()> {
    use landlock::{AccessFs, BitFlags, PathBeneath, RulesetCreatedAttr, RulesetStatus};
    use nix::sys::resource::Resource;
    use std::os::unix::{fs::MetadataExt, process::CommandExt};
    let mut arguments = std::env::args_os().skip(2);
    let executable = arguments
        .next()
        .ok_or_else(|| io::Error::other("missing media executable"))?;
    let payload = arguments
        .next()
        .ok_or_else(|| io::Error::other("missing media limits"))?;
    let payload = payload
        .to_str()
        .filter(|text| text.len() <= 16 * 1024)
        .ok_or_else(|| io::Error::other("invalid media limits"))?;
    let limits: WorkerLimits = serde_json::from_str(payload).map_err(io::Error::other)?;
    if !limits.valid || arguments.next().as_deref() != Some(std::ffi::OsStr::new("--")) {
        return Err(io::Error::other("invalid media worker configuration"));
    }
    set_limit(Resource::RLIMIT_CPU, limits.cpu_seconds)?;
    set_limit(Resource::RLIMIT_AS, limits.address_space_bytes)?;
    set_limit(Resource::RLIMIT_FSIZE, limits.file_size_bytes)?;
    set_limit(Resource::RLIMIT_CORE, 0)?;
    if let Some(rules) = limits.rules {
        if rules.is_empty() || rules.len() > 32 {
            return Err(io::Error::other("invalid media rule count"));
        }
        let mut ruleset = require_landlock()?;
        for (index, rule) in rules.into_iter().enumerate() {
            if rule.descriptor != 4 + index as RawFd {
                return Err(io::Error::other("invalid inherited rule descriptor"));
            }
            // Reopen only the worker's explicitly mapped O_PATH capability. Do not
            // canonicalize it: renamed or unlinked paths retain the same inode.
            let handle = File::from(rustix::fs::open(
                format!("/proc/self/fd/{}", rule.descriptor),
                rustix::fs::OFlags::PATH | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )?);
            let metadata = handle.metadata()?;
            if metadata.dev() != rule.device || metadata.ino() != rule.inode {
                return Err(io::Error::other("inherited media rule identity changed"));
            }
            nix::unistd::close(rule.descriptor).map_err(io::Error::from)?;
            let access = BitFlags::<AccessFs>::from_bits(rule.allowed_access)
                .map_err(|_| io::Error::other("invalid media access rights"))?;
            ruleset = ruleset
                .add_rule(PathBeneath::new(handle, access))
                .map_err(io::Error::other)?;
        }
        let status = ruleset.restrict_self().map_err(io::Error::other)?;
        if status.ruleset != RulesetStatus::FullyEnforced || !status.no_new_privs {
            return Err(io::Error::other(
                "media filesystem sandbox was not fully enforced",
            ));
        }
    }
    set_limit(Resource::RLIMIT_NOFILE, CHILD_DESCRIPTOR_LIMIT)?;
    install_media_child_filter(
        &media_child_filter()
            .ok_or_else(|| io::Error::other("unsupported media sandbox architecture"))?,
    )?;
    Err(std::process::Command::new(executable)
        .args(arguments)
        .exec())
}

/// Build the x86-64 classic seccomp-BPF policy for network, parent-process and
/// descendant isolation. Thread creation is allowed only within the supervised
/// process group.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn media_child_filter() -> Option<Vec<seccompiler::sock_filter>> {
    const BPF_LD_W_ABS: u16 = 0x20;
    const BPF_JMP_JEQ_K: u16 = 0x15;
    const BPF_JMP_JSET_K: u16 = 0x45;
    const BPF_RET_K: u16 = 0x06;
    const RET_ERRNO: u32 = 0x0005_0000;
    const RET_ALLOW: u32 = 0x7fff_0000;
    const RET_KILL_PROCESS: u32 = 0x8000_0000;
    const AUDIT_ARCH: u32 = 0xc000_003e;
    const X32_SYSCALL_BIT: u32 = 0x4000_0000;

    let mut filter = vec![
        seccompiler::sock_filter {
            code: BPF_LD_W_ABS,
            jt: 0,
            jf: 0,
            k: 4,
        },
        seccompiler::sock_filter {
            code: BPF_JMP_JEQ_K,
            jt: 1,
            jf: 0,
            k: AUDIT_ARCH,
        },
        seccompiler::sock_filter {
            code: BPF_RET_K,
            jt: 0,
            jf: 0,
            k: RET_KILL_PROCESS,
        },
        seccompiler::sock_filter {
            code: BPF_LD_W_ABS,
            jt: 0,
            jf: 0,
            k: 0,
        },
    ];

    filter.extend([
        seccompiler::sock_filter {
            code: BPF_JMP_JSET_K,
            jt: 0,
            jf: 1,
            k: X32_SYSCALL_BIT,
        },
        seccompiler::sock_filter {
            code: BPF_RET_K,
            jt: 0,
            jf: 0,
            k: RET_KILL_PROCESS,
        },
    ]);

    // Only pthread-style clone calls are needed by the encoders. Requiring
    // CLONE_THREAD keeps every worker in the supervised process; fork/vfork
    // and clone3 are denied below so descendants cannot outlive the job.
    filter.extend([
        seccompiler::sock_filter {
            code: BPF_JMP_JEQ_K,
            jt: 0,
            jf: 3,
            k: libc::SYS_clone as u32,
        },
        seccompiler::sock_filter {
            code: BPF_LD_W_ABS,
            jt: 0,
            jf: 0,
            k: 16,
        },
        seccompiler::sock_filter {
            code: BPF_JMP_JSET_K,
            jt: 1,
            jf: 0,
            k: libc::CLONE_THREAD as u32,
        },
        seccompiler::sock_filter {
            code: BPF_RET_K,
            jt: 0,
            jf: 0,
            k: RET_ERRNO | libc::EPERM as u32,
        },
    ]);

    // clone3 cannot be safely filtered by its pointed-to flags in classic
    // BPF. Return ENOSYS so libc can fall back to the constrained clone path.
    filter.extend([
        seccompiler::sock_filter {
            code: BPF_JMP_JEQ_K,
            jt: 0,
            jf: 1,
            k: libc::SYS_clone3 as u32,
        },
        seccompiler::sock_filter {
            code: BPF_RET_K,
            jt: 0,
            jf: 0,
            k: RET_ERRNO | libc::ENOSYS as u32,
        },
    ]);

    let denied = vec![
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_connect,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_accept,
        libc::SYS_accept4,
        libc::SYS_sendto,
        libc::SYS_recvfrom,
        libc::SYS_sendmsg,
        libc::SYS_recvmsg,
        libc::SYS_sendmmsg,
        libc::SYS_recvmmsg,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_kill,
        libc::SYS_tkill,
        libc::SYS_tgkill,
        libc::SYS_rt_sigqueueinfo,
        libc::SYS_rt_tgsigqueueinfo,
        libc::SYS_pidfd_send_signal,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_process_madvise,
        libc::SYS_process_mrelease,
        libc::SYS_pidfd_getfd,
        libc::SYS_fork,
        libc::SYS_vfork,
        libc::SYS_open_by_handle_at,
        libc::SYS_name_to_handle_at,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_setns,
        libc::SYS_unshare,
    ];
    for number in denied {
        let number = u32::try_from(number).ok()?;
        filter.push(seccompiler::sock_filter {
            code: BPF_JMP_JEQ_K,
            jt: 0,
            jf: 1,
            k: number,
        });
        filter.push(seccompiler::sock_filter {
            code: BPF_RET_K,
            jt: 0,
            jf: 0,
            k: RET_ERRNO | libc::EPERM as u32,
        });
    }
    filter.push(seccompiler::sock_filter {
        code: BPF_RET_K,
        jt: 0,
        jf: 0,
        k: RET_ALLOW,
    });
    u16::try_from(filter.len()).ok()?;
    Some(filter)
}

#[cfg(all(target_os = "linux", not(target_arch = "x86_64")))]
fn media_child_filter() -> Option<Vec<seccompiler::sock_filter>> {
    None
}

fn install_media_child_filter(filter: &[seccompiler::sock_filter]) -> io::Result<()> {
    seccompiler::apply_filter(filter).map_err(io::Error::other)
}

fn set_limit(resource: nix::sys::resource::Resource, value: u64) -> io::Result<()> {
    nix::sys::resource::setrlimit(resource, value, value).map_err(io::Error::from)
}

#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
mod tests {
    use super::{
        CHILD_DESCRIPTOR_LIMIT, CHILD_INPUT_FD, MediaChildSandbox, apply_child_limits,
        media_child_filter, media_command, require_landlock, stop_child,
    };
    use std::{
        fs::{self, File},
        os::fd::{AsRawFd, RawFd},
        process::Stdio,
        time::Duration,
    };
    use tokio::{process::Command, time::timeout};

    #[tokio::test]
    async fn termination_gives_a_child_time_to_finish_and_reaps_it() {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut child = Command::new("/bin/sh")
            .args([
                "-c",
                "trap 'exit 42' TERM; printf 'ready\\n'; while :; do :; done",
            ])
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut ready = String::new();
        timeout(Duration::from_secs(2), output.read_line(&mut ready))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ready.trim(), "ready");
        let status = stop_child(&mut child).await.unwrap();
        assert_eq!(status.code(), Some(42));
        assert_eq!(child.try_wait().unwrap().unwrap().code(), Some(42));
    }

    #[tokio::test]
    async fn termination_forces_a_stuck_child_after_the_grace_period() {
        use std::os::unix::process::ExitStatusExt;
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut child = Command::new("/bin/sh")
            .args(["-c", "trap '' TERM; printf 'ready\\n'; while :; do :; done"])
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut ready = String::new();
        timeout(Duration::from_secs(2), output.read_line(&mut ready))
            .await
            .unwrap()
            .unwrap();
        let status = timeout(Duration::from_secs(4), stop_child(&mut child))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert!(child.try_wait().unwrap().is_some());
    }

    #[tokio::test]
    async fn media_child_cannot_create_network_sockets() {
        const CHILD: &str = "PUFFINBOX_NETWORK_DENY_CHILD";
        let mut command = media_command(std::env::current_exe().unwrap());
        // The test harness needs more virtual address space than a decoder.
        apply_child_limits(&mut command, 10, 4 * 1024 * 1024 * 1024, 0, None);
        command
            .arg("--exact")
            .arg("media_features::process_limits::tests::network_deny_child_probe")
            .arg("--nocapture")
            .env(CHILD, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = command.spawn().expect("limited media child should launch");
        let output = timeout(Duration::from_secs(5), child.wait_with_output())
            .await
            .expect("network-denial probe exceeded its time limit")
            .expect("limited media child output should be available");
        assert!(
            output.status.success(),
            "network-denial probe failed (status {:?}): {}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[tokio::test]
    async fn media_child_filesystem_and_parent_process_access_are_confined() {
        assert!(require_landlock().is_ok());
        let base =
            std::env::temp_dir().join(format!("puffinbox-landlock-{}", uuid::Uuid::new_v4()));
        let scratch = base.join("scratch");
        fs::create_dir_all(&scratch).unwrap();
        let source_path = base.join("catalog-input.mkv");
        let outside_path = base.join("outside-secret.txt");
        fs::write(&source_path, b"only approved input bytes").unwrap();
        fs::write(&outside_path, b"do not expose outside files").unwrap();

        // Exercise remapping with an input descriptor above the child limit.
        // Duplicate to a deliberate minimum rather than relying on unrelated
        // open descriptors or test-runner allocation order.
        let original_input = File::open(&source_path).unwrap();
        let minimum_input_fd = CHILD_DESCRIPTOR_LIMIT as RawFd;
        let input =
            File::from(rustix::io::fcntl_dupfd_cloexec(&original_input, minimum_input_fd).unwrap());
        assert!(input.as_raw_fd() >= minimum_input_fd);
        drop(original_input);
        let parent_secret_fd = File::open(&outside_path).unwrap();
        let input_fd = input.as_raw_fd();
        let parent_secret_fd_number = parent_secret_fd.as_raw_fd();
        let parent_pid = std::process::id();
        let sandbox =
            MediaChildSandbox::prepare(&std::env::current_exe().unwrap(), input, Some(&scratch))
                .unwrap();
        assert_eq!(sandbox.input_fd(), CHILD_INPUT_FD);

        let mut command = media_command(sandbox.executable());
        apply_child_limits(&mut command, 10, 4 * 1024 * 1024 * 1024, 0, Some(sandbox));
        command
            .env_clear()
            .env("PUFFINBOX_LANDLOCK_CHILD", "1")
            .env("PUFFINBOX_LANDLOCK_INPUT_FD", CHILD_INPUT_FD.to_string())
            .env("PUFFINBOX_LANDLOCK_INPUT_PARENT_FD", input_fd.to_string())
            .env(
                "PUFFINBOX_LANDLOCK_PARENT_SECRET_FD",
                parent_secret_fd_number.to_string(),
            )
            .env("PUFFINBOX_LANDLOCK_PARENT_PID", parent_pid.to_string())
            .env("PUFFINBOX_LANDLOCK_OUTSIDE_PATH", outside_path.as_os_str())
            .arg("--exact")
            .arg("media_features::process_limits::tests::media_child_sandbox_child_probe")
            .arg("--nocapture")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let output = timeout(Duration::from_secs(10), command.output())
            .await
            .expect("sandbox child exceeded its time limit")
            .expect("sandbox child output should be available");
        assert!(
            output.status.success(),
            "sandbox probe failed (status {:?}): {}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        drop(parent_secret_fd);
        fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn output_only_media_child_reads_server_pipe_under_filesystem_rules() {
        const CHILD: &str = "PUFFINBOX_OUTPUT_ONLY_CHILD";
        let base =
            std::env::temp_dir().join(format!("puffinbox-pipe-sandbox-{}", uuid::Uuid::new_v4()));
        let scratch = base.join("scratch");
        fs::create_dir_all(&scratch).unwrap();
        let sandbox = MediaChildSandbox::prepare_output_bounded(
            std::env::current_exe().unwrap(),
            Some(scratch.clone()),
            crate::media_features::secure_path::filesystem_permit().unwrap(),
        )
        .await
        .unwrap();
        assert!(sandbox._input.is_none());
        let mut command = media_command(sandbox.executable());
        apply_child_limits(
            &mut command,
            10,
            4 * 1024 * 1024 * 1024,
            4096,
            Some(sandbox),
        );
        command
            .env_clear()
            .env(CHILD, "1")
            .current_dir(&scratch)
            .arg("--exact")
            .arg("media_features::process_limits::tests::output_only_child_probe")
            .arg("--nocapture")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("output-only child should spawn");
        let mut stdin = child.stdin.take().expect("child stdin should be piped");
        use tokio::io::AsyncWriteExt as _;
        stdin.write_all(b"controlled MPEG-TS bytes").await.unwrap();
        drop(stdin);
        let output = timeout(Duration::from_secs(5), child.wait_with_output())
            .await
            .expect("output-only child exceeded its time limit")
            .expect("output-only child output should be available");
        assert!(
            output.status.success(),
            "output-only sandbox probe failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read(scratch.join("pipe-result")).unwrap(),
            b"controlled MPEG-TS bytes"
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn output_only_child_probe() {
        if std::env::var_os("PUFFINBOX_OUTPUT_ONLY_CHILD").is_none() {
            return;
        }
        use std::io::Read as _;
        let mut input = Vec::new();
        std::io::stdin().read_to_end(&mut input).unwrap();
        assert_eq!(input, b"controlled MPEG-TS bytes");
        assert!(
            fs::read("/etc/passwd").is_err(),
            "host filesystem was readable"
        );
        fs::write("pipe-result", input).unwrap();
    }

    #[test]
    fn media_child_sandbox_child_probe() {
        if std::env::var_os("PUFFINBOX_LANDLOCK_CHILD").is_none() {
            return;
        }
        // Only the approved input survives exec. Landlock path capabilities
        // must be closed so the decoder cannot use them as directory handles.
        for descriptor in 4..36 {
            assert_eq!(
                nix::fcntl::fcntl(descriptor, nix::fcntl::FcntlArg::F_GETFD),
                Err(nix::errno::Errno::EBADF),
                "unexpected inherited descriptor {descriptor}"
            );
        }
        let input_fd = env_fd("PUFFINBOX_LANDLOCK_INPUT_FD");
        let mut input_bytes = [0_u8; 64];
        let read = nix::unistd::read(input_fd, &mut input_bytes).unwrap();
        let expected_input = b"only approved input bytes";
        assert_eq!(read, expected_input.len());
        assert_eq!(&input_bytes[..read], expected_input);

        let outside_path = std::env::var("PUFFINBOX_LANDLOCK_OUTSIDE_PATH").unwrap();
        assert!(fs::read(outside_path).is_err(), "outside file was readable");
        let parent_pid = std::env::var("PUFFINBOX_LANDLOCK_PARENT_PID")
            .unwrap()
            .parse::<libc::pid_t>()
            .unwrap();
        let parent_secret_fd = env_fd("PUFFINBOX_LANDLOCK_PARENT_SECRET_FD");
        assert!(
            fs::read(format!("/proc/{parent_pid}/environ")).is_err(),
            "parent environment was readable"
        );
        assert!(
            fs::read(format!("/proc/{parent_pid}/fd/{parent_secret_fd}")).is_err(),
            "parent file descriptor was readable"
        );

        let pid = nix::unistd::Pid::from_raw(parent_pid);
        assert_eq!(
            nix::sys::signal::kill(pid, None),
            Err(nix::errno::Errno::EPERM)
        );
        assert_eq!(nix::sys::ptrace::attach(pid), Err(nix::errno::Errno::EPERM));
        let mut local = [0_u8; 1];
        assert_eq!(
            nix::sys::uio::process_vm_readv(
                pid,
                &mut [std::io::IoSliceMut::new(&mut local)],
                &[nix::sys::uio::RemoteIoVec { base: 0, len: 1 }]
            ),
            Err(nix::errno::Errno::EPERM)
        );

        let parent_pid_fd = rustix::process::pidfd_open(
            rustix::process::Pid::from_raw(parent_pid).unwrap(),
            rustix::process::PidfdFlags::empty(),
        )
        .unwrap();
        let result = rustix::process::pidfd_getfd(
            &parent_pid_fd,
            env_fd("PUFFINBOX_LANDLOCK_INPUT_PARENT_FD"),
            rustix::process::PidfdGetfdFlags::empty(),
        );
        assert!(matches!(result, Err(rustix::io::Errno::PERM)));
        drop(parent_pid_fd);
        assert_network_denied();

        let thread = std::thread::spawn(|| 7);
        assert_eq!(
            thread.join().unwrap(),
            7,
            "decoder threads must remain usable"
        );
    }

    fn env_fd(name: &str) -> libc::c_int {
        std::env::var(name).unwrap().parse().unwrap()
    }

    #[test]
    fn network_deny_child_probe() {
        if std::env::var_os("PUFFINBOX_NETWORK_DENY_CHILD").is_none() {
            return;
        }
        assert_network_denied();
    }

    fn assert_network_denied() {
        let result = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        );
        assert!(matches!(result, Err(error) if error.raw_os_error() == Some(libc::EPERM)));
    }

    #[test]
    fn classic_filter_denies_x32_parent_access_and_non_thread_clones() {
        let filter = media_child_filter().expect("supported Linux architecture");
        let arch = 0xc000_003e;
        const RET_ERRNO: u32 = 0x0005_0000;
        const RET_ALLOW: u32 = 0x7fff_0000;
        const RET_KILL_PROCESS: u32 = 0x8000_0000;
        assert_eq!(
            evaluate_filter(&filter, arch, libc::SYS_socket as u32, 0),
            RET_ERRNO | libc::EPERM as u32
        );
        assert_eq!(
            evaluate_filter(&filter, arch, libc::SYS_getpid as u32, 0),
            RET_ALLOW
        );
        assert_eq!(
            evaluate_filter(&filter, arch ^ 1, libc::SYS_getpid as u32, 0),
            RET_KILL_PROCESS
        );
        assert_eq!(
            evaluate_filter(
                &filter,
                arch,
                libc::SYS_clone as u32,
                libc::CLONE_THREAD as u32
            ),
            RET_ALLOW
        );
        assert_eq!(
            evaluate_filter(&filter, arch, libc::SYS_clone as u32, libc::SIGCHLD as u32),
            RET_ERRNO | libc::EPERM as u32
        );
        assert_eq!(
            evaluate_filter(&filter, arch, libc::SYS_fork as u32, 0),
            RET_ERRNO | libc::EPERM as u32
        );
        assert_eq!(
            evaluate_filter(&filter, arch, libc::SYS_clone3 as u32, 0),
            RET_ERRNO | libc::ENOSYS as u32
        );
        assert_eq!(
            evaluate_filter(&filter, arch, (libc::SYS_socket as u32) | 0x4000_0000, 0,),
            RET_KILL_PROCESS
        );
    }

    fn evaluate_filter(
        filter: &[seccompiler::sock_filter],
        arch: u32,
        syscall_number: u32,
        first_argument: u32,
    ) -> u32 {
        let mut accumulator = 0_u32;
        let mut instruction_pointer = 0_usize;
        while let Some(instruction) = filter.get(instruction_pointer) {
            match instruction.code {
                0x20 => {
                    accumulator = match instruction.k {
                        0 => syscall_number,
                        4 => arch,
                        16 => first_argument,
                        _ => panic!("unexpected absolute BPF load offset {}", instruction.k),
                    };
                    instruction_pointer += 1;
                }
                0x15 => {
                    let jump = if accumulator == instruction.k {
                        instruction.jt
                    } else {
                        instruction.jf
                    };
                    instruction_pointer += 1 + jump as usize;
                }
                0x45 => {
                    let jump = if accumulator & instruction.k != 0 {
                        instruction.jt
                    } else {
                        instruction.jf
                    };
                    instruction_pointer += 1 + jump as usize;
                }
                0x06 => return instruction.k,
                code => panic!("unexpected BPF instruction {code:#x}"),
            }
        }
        panic!("BPF program did not return an action");
    }
}
