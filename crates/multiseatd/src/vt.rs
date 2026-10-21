use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;

pub trait VtOps {
    fn open(&self, vt: i32) -> io::Result<()>;
    fn close(&self, vt: i32) -> io::Result<()>;
    fn switch(&self, current: i32, target: i32) -> io::Result<()>;
    /// The foreground virtual terminal according to the kernel.
    ///
    /// A VT-bound seat must resolve this whenever a client opens the seat:
    /// the daemon only learns about switches it arbitrates itself, so a
    /// cached value goes stale as soon as anything else changes the VT,
    /// including every switch made before the first client connected.
    fn current(&self) -> io::Result<i32>;
}

#[derive(Default)]
pub struct NoopVtOps;

impl VtOps for NoopVtOps {
    fn open(&self, _vt: i32) -> io::Result<()> {
        Ok(())
    }
    fn close(&self, _vt: i32) -> io::Result<()> {
        Ok(())
    }
    fn switch(&self, _current: i32, _target: i32) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::ENOTSUP))
    }
    fn current(&self) -> io::Result<i32> {
        Err(io::Error::from_raw_os_error(libc::ENOTSUP))
    }
}

#[derive(Default)]
pub struct LinuxVtOps;

impl LinuxVtOps {
    fn tty(vt: i32) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("/dev/tty{vt}"))
    }
}

impl VtOps for LinuxVtOps {
    fn open(&self, vt: i32) -> io::Result<()> {
        let tty = Self::tty(vt)?;
        let mode = VtMode {
            mode: VT_PROCESS,
            waitv: 0,
            relsig: libc::SIGUSR1 as i16,
            acqsig: libc::SIGUSR2 as i16,
            frsig: 0,
        };
        ioctl_ptr(&tty, VT_SETMODE, &mode)?;
        ioctl_value(&tty, KDSKBMODE, K_OFF)?;
        ioctl_value(&tty, KDSETMODE, KD_GRAPHICS)
    }

    fn close(&self, vt: i32) -> io::Result<()> {
        let tty = Self::tty(vt)?;
        let mode = VtMode {
            mode: VT_AUTO,
            waitv: 0,
            relsig: 0,
            acqsig: 0,
            frsig: 0,
        };
        let first = ioctl_ptr(&tty, VT_SETMODE, &mode).err();
        let second = ioctl_value(&tty, KDSKBMODE, K_UNICODE).err();
        let third = ioctl_value(&tty, KDSETMODE, KD_TEXT).err();
        first.or(second).or(third).map_or(Ok(()), Err)
    }

    fn switch(&self, current: i32, target: i32) -> io::Result<()> {
        let tty = Self::tty(current)?;
        ioctl_value(&tty, VT_ACTIVATE, target)
    }

    fn current(&self) -> io::Result<i32> {
        current_vt()
    }
}

/// Reads the foreground VT from `/dev/tty0`.
pub fn current_vt() -> io::Result<i32> {
    #[repr(C)]
    #[derive(Default)]
    struct VtStat {
        active: u16,
        signal: u16,
        state: u16,
    }
    let console = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty0")?;
    let mut state = VtStat::default();
    // SAFETY: VT_GETSTATE fills the vt_stat structure this call owns.
    if unsafe { libc::ioctl(console.as_raw_fd(), VT_GETSTATE, &mut state) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(i32::from(state.active))
}

#[repr(C)]
struct VtMode {
    mode: u8,
    waitv: u8,
    relsig: i16,
    acqsig: i16,
    frsig: i16,
}

const VT_AUTO: u8 = 0;
const VT_PROCESS: u8 = 1;
const VT_SETMODE: libc::c_ulong = 0x5602;
const VT_GETSTATE: libc::c_ulong = 0x5603;
const VT_ACTIVATE: libc::c_ulong = 0x5606;
const KDSETMODE: libc::c_ulong = 0x4b3a;
const KD_TEXT: i32 = 0;
const KD_GRAPHICS: i32 = 1;
const KDSKBMODE: libc::c_ulong = 0x4b45;
const K_UNICODE: i32 = 0x03;
const K_OFF: i32 = 0x04;

fn ioctl_ptr<T>(file: &File, request: libc::c_ulong, value: &T) -> io::Result<()> {
    // SAFETY: request expects a pointer to the matching kernel structure and
    // `value` remains alive for the duration of the call.
    if unsafe { libc::ioctl(file.as_raw_fd(), request, value) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn ioctl_value(file: &File, request: libc::c_ulong, value: i32) -> io::Result<()> {
    // SAFETY: these Linux VT/KD ioctls consume one integer value.
    if unsafe { libc::ioctl(file.as_raw_fd(), request, value) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
