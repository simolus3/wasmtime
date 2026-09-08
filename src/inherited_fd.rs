use std::env;
use std::mem::{self, MaybeUninit};
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::RawFd;
use std::process;
use std::{
    net::TcpListener,
    os::fd::OwnedFd,
    sync::{Mutex, OnceLock},
};

/// A file descriptor the current process has inherited from a parent process.
pub enum InheritedFileDescriptor {
    /// An inherited socket verified to be a TCP socket.
    TcpSocket(TcpListener),
}

static INHERITED_FDS: OnceLock<Mutex<Vec<InheritedFileDescriptor>>> = OnceLock::new();

/// Takes ownership of file descriptors this process has inherited from a parent process like a
/// service manager.
///
/// These are looked up with the [protocol from systemd](https://www.freedesktop.org/software/systemd/man/latest/sd_listen_fds.html#Notes).
/// This is used to implement socket activation for `wasmtime serve`.
///
/// # Safety
///
/// This must be called first in `main()`, before the file descriptors can be used by anything
/// else.
pub unsafe fn init_inherited_fds() {
    // The logic here is taken from https://github.com/systemd/systemd/blob/main/src/libsystemd/sd-daemon/sd-daemon.c,
    // the protocol is described in the "Notes" section of https://www.freedesktop.org/software/systemd/man/latest/sd_listen_fds.html#Notes.

    if !env::var("LISTEN_PID")
        .ok()
        .and_then(|pid| pid.parse().ok())
        .is_some_and(|pid: u32| pid == process::id())
    {
        // Not meant for this process, ignore.
        return;
    }

    let Some(num_fds) = env::var("LISTEN_FDS").ok().and_then(|fds| fds.parse().ok()) else {
        return;
    };

    let first_fd: RawFd = 3;
    let Some(last_fd) = first_fd.checked_add(num_fds) else {
        return;
    };

    let mut descriptors = Vec::with_capacity(num_fds as usize);
    for fd in first_fd..last_fd {
        let fd = unsafe {
            // Safety: This is called first in main and we checked the PID, so we have exclusive
            // access to this fd.
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            OwnedFd::from_raw_fd(fd)
        };

        if is_tcp_socket(&fd) {
            descriptors.push(InheritedFileDescriptor::TcpSocket(fd.into()));
        }
    }

    let _ = INHERITED_FDS.set(Mutex::new(descriptors));
}

/// Consumes inherited file descriptors obtained through [init_inherited_fds].
pub fn take_inherited_fds() -> Vec<InheritedFileDescriptor> {
    let Some(fds) = INHERITED_FDS.get() else {
        return Default::default();
    };

    let mut guard = fds.lock().unwrap();
    return mem::take(&mut *guard);
}

fn is_tcp_socket(fd: &OwnedFd) -> bool {
    let mut stat: MaybeUninit<libc::stat> = MaybeUninit::uninit();
    if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return false;
    }

    if (unsafe { stat.assume_init() }.st_mode & libc::S_IFMT) != libc::S_IFSOCK {
        return false;
    }

    let sa_family = unsafe {
        let mut sockaddr: MaybeUninit<libc::sockaddr> = MaybeUninit::uninit();
        let mut len = mem::size_of::<libc::sockaddr>() as libc::c_uint;

        if libc::getsockname(fd.as_raw_fd(), sockaddr.as_mut_ptr(), &mut len) != 0 {
            return false;
        }
        sockaddr.assume_init().sa_family
    } as libc::c_int;

    if sa_family != libc::AF_INET && sa_family != libc::AF_INET6 {
        return false;
    }

    let mut socket_type: libc::c_int = 0;
    unsafe {
        let mut type_len = mem::size_of_val(&socket_type) as libc::c_uint;

        if libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            std::ptr::from_mut(&mut socket_type).cast(),
            &mut type_len,
        ) != 0
        {
            return false;
        }
    }

    socket_type == libc::SOCK_STREAM
}
