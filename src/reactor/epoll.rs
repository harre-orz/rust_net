use super::Intr;
use ffi::{AsRawFd, RawFd, SystemError, OPERATION_CANCELED, close, sock_error};
use core::{AsIoContext, IoContext, ThreadIoContext, Perform};
use timer::TimerQueue;

use std::io;
use std::mem;
use std::sync::Mutex;
use std::collections::{HashSet, VecDeque};
use std::ops::{Deref, DerefMut};
use std::hash::{Hash, Hasher};
use libc::{self, epoll_event, epoll_create1, epoll_ctl, epoll_wait, EPOLLIN, EPOLLOUT, EPOLLERR,
           EPOLLHUP, EPOLLET, EPOLL_CLOEXEC, EPOLL_CTL_ADD, EPOLL_CTL_DEL};

fn dispatch_socket(ev: &epoll_event, this: &mut ThreadIoContext) {
    let eev = unsafe { &mut *(ev.u64 as *mut Epoll) };
    if (ev.events & (EPOLLERR | EPOLLHUP) as u32) != 0 {
        let err = sock_error(eev);
        this.as_ctx().clone().as_reactor().cancel_ops_nolock(
            eev,
            this.as_ctx(),
            err,
        );
        return;
    }
    if (ev.events & EPOLLIN as u32) as u32 != 0 {
        if let Some(op) = eev.input.queue.pop_front() {
            eev.input.blocked = true;
            this.push(op, SystemError::default());
        }
    }
    if (ev.events & EPOLLOUT as u32) as u32 != 0 {
        if let Some(op) = eev.output.queue.pop_front() {
            eev.output.blocked = true;
            this.push(op, SystemError::default());
        }
    }
}

fn dispatch_intr(ev: &epoll_event, _: &mut ThreadIoContext) {
    let eev = unsafe { &*(ev.u64 as *const Epoll) };
    if (ev.events & EPOLLIN as u32) != 0 {
        unsafe {
            let mut buf: [u8; 8] = mem::uninitialized();
            libc::read(eev.fd, buf.as_mut_ptr() as *mut _, buf.len());
        }
    }
}

#[derive(Default)]
struct Ops {
    queue: VecDeque<Box<Perform>>,
    blocked: bool,
    canceled: bool,
}

pub struct Epoll {
    fd: RawFd,
    input: Ops,
    output: Ops,
    dispatch: fn(&epoll_event, &mut ThreadIoContext),
}

impl Epoll {
    pub fn socket(fd: RawFd) -> Self {
        Epoll {
            fd: fd,
            input: Default::default(),
            output: Default::default(),
            dispatch: dispatch_socket,
        }
    }

    pub fn intr(fd: RawFd) -> Self {
        Epoll {
            fd: fd,
            input: Default::default(),
            output: Default::default(),
            dispatch: dispatch_intr,
        }
    }
}

impl AsRawFd for Epoll {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

struct EpollRef(*const Epoll);

impl PartialEq for EpollRef {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for EpollRef {}

impl Hash for EpollRef {
    fn hash<H>(&self, state: &mut H)
    where
        H: Hasher,
    {
        state.write_usize(self.0 as usize)
    }
}

impl Deref for EpollRef {
    type Target = Epoll;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.0 }
    }
}

impl DerefMut for EpollRef {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *(self.0 as *mut Epoll) }
    }
}

pub struct EpollReactor {
    epfd: RawFd,
    mutex: Mutex<HashSet<EpollRef>>,
    intr: Intr,
    pub tq: TimerQueue,
}

impl EpollReactor {
    pub fn new() -> io::Result<Self> {
        match unsafe { epoll_create1(EPOLL_CLOEXEC) } {
            -1 => Err(SystemError::last_error().into()),
            epfd => Ok(EpollReactor {
                epfd: epfd,
                mutex: Default::default(),
                intr: Intr::new()?,
                tq: TimerQueue::new()?,
            }),
        }
    }

    pub fn init(&self) {
        self.intr.startup(self);
        self.tq.startup(self);
    }

    pub fn poll(&self, block: bool, this: &mut ThreadIoContext) {
        let timeout = if block {
            self.tq.wait_duration(10 * 1_000_000_000) / 1_000_000
        } else {
            0
        } as i32;

        let mut events: [epoll_event; 128] = unsafe { mem::uninitialized() };
        let n = unsafe { epoll_wait(self.epfd, events.as_mut_ptr(), events.len() as i32, timeout) };

        self.tq.get_ready_timers(this);
        if n > 0 {
            let _epoll = self.mutex.lock().unwrap();
            for ev in &events[..(n as usize)] {
                let eev = unsafe { &*(ev.u64 as *mut Epoll) };
                (eev.dispatch)(ev, this)
            }
        }
    }

    fn epoll_ctl(&self, eev: &Epoll, op: i32, events: i32) {
        let mut ev = epoll_event {
            events: events as u32,
            u64: eev as *const _ as u64,
        };
        unsafe { epoll_ctl(self.epfd, op, eev.fd, &mut ev) };
    }

    pub fn register_socket(&self, eev: &Epoll) {
        self.epoll_ctl(eev, EPOLL_CTL_ADD, EPOLLIN | EPOLLOUT | EPOLLET)
    }

    pub fn deregister_socket(&self, eev: &Epoll) {
        self.epoll_ctl(eev, EPOLL_CTL_DEL, 0)
    }

    pub fn register_intr(&self, eev: &Epoll) {
        self.epoll_ctl(eev, EPOLL_CTL_ADD, EPOLLIN | EPOLLET)
    }

    pub fn deregister_intr(&self, eev: &Epoll) {
        self.deregister_socket(eev)
    }

    pub fn interrupt(&self) {
        self.intr.interrupt()
    }

    pub fn add_read_op(
        &self,
        eev: &Epoll,
        this: &mut ThreadIoContext,
        op: Box<Perform>,
        err: SystemError,
    ) {
        let ops = &mut EpollRef(eev).input;
        let _ep = self.mutex.lock().unwrap();
        if err == SystemError::default() {
            if ops.queue.is_empty() && !ops.blocked {
                ops.blocked = true;
                this.push(op, SystemError::default());
            } else {
                ops.queue.push_back(op);
            }
        } else if ops.canceled {
            ops.queue.push_front(op);
            for op in ops.queue.drain(..) {
                this.push(op, OPERATION_CANCELED);
            }
        } else {
            ops.blocked = false;
            ops.queue.push_front(op);
        }
    }

    pub fn add_write_op(
        &self,
        eev: &Epoll,
        this: &mut ThreadIoContext,
        op: Box<Perform>,
        err: SystemError,
    ) {
        let ops = &mut EpollRef(eev).output;
        let _epoll = self.mutex.lock().unwrap();
        if err == SystemError::default() {
            if ops.queue.is_empty() && !ops.blocked {
                ops.blocked = true;
                this.push(op, SystemError::default());
            } else {
                ops.queue.push_back(op);
            }
        } else if ops.canceled {
            ops.queue.push_front(op);
            for op in ops.queue.drain(..) {
                this.push(op, OPERATION_CANCELED);
            }
        } else {
            println!("add wirte_op {}", err);
            ops.blocked = false;
            ops.queue.push_front(op);
        }
    }

    pub fn next_read_op(&self, eev: &Epoll, this: &mut ThreadIoContext) {
        let ops = &mut EpollRef(eev).input;
        let _epoll = self.mutex.lock().unwrap();
        if ops.canceled {
            ops.canceled = false;
            for op in ops.queue.drain(..) {
                this.push(op, OPERATION_CANCELED);
            }
        } else {
            if let Some(op) = ops.queue.pop_front() {
                this.push(op, SystemError::default());
            } else {
                ops.blocked = false;
            }
        }
    }

    pub fn next_write_op(&self, eev: &Epoll, this: &mut ThreadIoContext) {
        let ops = &mut EpollRef(eev).output;
        let _epoll = self.mutex.lock().unwrap();
        if ops.canceled {
            ops.canceled = false;
            for op in ops.queue.drain(..) {
                this.push(op, OPERATION_CANCELED);
            }
        } else {
            if let Some(op) = ops.queue.pop_front() {
                this.push(op, SystemError::default());
            } else {
                ops.blocked = false;
            }
        }
    }

    pub fn cancel_ops(&self, eev: &Epoll, ctx: &IoContext, err: SystemError) {
        let _epoll = self.mutex.lock().unwrap();
        self.cancel_ops_nolock(eev, ctx, err)
    }

    fn cancel_ops_nolock(&self, eev: &Epoll, ctx: &IoContext, err: SystemError) {
        for ops in &mut [&mut EpollRef(eev).input, &mut EpollRef(eev).output] {
            if !ops.canceled {
                ops.canceled = true;
                if !ops.blocked {
                    for op in ops.queue.drain(..) {
                        ctx.do_post((op, err))
                    }
                }
            }
        }
    }
}

impl Drop for EpollReactor {
    fn drop(&mut self) {
        self.intr.cleanup(self);
        close(self.epfd);
    }
}
