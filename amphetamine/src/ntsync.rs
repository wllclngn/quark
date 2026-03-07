// ntsync -- kernel-native NT sync primitive driver wrapper
//
// Wraps /dev/ntsync ioctls (Linux 6.14+) to provide atomic
// semaphore, mutex, and event operations backed by the kernel.
// Falls back gracefully: NtsyncDevice::open() returns None
// on older kernels without the driver.
//
// Each NtsyncObj is a file descriptor returned by the kernel.
// Drop closes the FD automatically.

use std::os::unix::io::RawFd;

// ---- ioctl codes (computed from _IOW/_IOR/_IOWR macros) ----
// type = 'N' = 0x4E, sizes from /usr/include/linux/ntsync.h

const NTSYNC_IOC_CREATE_SEM:   u64 = 0x40084E80; // _IOW ('N', 0x80, 8)
const NTSYNC_IOC_SEM_RELEASE:  u64 = 0xC0044E81; // _IOWR('N', 0x81, 4)
const NTSYNC_IOC_WAIT_ANY:     u64 = 0xC0284E82; // _IOWR('N', 0x82, 40)
const NTSYNC_IOC_WAIT_ALL:     u64 = 0xC0284E83; // _IOWR('N', 0x83, 40)
const NTSYNC_IOC_CREATE_MUTEX: u64 = 0x40084E84; // _IOW ('N', 0x84, 8)
const NTSYNC_IOC_MUTEX_UNLOCK: u64 = 0xC0084E85; // _IOWR('N', 0x85, 8)
const NTSYNC_IOC_CREATE_EVENT: u64 = 0x40084E87; // _IOW ('N', 0x87, 8)
const NTSYNC_IOC_EVENT_SET:    u64 = 0x80044E88; // _IOR ('N', 0x88, 4)
const NTSYNC_IOC_EVENT_RESET:  u64 = 0x80044E89; // _IOR ('N', 0x89, 4)
const NTSYNC_IOC_EVENT_PULSE:  u64 = 0x80044E8A; // _IOR ('N', 0x8a, 4)

// ---- Kernel structs (match /usr/include/linux/ntsync.h) ----

#[repr(C)]
struct NtsyncSemArgs {
    count: u32,
    max: u32,
}

#[repr(C)]
struct NtsyncMutexArgs {
    owner: u32,
    count: u32,
}

#[repr(C)]
struct NtsyncEventArgs {
    manual: u32,
    signaled: u32,
}

#[repr(C)]
struct NtsyncWaitArgs {
    timeout: u64,
    objs: u64,
    count: u32,
    index: u32,
    flags: u32,
    owner: u32,
    alert: u32,
    pad: u32,
}

// ---- Public types ----

pub enum WaitResult {
    Signaled(u32),  // index of signaled object
    Timeout,
    Error,
}

/// A single ntsync kernel object (semaphore, mutex, or event).
/// Owns the file descriptor; Drop closes it.
pub struct NtsyncObj {
    fd: RawFd,
}

impl NtsyncObj {
    /// Release (post) a semaphore. Returns previous count.
    pub fn sem_release(&self, count: u32) -> Result<u32, i32> {
        let mut val = count;
        let ret = unsafe {
            libc::ioctl(self.fd, NTSYNC_IOC_SEM_RELEASE, &mut val as *mut u32)
        };
        if ret < 0 { Err(errno()) } else { Ok(val) }
    }

    /// Unlock a mutex. Returns previous recursion count.
    pub fn mutex_unlock(&self, owner: u32) -> Result<u32, i32> {
        let mut args = NtsyncMutexArgs { owner, count: 0 };
        let ret = unsafe {
            libc::ioctl(self.fd, NTSYNC_IOC_MUTEX_UNLOCK, &mut args as *mut NtsyncMutexArgs)
        };
        if ret < 0 { Err(errno()) } else { Ok(args.count) }
    }

    /// Signal an event. Returns previous state.
    pub fn event_set(&self) -> Result<u32, i32> {
        let mut prev: u32 = 0;
        let ret = unsafe {
            libc::ioctl(self.fd, NTSYNC_IOC_EVENT_SET, &mut prev as *mut u32)
        };
        if ret < 0 { Err(errno()) } else { Ok(prev) }
    }

    /// Reset (unsignal) an event. Returns previous state.
    pub fn event_reset(&self) -> Result<u32, i32> {
        let mut prev: u32 = 0;
        let ret = unsafe {
            libc::ioctl(self.fd, NTSYNC_IOC_EVENT_RESET, &mut prev as *mut u32)
        };
        if ret < 0 { Err(errno()) } else { Ok(prev) }
    }

    /// Pulse an event (set then reset atomically). Returns previous state.
    pub fn event_pulse(&self) -> Result<u32, i32> {
        let mut prev: u32 = 0;
        let ret = unsafe {
            libc::ioctl(self.fd, NTSYNC_IOC_EVENT_PULSE, &mut prev as *mut u32)
        };
        if ret < 0 { Err(errno()) } else { Ok(prev) }
    }

    pub fn fd(&self) -> RawFd { self.fd }
}

impl Drop for NtsyncObj {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd); }
    }
}

/// Handle to /dev/ntsync device. One per triskelion instance.
pub struct NtsyncDevice {
    fd: RawFd,
}

impl NtsyncDevice {
    /// Try to open /dev/ntsync. Returns None if device doesn't exist.
    pub fn open() -> Option<Self> {
        let path = b"/dev/ntsync\0";
        let fd = unsafe {
            libc::open(path.as_ptr() as *const libc::c_char, libc::O_RDWR | libc::O_CLOEXEC)
        };
        if fd < 0 { None } else { Some(Self { fd }) }
    }

    pub fn create_sem(&self, count: u32, max: u32) -> Option<NtsyncObj> {
        let mut args = NtsyncSemArgs { count, max };
        let fd = unsafe {
            libc::ioctl(self.fd, NTSYNC_IOC_CREATE_SEM, &mut args as *mut NtsyncSemArgs)
        };
        if fd < 0 { None } else { Some(NtsyncObj { fd }) }
    }

    pub fn create_mutex(&self, owner: u32, count: u32) -> Option<NtsyncObj> {
        let mut args = NtsyncMutexArgs { owner, count };
        let fd = unsafe {
            libc::ioctl(self.fd, NTSYNC_IOC_CREATE_MUTEX, &mut args as *mut NtsyncMutexArgs)
        };
        if fd < 0 { None } else { Some(NtsyncObj { fd }) }
    }

    pub fn create_event(&self, manual: bool, signaled: bool) -> Option<NtsyncObj> {
        let mut args = NtsyncEventArgs {
            manual: manual as u32,
            signaled: signaled as u32,
        };
        let fd = unsafe {
            libc::ioctl(self.fd, NTSYNC_IOC_CREATE_EVENT, &mut args as *mut NtsyncEventArgs)
        };
        if fd < 0 { None } else { Some(NtsyncObj { fd }) }
    }

    /// Wait for any of the given objects to become signaled (poll with timeout=0).
    /// `obj_fds` is a slice of ntsync object file descriptors.
    /// `owner` is the thread ID for mutex ownership.
    pub fn wait_any(&self, obj_fds: &[RawFd], timeout_ns: u64, owner: u32) -> WaitResult {
        self.do_wait(NTSYNC_IOC_WAIT_ANY, obj_fds, timeout_ns, owner)
    }

    /// Wait for all objects to become signaled simultaneously.
    pub fn wait_all(&self, obj_fds: &[RawFd], timeout_ns: u64, owner: u32) -> WaitResult {
        self.do_wait(NTSYNC_IOC_WAIT_ALL, obj_fds, timeout_ns, owner)
    }

    fn do_wait(&self, ioctl_code: u64, obj_fds: &[RawFd], timeout_ns: u64, owner: u32) -> WaitResult {
        if obj_fds.is_empty() {
            return WaitResult::Timeout;
        }

        // Convert RawFd (i32) to u32 for the kernel
        let fds: Vec<u32> = obj_fds.iter().map(|&fd| fd as u32).collect();

        let mut args = NtsyncWaitArgs {
            timeout: timeout_ns,
            objs: fds.as_ptr() as u64,
            count: fds.len() as u32,
            index: 0,
            flags: 0, // CLOCK_MONOTONIC
            owner,
            alert: 0,
            pad: 0,
        };

        let ret = unsafe {
            libc::ioctl(self.fd, ioctl_code, &mut args as *mut NtsyncWaitArgs)
        };

        if ret == 0 {
            WaitResult::Signaled(args.index)
        } else {
            if errno() == libc::ETIMEDOUT {
                WaitResult::Timeout
            } else {
                WaitResult::Error
            }
        }
    }
}

impl Drop for NtsyncDevice {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd); }
    }
}

fn errno() -> i32 {
    unsafe { *libc::__errno_location() }
}
