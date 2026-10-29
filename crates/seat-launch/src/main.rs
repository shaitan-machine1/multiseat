use clap::Parser;
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, Uid, User, execvp, fork, initgroups, setgid, setuid};
use seat_udev::SeatName;
use std::ffi::CString;
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd};
use std::process::ExitCode;
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Launch a compositor with an inherited multiseatd connection"
)]
struct Args {
    #[arg(long)]
    seat: SeatName,
    #[arg(long)]
    user: String,
    #[arg(long, default_value_t = 3)]
    fd: i32,
    #[arg(required = true, trailing_var_arg = true)]
    command: Vec<String>,
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("seat-launch: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<u8, Box<dyn std::error::Error>> {
    if args.fd < 3 {
        return Err("inherited descriptor must be at least 3".into());
    }
    let user = resolve_user(&args.user)?.ok_or("unknown user")?;
    let client = libmultiseatd::Client::system(Duration::from_secs(5))?;
    let (id, connection) = client.create_client(args.seat.as_str(), user.uid.as_raw())?;
    // Do not copy a D-Bus connection into the child lineage.
    drop(client);
    // SAFETY: seat-launch is single-threaded; the child sets up and execs.
    let result = match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => {
            drop(connection);
            loop {
                match waitpid(child, None) {
                    Ok(WaitStatus::Exited(_, code)) => break Ok(code.clamp(0, 255) as u8),
                    Ok(WaitStatus::Signaled(_, signal, _)) => {
                        break Ok((128 + signal as i32).clamp(0, 255) as u8);
                    }
                    Err(nix::errno::Errno::EINTR) => continue,
                    Err(error) => break Err(error.into()),
                    _ => continue,
                }
            }
        }
        Ok(ForkResult::Child) => return child_exec(args, user, connection),
        Err(error) => {
            drop(connection);
            Err(error.into())
        }
    };
    // EOF is normal lifetime; explicit revoke covers leaked compositor copies
    // and startup failure. It is safe if EOF already removed the lease. A lost
    // bus or daemon must not rewrite the compositor's exit status.
    match libmultiseatd::Client::system(Duration::from_secs(5))
        .and_then(|client| client.revoke_client(&id))
    {
        Ok(()) => result,
        Err(error) => {
            eprintln!("seat-launch: could not revoke client {id}: {error}");
            result
        }
    }
}

fn child_exec(
    args: Args,
    user: User,
    connection: OwnedFd,
) -> Result<u8, Box<dyn std::error::Error>> {
    let raw = connection.as_raw_fd();
    if raw != args.fd {
        // SAFETY: both descriptors are valid integers and dup2 atomically
        // replaces the destination.
        if unsafe { libc::dup2(raw, args.fd) } == -1 {
            return Err(std::io::Error::last_os_error().into());
        }
        drop(connection); // the duplicate at args.fd is the child's copy
    } else {
        let _ = connection.into_raw_fd(); // ownership is now the fixed fd
    }
    // SAFETY: F_GETFD/F_SETFD do not dereference pointers.
    let flags = unsafe { libc::fcntl(args.fd, libc::F_GETFD) };
    if flags == -1
        || unsafe { libc::fcntl(args.fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } == -1
    {
        return Err(std::io::Error::last_os_error().into());
    }

    let username = CString::new(user.name.clone())?;
    initgroups(&username, user.gid)?;
    setgid(user.gid)?;
    setuid(user.uid)?;

    // SAFETY: this single-threaded post-fork child immediately execs and does
    // not return to shared Rust state.
    unsafe {
        std::env::set_var("SEATD_FD", args.fd.to_string());
        std::env::set_var("LIBSEAT_BACKEND", "seatd");
        std::env::set_var("XDG_SEAT", args.seat.as_str());
        std::env::set_var("HOME", user.dir);
        std::env::set_var("USER", &user.name);
        std::env::set_var("LOGNAME", &user.name);
    }
    let command = args
        .command
        .iter()
        .map(|value| CString::new(value.as_str()))
        .collect::<Result<Vec<_>, _>>()?;
    execvp(&command[0], &command)?;
    unreachable!()
}

fn resolve_user(value: &str) -> nix::Result<Option<User>> {
    if let Ok(uid) = value.parse::<u32>() {
        User::from_uid(Uid::from_raw(uid))
    } else {
        User::from_name(value)
    }
}
