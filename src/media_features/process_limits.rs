//! Child-only resource limits and Linux process isolation for media tools.

use std::{
    fs::{self, File, OpenOptions},
    io,
    os::fd::{AsRawFd, RawFd},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
use tokio::process::{Child, Command};
use tokio::sync::OwnedSemaphorePermit;

use crate::ApiError;

const LANDLOCK_ABI_REQUIRED: i64 = 3;
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
        // SAFETY: this positive PID belongs to the unreaped child owned by the
        // caller. It cannot be reused until this owner waits for the child.
        let signaled = unsafe { libc::kill(pid, libc::SIGTERM) };
        if signaled != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
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
const LANDLOCK_FS_REMOVE_DIR: u64 = 1 << 4;
const LANDLOCK_FS_REMOVE_FILE: u64 = 1 << 5;
const LANDLOCK_FS_MAKE_CHAR: u64 = 1 << 6;
const LANDLOCK_FS_MAKE_DIR: u64 = 1 << 7;
const LANDLOCK_FS_MAKE_REG: u64 = 1 << 8;
const LANDLOCK_FS_MAKE_SOCK: u64 = 1 << 9;
const LANDLOCK_FS_MAKE_FIFO: u64 = 1 << 10;
const LANDLOCK_FS_MAKE_BLOCK: u64 = 1 << 11;
const LANDLOCK_FS_MAKE_SYM: u64 = 1 << 12;
const LANDLOCK_FS_REFER: u64 = 1 << 13;
const LANDLOCK_FS_TRUNCATE: u64 = 1 << 14;
const LANDLOCK_ACCESS_FS_SUPPORTED_ABI_3: u64 = LANDLOCK_FS_EXECUTE
    | LANDLOCK_FS_WRITE_FILE
    | LANDLOCK_FS_READ_FILE
    | LANDLOCK_FS_READ_DIR
    | LANDLOCK_FS_REMOVE_DIR
    | LANDLOCK_FS_REMOVE_FILE
    | LANDLOCK_FS_MAKE_CHAR
    | LANDLOCK_FS_MAKE_DIR
    | LANDLOCK_FS_MAKE_REG
    | LANDLOCK_FS_MAKE_SOCK
    | LANDLOCK_FS_MAKE_FIFO
    | LANDLOCK_FS_MAKE_BLOCK
    | LANDLOCK_FS_MAKE_SYM
    | LANDLOCK_FS_REFER
    | LANDLOCK_FS_TRUNCATE;
const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
const LANDLOCK_RULE_PATH_BENEATH: libc::c_int = 1;
const LANDLOCK_CREATE_RULESET_HANDLED_ACCESS_FLAGS: libc::c_uint = 0;

const RUNTIME_READ_ACCESS: u64 = LANDLOCK_FS_EXECUTE | LANDLOCK_FS_READ_FILE | LANDLOCK_FS_READ_DIR;
const SCRATCH_ACCESS: u64 = LANDLOCK_FS_READ_FILE
    | LANDLOCK_FS_WRITE_FILE
    | LANDLOCK_FS_READ_DIR
    | LANDLOCK_FS_REMOVE_FILE
    | LANDLOCK_FS_MAKE_REG
    | LANDLOCK_FS_REFER
    | LANDLOCK_FS_TRUNCATE;

#[repr(C)]
struct LandlockRulesetAttr {
    handled_access_fs: u64,
}

#[repr(C)]
struct LandlockPathBeneathAttr {
    allowed_access: u64,
    parent_fd: libc::c_int,
}

struct LandlockRule {
    path_fd: RawFd,
    allowed_access: u64,
}

/// Paths opened by the server and converted to Landlock rules. The source
/// media remains an already-open file descriptor; no parent `/proc` path is
/// made visible to the decoder.
pub(super) struct MediaChildSandbox {
    executable: PathBuf,
    input_fd: Option<RawFd>,
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
        let abi = landlock_abi_version()?;
        if abi < LANDLOCK_ABI_REQUIRED {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Landlock ABI 3 or newer is required for media processes",
            ));
        }
        if let Some(file) = &input
            && !file.metadata()?.is_file()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "media input descriptor is not a regular file",
            ));
        }
        let input_fd = input.as_ref().map(AsRawFd::as_raw_fd);

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
            input_fd,
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
    rules.push(LandlockRule {
        path_fd: handle.as_raw_fd(),
        allowed_access,
    });
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

fn landlock_abi_version() -> io::Result<i64> {
    // SAFETY: this uses the documented version-query form with a null ruleset
    // attribute and size zero; the kernel reads no pointer data in this mode.
    let result = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<LandlockRulesetAttr>(),
            0_usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

/// Install child resource limits, a fail-closed Landlock filesystem policy,
/// and a seccomp policy before an external decoder or encoder starts. Media
/// children can read one inherited source descriptor, execute only the chosen
/// binary, read runtime libraries, and use the private scratch directory.
pub(super) fn apply_child_limits(
    command: &mut Command,
    cpu_seconds: u64,
    address_space_bytes: u64,
    file_size_bytes: u64,
    sandbox: Option<MediaChildSandbox>,
) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        #[cfg(target_os = "linux")]
        let child_filter = media_child_filter();
        unsafe {
            command.as_std_mut().pre_exec(move || {
                set_limit(libc::RLIMIT_CPU, cpu_seconds)?;
                set_limit(libc::RLIMIT_AS, address_space_bytes)?;
                set_limit(libc::RLIMIT_FSIZE, file_size_bytes)?;
                set_limit(libc::RLIMIT_CORE, 0)?;
                #[cfg(target_os = "linux")]
                if let Some(sandbox) = sandbox.as_ref() {
                    install_landlock_sandbox(sandbox)?;
                    if sandbox.input_fd.is_some() {
                        remap_input_descriptor(sandbox)?;
                    }
                }
                set_limit(libc::RLIMIT_NOFILE, CHILD_DESCRIPTOR_LIMIT)?;
                #[cfg(target_os = "linux")]
                install_media_child_filter(child_filter.as_deref().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "media child filter is unavailable for this Linux architecture",
                    )
                })?)?;
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (
            command,
            cpu_seconds,
            address_space_bytes,
            file_size_bytes,
            sandbox,
        );
    }
}

#[cfg(target_os = "linux")]
fn install_landlock_sandbox(sandbox: &MediaChildSandbox) -> io::Result<()> {
    const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;
    let abi = landlock_abi_version()?;
    if abi < LANDLOCK_ABI_REQUIRED {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Landlock ABI 3 or newer is required for media processes",
        ));
    }
    let ruleset_attr = LandlockRulesetAttr {
        handled_access_fs: LANDLOCK_ACCESS_FS_SUPPORTED_ABI_3,
    };
    // SAFETY: the ABI-3 ruleset structure is initialized and matches the
    // documented kernel ABI; its address remains valid for the syscall.
    let ruleset_fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &ruleset_attr as *const LandlockRulesetAttr,
            std::mem::size_of::<LandlockRulesetAttr>(),
            LANDLOCK_CREATE_RULESET_HANDLED_ACCESS_FLAGS,
        )
    };
    if ruleset_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let ruleset_fd = ruleset_fd as RawFd;
    for rule in &sandbox.rules {
        let path_attr = LandlockPathBeneathAttr {
            allowed_access: rule.allowed_access,
            parent_fd: rule.path_fd,
        };
        // SAFETY: the path-beneath structure is initialized and remains valid
        // until the kernel finishes adding this rule.
        if unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                ruleset_fd,
                LANDLOCK_RULE_PATH_BENEATH,
                &path_attr as *const LandlockPathBeneathAttr,
                0_u32,
            )
        } < 0
        {
            let error = io::Error::last_os_error();
            unsafe { libc::close(ruleset_fd) };
            return Err(error);
        }
    }

    if unsafe { libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        let error = io::Error::last_os_error();
        unsafe { libc::close(ruleset_fd) };
        return Err(error);
    }
    // SAFETY: the descriptor is a ruleset created above; flags are zero.
    if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset_fd, 0_u32) } < 0 {
        let error = io::Error::last_os_error();
        unsafe { libc::close(ruleset_fd) };
        return Err(error);
    }
    unsafe { libc::close(ruleset_fd) };
    Ok(())
}

#[cfg(target_os = "linux")]
fn remap_input_descriptor(sandbox: &MediaChildSandbox) -> io::Result<()> {
    let Some(input_fd) = sandbox.input_fd else {
        return Ok(());
    };
    if input_fd == CHILD_INPUT_FD {
        let flags = unsafe { libc::fcntl(CHILD_INPUT_FD, libc::F_GETFD) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::fcntl(CHILD_INPUT_FD, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(());
    }
    // The child stdio setup reserves descriptors 0–2. Descriptor 3 is a fixed
    // input slot and may replace a temporary Landlock path handle after the
    // rules have already been installed.
    if unsafe { libc::dup3(input_fd, CHILD_INPUT_FD, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::close(input_fd) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Build the x86-64 classic seccomp-BPF policy for network, parent-process and
/// descendant isolation. Thread creation is allowed only within the supervised
/// process group.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn media_child_filter() -> Option<Vec<libc::sock_filter>> {
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
        libc::sock_filter {
            code: BPF_LD_W_ABS,
            jt: 0,
            jf: 0,
            k: 4,
        },
        libc::sock_filter {
            code: BPF_JMP_JEQ_K,
            jt: 1,
            jf: 0,
            k: AUDIT_ARCH,
        },
        libc::sock_filter {
            code: BPF_RET_K,
            jt: 0,
            jf: 0,
            k: RET_KILL_PROCESS,
        },
        libc::sock_filter {
            code: BPF_LD_W_ABS,
            jt: 0,
            jf: 0,
            k: 0,
        },
    ];

    filter.extend([
        libc::sock_filter {
            code: BPF_JMP_JSET_K,
            jt: 0,
            jf: 1,
            k: X32_SYSCALL_BIT,
        },
        libc::sock_filter {
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
        libc::sock_filter {
            code: BPF_JMP_JEQ_K,
            jt: 0,
            jf: 3,
            k: libc::SYS_clone as u32,
        },
        libc::sock_filter {
            code: BPF_LD_W_ABS,
            jt: 0,
            jf: 0,
            k: 16,
        },
        libc::sock_filter {
            code: BPF_JMP_JSET_K,
            jt: 1,
            jf: 0,
            k: libc::CLONE_THREAD as u32,
        },
        libc::sock_filter {
            code: BPF_RET_K,
            jt: 0,
            jf: 0,
            k: RET_ERRNO | libc::EPERM as u32,
        },
    ]);

    // clone3 cannot be safely filtered by its pointed-to flags in classic
    // BPF. Return ENOSYS so libc can fall back to the constrained clone path.
    filter.extend([
        libc::sock_filter {
            code: BPF_JMP_JEQ_K,
            jt: 0,
            jf: 1,
            k: libc::SYS_clone3 as u32,
        },
        libc::sock_filter {
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
        filter.push(libc::sock_filter {
            code: BPF_JMP_JEQ_K,
            jt: 0,
            jf: 1,
            k: number,
        });
        filter.push(libc::sock_filter {
            code: BPF_RET_K,
            jt: 0,
            jf: 0,
            k: RET_ERRNO | libc::EPERM as u32,
        });
    }
    filter.push(libc::sock_filter {
        code: BPF_RET_K,
        jt: 0,
        jf: 0,
        k: RET_ALLOW,
    });
    u16::try_from(filter.len()).ok()?;
    Some(filter)
}

#[cfg(all(target_os = "linux", not(target_arch = "x86_64")))]
fn media_child_filter() -> Option<Vec<libc::sock_filter>> {
    None
}

#[cfg(target_os = "linux")]
fn install_media_child_filter(filter: &[libc::sock_filter]) -> io::Result<()> {
    const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;
    const PR_SET_SECCOMP: libc::c_int = 22;
    const SECCOMP_MODE_FILTER: libc::c_ulong = 2;
    if filter.is_empty() || filter.len() > u16::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "network filter is invalid",
        ));
    }
    let mut program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_ptr() as *mut libc::sock_filter,
    };
    // SAFETY: the filter slice and program remain alive through both syscalls;
    // the program points only into that initialized slice.
    if unsafe { libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe {
        libc::prctl(
            PR_SET_SECCOMP,
            SECCOMP_MODE_FILTER,
            &mut program as *mut libc::sock_fprog as libc::c_ulong,
            0,
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn set_limit(resource: Resource, value: u64) -> io::Result<()> {
    let value = libc::rlim_t::try_from(value).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "process limit is out of range")
    })?;
    let limit = libc::rlimit {
        rlim_cur: value,
        rlim_max: value,
    };
    // SAFETY: `limit` is fully initialized and lives for the duration of this
    // direct system call in the child process.
    let result = unsafe { libc::setrlimit(resource, &limit) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

// glibc exposes this alias while musl declares `setrlimit` with `c_int`.
// Keep the signature portable to the Linux musl release target.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
type Resource = libc::__rlimit_resource_t;
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
type Resource = libc::c_int;

#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
mod tests {
    use super::{
        CHILD_DESCRIPTOR_LIMIT, CHILD_INPUT_FD, MediaChildSandbox, apply_child_limits,
        landlock_abi_version, media_child_filter, stop_child,
    };
    use std::{
        fs::{self, File},
        os::fd::{AsRawFd, FromRawFd, RawFd},
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
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .arg("--exact")
            .arg("media_features::process_limits::tests::network_deny_child_probe")
            .arg("--nocapture")
            .env(CHILD, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // The test harness itself needs a larger virtual address space than a
        // decoder, so this child exercises the seccomp filter independently
        // of production decoder memory limits.
        apply_child_limits(&mut command, 10, 4 * 1024 * 1024 * 1024, 0, None);
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
        assert!(landlock_abi_version().is_ok_and(|abi| abi >= super::LANDLOCK_ABI_REQUIRED));
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
        let high_input_fd = unsafe {
            libc::fcntl(
                original_input.as_raw_fd(),
                libc::F_DUPFD_CLOEXEC,
                minimum_input_fd,
            )
        };
        assert!(
            high_input_fd >= minimum_input_fd,
            "fixture input fd must exceed the child descriptor limit: {}",
            std::io::Error::last_os_error()
        );
        let input = unsafe { File::from_raw_fd(high_input_fd) };
        drop(original_input);
        let parent_secret_fd = File::open(&outside_path).unwrap();
        let input_fd = input.as_raw_fd();
        let parent_secret_fd_number = parent_secret_fd.as_raw_fd();
        let parent_pid = std::process::id();
        let sandbox =
            MediaChildSandbox::prepare(&std::env::current_exe().unwrap(), input, Some(&scratch))
                .unwrap();
        assert_eq!(sandbox.input_fd(), CHILD_INPUT_FD);

        let mut command = Command::new(sandbox.executable());
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
        assert!(sandbox.input_fd.is_none());
        let mut command = Command::new(sandbox.executable());
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
        let input_fd = env_fd("PUFFINBOX_LANDLOCK_INPUT_FD");
        let mut input_bytes = [0_u8; 64];
        let read = unsafe {
            libc::pread(
                input_fd,
                input_bytes.as_mut_ptr() as *mut libc::c_void,
                input_bytes.len(),
                0,
            )
        };
        let expected_input = b"only approved input bytes";
        assert_eq!(read, expected_input.len() as isize);
        assert_eq!(&input_bytes[..read as usize], expected_input);

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

        assert_eq!(unsafe { libc::kill(parent_pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
        assert_eq!(
            unsafe { libc::ptrace(libc::PTRACE_ATTACH, parent_pid, 0, 0) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
        let mut local = [0_u8; 1];
        let local_iovec = libc::iovec {
            iov_base: local.as_mut_ptr() as *mut libc::c_void,
            iov_len: local.len(),
        };
        let remote_iovec = libc::iovec {
            iov_base: std::ptr::null_mut(),
            iov_len: 1,
        };
        let result = unsafe {
            libc::syscall(
                libc::SYS_process_vm_readv,
                parent_pid,
                &local_iovec as *const libc::iovec,
                1_usize,
                &remote_iovec as *const libc::iovec,
                1_usize,
                0_usize,
            )
        };
        assert_eq!(result, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );

        let parent_pid_fd = unsafe { libc::syscall(libc::SYS_pidfd_open, parent_pid, 0_u32) };
        assert!(
            parent_pid_fd >= 0,
            "pidfd_open failed before the denied read"
        );
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_getfd,
                parent_pid_fd as libc::c_int,
                env_fd("PUFFINBOX_LANDLOCK_INPUT_PARENT_FD"),
                0_u32,
            )
        };
        assert_eq!(result, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
        unsafe { libc::close(parent_pid_fd as libc::c_int) };

        let socket = unsafe {
            libc::socket(
                libc::AF_INET,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                libc::IPPROTO_TCP,
            )
        };
        assert_eq!(socket, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );

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
        let mode = unsafe { libc::prctl(21, 0, 0, 0, 0) };
        let socket = unsafe {
            libc::socket(
                libc::AF_INET,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                libc::IPPROTO_TCP,
            )
        };
        assert_eq!(socket, -1, "socket succeeded in seccomp mode {mode}");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
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
        filter: &[libc::sock_filter],
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
