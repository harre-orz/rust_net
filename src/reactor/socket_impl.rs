use super::Handle;
use ffi::{RawFd, AsRawFd, SystemError, close, OPERATION_CANCELED, Timeout};
use core::{IoContext, AsIoContext, ThreadIoContext, Perform};

pub struct SocketImpl<T> {
    pub data: T,
    ctx: IoContext,
    fd: Handle,
    pub timeout: Timeout,
}

impl<T> SocketImpl<T> {
    pub fn new(ctx: &IoContext, fd: RawFd, data: T) -> Box<Self> {
        let soc = Box::new(SocketImpl {
            data: data,
            ctx: ctx.clone(),
            fd: Handle::socket(fd),
            timeout: Timeout::max(),
        });
        ctx.as_reactor().register_socket(&soc.fd);
        soc
    }

    pub fn add_read_op(&self, this: &mut ThreadIoContext, op: Box<Perform>, err: SystemError) {
        self.ctx.as_reactor().add_read_op(&self.fd, this, op, err)
    }

    pub fn add_write_op(&self, this: &mut ThreadIoContext, op: Box<Perform>, err: SystemError) {
        self.ctx.as_reactor().add_write_op(&self.fd, this, op, err)
    }

    pub fn next_read_op(&self, this: &mut ThreadIoContext) {
        self.ctx.as_reactor().next_read_op(&self.fd, this)
    }

    pub fn next_write_op(&self, this: &mut ThreadIoContext) {
        self.ctx.as_reactor().next_write_op(&self.fd, this)
    }

    pub fn cancel(&self) {
        self.ctx.clone().as_reactor().cancel_ops(
            &self.fd,
            &self.ctx,
            OPERATION_CANCELED,
        )
    }
}

unsafe impl<T> AsIoContext for SocketImpl<T> {
    fn as_ctx(&self) -> &IoContext {
        if let Some(this) = ThreadIoContext::callstack(&self.ctx) {
            this.as_ctx()
        } else {
            &self.ctx
        }
    }
}

impl<T> AsRawFd for SocketImpl<T> {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl<T> Drop for SocketImpl<T> {
    fn drop(&mut self) {
        self.ctx.as_reactor().deregister_socket(&self.fd);
        close(self.fd.as_raw_fd())
    }
}
