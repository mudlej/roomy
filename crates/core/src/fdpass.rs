//! Passing an open file descriptor to another process over a unix socket.
//!
//! This is how a window gets its private connection to a VM's display: the
//! supervisor makes a socket pair, gives one end to QEMU and the other to
//! the window.

use std::io::{self, Write};
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

const FD_SIZE: u32 = size_of::<libc::c_int>() as u32;

/// Room for the control message of a handful of descriptors, aligned for `cmsghdr`.
type ControlBuffer = [u64; 16];

/// Sends `bytes` with `fd` attached to the first of them.
pub fn send_with_fd(stream: &UnixStream, bytes: &[u8], fd: BorrowedFd<'_>) -> io::Result<()> {
    if bytes.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "a descriptor needs at least one byte to travel with"));
    }
    let mut control: ControlBuffer = [0; 16];
    let mut iov = libc::iovec { iov_base: bytes.as_ptr() as *mut libc::c_void, iov_len: bytes.len() };
    // SAFETY: msghdr is plain data; all-zero is a valid value.
    let mut msg: libc::msghdr = unsafe { zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    // SAFETY: CMSG_SPACE only computes a size.
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(FD_SIZE) } as _;
    // SAFETY: the control buffer is large enough for one descriptor and aligned for cmsghdr.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&msg);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(FD_SIZE) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<libc::c_int>(), fd.as_raw_fd());
    }
    // SAFETY: msg points at buffers that outlive the call.
    let sent = retry_interrupted(|| unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) })?;
    let mut rest = stream;
    rest.write_all(&bytes[sent..])
}

/// Reads into `buf`, returning how many bytes arrived and the descriptor
/// that came with them, if any. Zero bytes means the other side closed.
pub fn recv_with_fd(stream: &UnixStream, buf: &mut [u8]) -> io::Result<(usize, Option<OwnedFd>)> {
    let mut control: ControlBuffer = [0; 16];
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    // SAFETY: msghdr is plain data; all-zero is a valid value.
    let mut msg: libc::msghdr = unsafe { zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = size_of::<ControlBuffer>() as _;
    // SAFETY: msg points at buffers that outlive the call.
    let received =
        retry_interrupted(|| unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) })?;

    let mut first = None;
    // SAFETY: the kernel filled the control buffer; the CMSG macros walk it within msg_controllen.
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&msg);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let payload = (*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let data = libc::CMSG_DATA(header).cast::<libc::c_int>();
                for index in 0..payload / FD_SIZE as usize {
                    // Every descriptor received is ours now: keep the first, close the rest.
                    let fd = OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(index)));
                    first.get_or_insert(fd);
                }
            }
            header = libc::CMSG_NXTHDR(&msg, header);
        }
    }
    Ok((received, first))
}

/// Makes a system call that returns a byte count, again for as long as a signal interrupts it.
fn retry_interrupted(mut call: impl FnMut() -> isize) -> io::Result<usize> {
    loop {
        let n = call();
        if n >= 0 {
            return Ok(n as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::fd::AsFd;

    #[test]
    fn a_descriptor_travels_with_the_bytes_and_stays_usable() {
        let (left, right) = UnixStream::pair().unwrap();
        let (mut ours, theirs) = UnixStream::pair().unwrap();
        send_with_fd(&left, b"take this\n", theirs.as_fd()).unwrap();
        drop(theirs);

        let mut buf = [0u8; 64];
        let (n, fd) = recv_with_fd(&right, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"take this\n");
        let mut received = UnixStream::from(fd.expect("a descriptor arrives with the bytes"));
        received.write_all(b"hello").unwrap();
        let mut heard = [0u8; 5];
        ours.read_exact(&mut heard).unwrap();
        assert_eq!(&heard, b"hello");
    }

    #[test]
    fn plain_bytes_arrive_without_a_descriptor() {
        let (mut left, right) = UnixStream::pair().unwrap();
        left.write_all(b"just text").unwrap();
        let mut buf = [0u8; 64];
        let (n, fd) = recv_with_fd(&right, &mut buf).unwrap();
        assert_eq!((&buf[..n], fd.is_none()), (&b"just text"[..], true));
    }
}
