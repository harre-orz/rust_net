use std::io;
use std::mem;
use std::ptr;
use error_code::{ErrorCode, READY, CANCELED, errno, eof, stopped, canceled};
use unsafe_cell::{UnsafeRefCell};
use {IoObject, IoService, Handler};
use io_service::{IoActor};
use backbone::{RawFd, AsRawFd, AsIoActor, getnonblock, setnonblock};
use backbone::ops::{cancel_io};
use libc::{self, EINTR, EAGAIN, SFD_CLOEXEC, SIG_SETMASK, c_void, sigset_t, signalfd_siginfo,
           signalfd, sigemptyset, sigaddset, sigdelset, pthread_sigmask};

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum Signal {
    /// Hangup detected on controlling terminal or death of controlling process.
    SIGHUP = libc::SIGHUP as isize,

    /// Interrupt from keyboard.
    SIGINT = libc::SIGINT as isize,

    /// Quit from keyboard.
    SIGQUIT = libc::SIGQUIT as isize,

    /// Illegal Instruction.
    SIGILL = libc::SIGILL as isize,

    /// Abort signal from abort(3)
    SIGABRT = libc::SIGABRT as isize,

    /// Floating point exception.
    SIGFPE = libc::SIGFPE as isize,

    /// Kill signal.
    SIGKILL = libc::SIGKILL as isize,

    /// Invalid memory reference.
    SIGSEGV = libc::SIGSEGV as isize,

    /// Broken pipe: write to pipe with no readers.
    SIGPIPE = libc::SIGPIPE as isize,

    /// Timer signal from alarm(2).
    SIGALRM = libc::SIGALRM as isize,

    /// Termination signal.
    SIGTERM = libc::SIGTERM as isize,

    /// User-defined signal 1.
    SIGUSR1 = libc::SIGUSR1 as isize,

    /// User-defined signal 2.
    SIGUSR2 = libc::SIGUSR2 as isize,

    /// Child stopped of terminated.
    SIGCHLD = libc::SIGCHLD as isize,

    /// Continue if stopped.
    SIGCONT = libc::SIGCONT as isize,

    /// Stop process.
    SIGSTOP = libc::SIGSTOP as isize,

    /// Stop typed at terminal.
    SIGTSTP = libc::SIGTSTP as isize,

    /// Terminal input for background process.
    SIGTTIN = libc::SIGTTIN as isize,

    /// Terminal output for background process.
    SIGTTOU = libc::SIGTTOU as isize,

    /// Bus error (bad memory access).
    SIGBUS = libc::SIGBUS as isize,

    /// Pollable event (Sys V). Synonym for SIGIO.
    SIGPOLL = libc::SIGPOLL as isize,

    /// Profiling timer expired.
    SIGPROF = libc::SIGPROF as isize,

    /// Bad argument to routine (SVr4).
    SIGSYS = libc::SIGSYS as isize,

    /// Trace/breakpoint trap.
    SIGTRAP = libc::SIGTRAP as isize,

    /// Urgent condition on socket (4.2BSD).
    SIGURG = libc::SIGURG as isize,

    /// Virtual alarm clock (4.2BSD).
    SIGVTALRM = libc::SIGVTALRM as isize,

    /// CPU time limit exceeded (4.2BSD).
    SIGXCPU = libc::SIGXCPU as isize,

    /// File size limit exceeded (4.2BSD).
    SIGXFSZ = libc::SIGXFSZ as isize,
}

fn signalfd_init() -> io::Result<(RawFd, sigset_t)> {
    let mut mask: sigset_t = unsafe { mem::uninitialized() };
    libc_ign!(sigemptyset(&mut mask));
    let fd = libc_try!(signalfd(-1, &mask, SFD_CLOEXEC));
    Ok((fd, mask))
}

fn signalfd_add<T: AsRawFd>(fd: &T, mask: &mut sigset_t, signal: Signal) -> io::Result<()> {
    libc_try!(sigaddset(mask, signal as i32));
    libc_ign!(pthread_sigmask(SIG_SETMASK, mask, ptr::null_mut()));
    libc_ign!(signalfd(fd.as_raw_fd(), mask, 0));
    Ok(())
}

fn signalfd_del<T: AsRawFd>(fd: &T, mask: &mut sigset_t, signal: Signal) -> io::Result<()> {
    libc_try!(sigdelset(mask, signal as i32));
    libc_ign!(pthread_sigmask(SIG_SETMASK, mask, ptr::null_mut()));
    libc_ign!(signalfd(fd.as_raw_fd(), mask, 0));
    Ok(())
}

fn signalfd_reset<T: AsRawFd>(fd: &T, mask: &mut sigset_t) -> io::Result<()> {
    libc_try!(sigemptyset(mask));
    libc_ign!(pthread_sigmask(SIG_SETMASK, mask, ptr::null_mut()));
    libc_ign!(signalfd(fd.as_raw_fd(), mask, 0));
    Ok(())
}

fn signalfd_read<T: AsIoActor>(fd: &T) -> io::Result<Signal> {
    if let Some(handler) = fd.as_io_actor().unset_input(false) {
        handler(fd.io_service(), CANCELED);
    }

    while !fd.io_service().stopped() {
        let mut ssi: signalfd_siginfo = unsafe { mem::uninitialized() };
        let len = unsafe { libc::read(
            fd.as_raw_fd(),
            &mut ssi as *mut _ as *mut c_void,
            mem::size_of::<signalfd_siginfo>()
        ) };
        if len > 0 {
            return Ok(unsafe { mem::transmute(ssi.ssi_signo as i8) });
        }
        if len == 0 {
            return Err(eof());
        }
        let ec = errno();
        if ec != EINTR {
            return Err(io::Error::from_raw_os_error(ec));
        }
    }

    Err(stopped())
}

fn signalfd_async_read<T: AsIoActor, F: Handler<Signal>>(fd: &T, handler: F) {
    let fd_ptr = UnsafeRefCell::new(fd);

    fd.as_io_actor().set_input(Box::new(move |io: *const IoService, ec: ErrorCode| {
        let io = unsafe { &*io };

        match ec {
            READY => {
                let fd = unsafe { fd_ptr.as_ref() };
                let mut ssi: signalfd_siginfo = unsafe { mem::uninitialized() };

                if let Some(new_handler) = fd.as_io_actor().unset_input(false) {
                    io.post(|io| new_handler(io, READY));
                    handler.callback(io, Err(canceled()));
                    return;
                }

                let mode = getnonblock(fd).unwrap();
                setnonblock(fd, true).unwrap();

                while !io.stopped() {
                    let len = unsafe { libc::read(
                        fd.as_raw_fd(),
                        &mut ssi as *mut _ as *mut c_void,
                        mem::size_of::<signalfd_siginfo>()
                    ) };
                    if len > 0 {
                        fd.as_io_actor().ready_input();
                        setnonblock(fd, mode).unwrap();
                        handler.callback(io, Ok(unsafe { mem::transmute(ssi.ssi_signo as u8) }));
                        return;
                    }
                    if len == 0 {
                        fd.as_io_actor().ready_input();
                        handler.callback(io, Err(eof()));
                        return;
                    }
                    let ec = errno();
                    if ec == EAGAIN {
                        setnonblock(fd, mode).unwrap();
                        signalfd_async_read(fd, handler);
                        return;
                    }
                    if ec != EINTR {
                        fd.as_io_actor().ready_input();
                        setnonblock(fd, mode).unwrap();
                        handler.callback(io, Err(io::Error::from_raw_os_error(ec)));
                        return;
                    }
                }
                fd.as_io_actor().ready_input();
                setnonblock(fd, mode).unwrap();
                handler.callback(io, Err(stopped()));
            },
            CANCELED => handler.callback(io, Err(canceled())),
            ErrorCode(ec) => handler.callback(io, Err(io::Error::from_raw_os_error(ec))),
        }
    }));
}

/// Provides a signal.
pub struct SignalSet {
    act: IoActor,
    mask: sigset_t,
}

impl SignalSet {
    pub fn new<T: IoObject>(io: &T) -> io::Result<SignalSet> {
        let (fd, mask) = try!(signalfd_init());
        Ok(SignalSet {
            act: IoActor::new(io, fd),
            mask: mask,
        })
    }

    pub fn add(&mut self, signal: Signal) -> io::Result<()> {
        signalfd_add(&self.act, &mut self.mask, signal)
    }

    pub fn async_wait<F: Handler<Signal>>(&self, handler: F) {
        signalfd_async_read(self, handler)
    }

    pub fn cancel(&self) {
        cancel_io(self)
    }

    pub fn clear(&mut self) -> io::Result<()> {
        signalfd_reset(&self.act, &mut self.mask)
    }

    pub fn remove(&mut self, signal: Signal) -> io::Result<()> {
        signalfd_del(&self.act, &mut self.mask, signal)
    }

    pub fn wait(&self) -> io::Result<Signal> {
        signalfd_read(self)
    }
}

impl IoObject for SignalSet {
    fn io_service(&self) -> &IoService {
        self.act.io_service()
    }
}

impl AsRawFd for SignalSet {
    fn as_raw_fd(&self) -> RawFd {
        self.act.as_raw_fd()
    }
}

impl AsIoActor for SignalSet {
    fn as_io_actor(&self) -> &IoActor {
        &self.act
    }
}

impl Drop for SignalSet {
    fn drop(&mut self) {
        signalfd_reset(&self.act, &mut self.mask).unwrap();
    }
}

pub fn raise(signal: Signal) -> io::Result<()> {
    libc_try!(libc::raise(signal as i32));
    Ok(())
}

#[test]
fn test_signal_set() {
    use IoService;

    let io = &IoService::new();
    let mut sig = SignalSet::new(io).unwrap();
    sig.add(Signal::SIGHUP).unwrap();
    sig.add(Signal::SIGUSR1).unwrap();
    sig.remove(Signal::SIGUSR1).unwrap();
    sig.remove(Signal::SIGUSR2).unwrap();
}

#[test]
fn test_signal_set_wait() {
    use IoService;

    let io = &IoService::new();
    let mut sig = SignalSet::new(io).unwrap();
    sig.add(Signal::SIGHUP).unwrap();
    sig.add(Signal::SIGUSR1).unwrap();
    raise(Signal::SIGHUP).unwrap();
    raise(Signal::SIGUSR1).unwrap();
    assert_eq!(sig.wait().unwrap(), Signal::SIGHUP);
    assert_eq!(sig.wait().unwrap(), Signal::SIGUSR1);
}