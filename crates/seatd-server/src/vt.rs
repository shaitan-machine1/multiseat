use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;

pub trait VtOps: Send + 'static {
    fn current(&mut self) -> io::Result<i32>;
    fn open(&mut self, vt: i32) -> io::Result<()>;
    fn close(&mut self, vt: i32) -> io::Result<()>;
    fn switch(&mut self, current: i32, target: i32) -> io::Result<()>;
    fn ack_release(&mut self) -> io::Result<()>;
    fn ack_acquire(&mut self) -> io::Result<()>;
}

impl<T: VtOps + ?Sized> VtOps for Box<T> {
    fn current(&mut self) -> io::Result<i32> {
        (**self).current()
    }

    fn open(&mut self, vt: i32) -> io::Result<()> {
        (**self).open(vt)
    }

    fn close(&mut self, vt: i32) -> io::Result<()> {
        (**self).close(vt)
    }

    fn switch(&mut self, current: i32, target: i32) -> io::Result<()> {
        (**self).switch(current, target)
    }

    fn ack_release(&mut self) -> io::Result<()> {
        (**self).ack_release()
    }

    fn ack_acquire(&mut self) -> io::Result<()> {
        (**self).ack_acquire()
    }
}

#[derive(Default)]
pub struct LinuxVtOps {
    terminals: BTreeMap<i32, File>,
}

impl LinuxVtOps {
    fn tty(vt: i32) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
            .open(format!("/dev/tty{vt}"))
    }
}

impl VtOps for LinuxVtOps {
    fn current(&mut self) -> io::Result<i32> {
        let console = Self::tty(0)?;
        let mut state = VtStat::default();
        ioctl_ptr_mut(&console, VT_GETSTATE, &mut state)?;
        Ok(i32::from(state.active))
    }

    fn open(&mut self, vt: i32) -> io::Result<()> {
        if self.terminals.contains_key(&vt) {
            return Ok(());
        }
        let tty = Self::tty(vt)?;
        let mode = VtMode {
            mode: VT_PROCESS,
            waitv: 0,
            relsig: libc::SIGUSR1 as i16,
            acqsig: libc::SIGUSR2 as i16,
            frsig: 0,
        };
        ioctl_ptr(&tty, VT_SETMODE, &mode)?;
        if let Err(error) = ioctl_value(&tty, KDSKBMODE, K_OFF)
            .and_then(|()| ioctl_value(&tty, KDSETMODE, KD_GRAPHICS))
        {
            let _ = restore_terminal(&tty);
            return Err(error);
        }
        self.terminals.insert(vt, tty);
        Ok(())
    }

    fn close(&mut self, vt: i32) -> io::Result<()> {
        let tty = match self.terminals.remove(&vt) {
            Some(tty) => tty,
            None => Self::tty(vt)?,
        };
        restore_terminal(&tty)
    }

    fn switch(&mut self, current: i32, target: i32) -> io::Result<()> {
        let tty = Self::tty(current)?;
        ioctl_value(&tty, VT_ACTIVATE, target)
    }

    fn ack_release(&mut self) -> io::Result<()> {
        let vt = self.current()?;
        let tty = Self::tty(vt)?;
        ioctl_value(&tty, VT_RELDISP, VT_RELEASE)
    }

    fn ack_acquire(&mut self) -> io::Result<()> {
        let vt = self.current()?;
        let tty = Self::tty(vt)?;
        ioctl_value(&tty, VT_RELDISP, VT_ACKACQ)
    }
}

impl Drop for LinuxVtOps {
    fn drop(&mut self) {
        for (vt, tty) in &self.terminals {
            if let Err(error) = restore_terminal(tty) {
                tracing::error!(vt, %error, "failed to restore VT during cleanup");
            }
        }
    }
}

fn restore_terminal(tty: &File) -> io::Result<()> {
    let mode = VtMode {
        mode: VT_AUTO,
        waitv: 0,
        relsig: 0,
        acqsig: 0,
        frsig: 0,
    };
    let first = ioctl_ptr(tty, VT_SETMODE, &mode).err();
    let second = ioctl_value(tty, KDSKBMODE, K_UNICODE).err();
    let third = ioctl_value(tty, KDSETMODE, KD_TEXT).err();
    first.or(second).or(third).map_or(Ok(()), Err)
}

pub fn block_vt_signals() -> io::Result<()> {
    let mask = vt_signal_mask()?;
    // SAFETY: mask is initialized and the old mask is intentionally ignored.
    let result = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut()) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(result))
    }
}

pub(crate) fn signalfd() -> io::Result<OwnedFd> {
    let mask = vt_signal_mask()?;
    // SAFETY: signalfd creates a new descriptor for the initialized mask.
    let fd = unsafe { libc::signalfd(-1, &mask, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) };
    if fd == -1 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: ownership of the newly created descriptor is transferred.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

pub(crate) fn read_signal(fd: &OwnedFd) -> io::Result<Option<i32>> {
    let mut info = std::mem::MaybeUninit::<libc::signalfd_siginfo>::uninit();
    // SAFETY: the buffer is large enough for one signalfd_siginfo structure.
    let result = unsafe {
        libc::read(
            fd.as_raw_fd(),
            info.as_mut_ptr().cast(),
            size_of::<libc::signalfd_siginfo>(),
        )
    };
    if result == size_of::<libc::signalfd_siginfo>() as isize {
        // SAFETY: a complete structure was written by read.
        return Ok(Some(unsafe { info.assume_init() }.ssi_signo as i32));
    }
    if result == -1 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(error);
    }
    Err(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "short read from VT signalfd",
    ))
}

fn vt_signal_mask() -> io::Result<libc::sigset_t> {
    // SAFETY: sigemptyset initializes the set before sigaddset uses it.
    let mut mask = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    if unsafe { libc::sigemptyset(&mut mask) } == -1
        || unsafe { libc::sigaddset(&mut mask, libc::SIGUSR1) } == -1
        || unsafe { libc::sigaddset(&mut mask, libc::SIGUSR2) } == -1
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(mask)
    }
}

#[repr(C)]
struct VtMode {
    mode: u8,
    waitv: u8,
    relsig: i16,
    acqsig: i16,
    frsig: i16,
}

#[repr(C)]
#[derive(Default)]
struct VtStat {
    active: u16,
    signal: u16,
    state: u16,
}

const VT_AUTO: u8 = 0;
const VT_PROCESS: u8 = 1;
const VT_SETMODE: libc::c_ulong = 0x5602;
const VT_GETSTATE: libc::c_ulong = 0x5603;
const VT_RELDISP: libc::c_ulong = 0x5605;
const VT_ACTIVATE: libc::c_ulong = 0x5606;
const VT_RELEASE: i32 = 1;
const VT_ACKACQ: i32 = 2;
const KDSETMODE: libc::c_ulong = 0x4b3a;
const KD_TEXT: i32 = 0;
const KD_GRAPHICS: i32 = 1;
const KDSKBMODE: libc::c_ulong = 0x4b45;
const K_UNICODE: i32 = 0x03;
const K_OFF: i32 = 0x04;

fn ioctl_ptr<T>(file: &File, request: libc::c_ulong, value: &T) -> io::Result<()> {
    // SAFETY: request expects a pointer to the matching live kernel structure.
    if unsafe { libc::ioctl(file.as_raw_fd(), request, value) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn ioctl_ptr_mut<T>(file: &File, request: libc::c_ulong, value: &mut T) -> io::Result<()> {
    // SAFETY: request writes to the matching live kernel structure.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vt_signals_can_be_blocked_and_consumed_through_signalfd() {
        block_vt_signals().unwrap();
        let fd = signalfd().unwrap();
        // SAFETY: raises the signal in this test process; it is blocked and
        // therefore becomes readable through signalfd rather than delivered.
        assert_eq!(unsafe { libc::raise(libc::SIGUSR1) }, 0);
        assert_eq!(read_signal(&fd).unwrap(), Some(libc::SIGUSR1));
        assert_eq!(read_signal(&fd).unwrap(), None);
    }
}
