//! Unix-socket helpers for receiving file descriptors via `SCM_RIGHTS`.
//!
//! MIT-SHM `AttachFd` (extension v1.2) passes a shared-memory file
//! descriptor in the cmsg of the same message that delivers the request
//! body. `std::os::unix::net::UnixStream::read` skips the cmsg, so we wrap
//! `recvmsg(2)` directly.

use std::{
    collections::VecDeque,
    io,
    mem::MaybeUninit,
    net::TcpStream,
    os::{
        fd::{AsRawFd, RawFd},
        unix::net::UnixStream,
    },
};

use crate::transport::Transport;

/// Read up to `buf.len()` bytes from `stream` and collect any file
/// descriptors that arrived via `SCM_RIGHTS` in the same message. Returns
/// `(bytes_read, fds)`. The caller owns the returned FDs and must close
/// them when done.
pub fn recv_with_fds(stream: &mut UnixStream, buf: &mut [u8]) -> io::Result<(usize, Vec<RawFd>)> {
    // SCM_RIGHTS cmsg holding up to 4 ints — wmaker's MIT-SHM AttachFd only
    // ever sends one, but oversize the buffer so the kernel can't truncate
    // the cmsg with MSG_CTRUNC under load.
    let mut cmsg_buf = [0u8; 64];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut msg: libc::msghdr = unsafe { MaybeUninit::zeroed().assume_init() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr().cast();
    msg.msg_controllen = cmsg_buf.len() as _;

    let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::other("recvmsg: cmsg truncated"));
    }

    let mut fds = Vec::new();
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    while !cmsg.is_null() {
        let header = unsafe { &*cmsg };
        if header.cmsg_level == libc::SOL_SOCKET && header.cmsg_type == libc::SCM_RIGHTS {
            let data = unsafe { libc::CMSG_DATA(cmsg) };
            // The cmsg payload is `cmsg_len - CMSG_LEN(0)` bytes wide, holding
            // packed `int` fds. Use CMSG_LEN(0) to find the header size in a
            // portable way (some libc implementations pad after the header).
            let header_len = unsafe { libc::CMSG_LEN(0) } as usize;
            let payload_len = (header.cmsg_len as usize).saturating_sub(header_len);
            let count = payload_len / std::mem::size_of::<libc::c_int>();
            for i in 0..count {
                let mut fd: libc::c_int = -1;
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        data.add(i * std::mem::size_of::<libc::c_int>()),
                        (&raw mut fd).cast(),
                        std::mem::size_of::<libc::c_int>(),
                    );
                }
                fds.push(fd as RawFd);
            }
        }
        cmsg = unsafe { libc::CMSG_NXTHDR(&msg, cmsg) };
    }

    Ok((n as usize, fds))
}

/// Send `bytes` together with `fd` over `stream` using `sendmsg(2)` so
/// the receiving end gets the FD via `SCM_RIGHTS`. Used by
/// MIT-SHM::CreateSegment and DRI3 to pass a server-allocated descriptor
/// back to the client in its reply.
///
/// All of `bytes` goes, or an error comes back. A client's socket may be
/// non-blocking and nearly full, so the `sendmsg` may refuse with `EAGAIN`
/// or (on a kernel that takes what room there is) send only part; the
/// descriptor rides with the first byte that goes, and the rest is written
/// after it, each wait bounded by [`SEND_WAIT_MS`]. Dropping the rest used to
/// tear the client's stream: its next reply began mid-message, and Xlib
/// ended the client with `XIO: fatal IO error` (Ferrix, Endfield's starts,
/// 2026-10-23).
pub fn send_with_fd(stream: &mut UnixStream, bytes: &[u8], fd: RawFd) -> io::Result<()> {
    let mut sent = loop {
        match send_once_with_fd(stream, bytes, fd) {
            Ok(n) => break n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => wait_writable(stream)?,
            Err(e) => return Err(e),
        }
    };
    while sent < bytes.len() {
        let rest = &bytes[sent..];
        // SAFETY: `rest` is a live slice for the length passed.
        let n = unsafe {
            libc::send(
                stream.as_raw_fd(),
                rest.as_ptr().cast(),
                rest.len(),
                libc::MSG_NOSIGNAL,
            )
        };
        if n >= 0 {
            sent += n as usize;
            continue;
        }
        let e = io::Error::last_os_error();
        match e.kind() {
            io::ErrorKind::Interrupted => {}
            io::ErrorKind::WouldBlock => wait_writable(stream)?,
            _ => return Err(e),
        }
    }
    Ok(())
}

/// How long [`send_with_fd`] waits for room each time before it gives the
/// client up as stuck.
const SEND_WAIT_MS: libc::c_int = 5_000;

/// Wait until `stream` can take more, or fail with `TimedOut`.
fn wait_writable(stream: &UnixStream) -> io::Result<()> {
    let mut pfd = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLOUT,
        revents: 0,
    };
    loop {
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&raw mut pfd, 1, SEND_WAIT_MS) };
        if n > 0 {
            return Ok(());
        }
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "client socket stayed full while a reply with a descriptor was sent",
            ));
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// One `sendmsg` of `bytes` with `fd` attached: the count it sent.
fn send_once_with_fd(stream: &UnixStream, bytes: &[u8], fd: RawFd) -> io::Result<usize> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut _,
        iov_len: bytes.len(),
    };
    // CMSG_SPACE(sizeof(int)) on Linux x86_64 is 24 bytes; allocate that.
    let mut cmsg_buf = [0u8; 24];
    let mut msg: libc::msghdr = unsafe { MaybeUninit::zeroed().assume_init() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr().cast();
    msg.msg_controllen = cmsg_buf.len() as _;
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&raw const msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32) as _;
        std::ptr::copy_nonoverlapping(
            (&raw const fd).cast(),
            libc::CMSG_DATA(cmsg),
            std::mem::size_of::<libc::c_int>(),
        );
        let n = libc::sendmsg(stream.as_raw_fd(), &raw const msg, libc::MSG_NOSIGNAL);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }
}

/// `Read` adapter over a `UnixStream` that uses `recvmsg(2)` under the hood
/// so any file descriptors that arrive via `SCM_RIGHTS` are queued for
/// later retrieval. The X11 dispatcher uses this to pull the FD that
/// accompanies a `MIT-SHM AttachFd` request.
pub(crate) struct UnixFdReader {
    stream: UnixStream,
    /// Bytes received but not yet handed out via `Read`.
    buf: Vec<u8>,
    /// Read cursor into `buf`.
    pos: usize,
    /// FDs received in cmsgs, in arrival order.
    fds: VecDeque<RawFd>,
}

impl UnixFdReader {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            buf: Vec::with_capacity(4096),
            pos: 0,
            fds: VecDeque::new(),
        }
    }

    /// Pop the next received FD, if any.
    fn pop_fd(&mut self) -> Option<RawFd> {
        self.fds.pop_front()
    }

    fn fill(&mut self) -> io::Result<()> {
        debug_assert!(
            self.pos == self.buf.len(),
            "UnixFdReader::fill called with bytes still buffered",
        );
        self.buf.clear();
        self.buf.resize(4096, 0);
        match recv_with_fds(&mut self.stream, &mut self.buf) {
            Ok((n, fds)) => {
                if n == 0 && fds.is_empty() {
                    // Peer closed the connection.
                    self.buf.clear();
                    self.pos = 0;
                    return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                }
                self.buf.truncate(n);
                self.pos = 0;
                for fd in fds {
                    self.fds.push_back(fd);
                }
                Ok(())
            }
            Err(e) => {
                // Restore the empty-buffer invariant so a caller that
                // retries (e.g. after `WouldBlock` + poll) sees a
                // clean state. Without this, `pos == buf.len` (the
                // debug assert at the top) would fail because we
                // already resized buf to 4096.
                self.buf.clear();
                self.pos = 0;
                Err(e)
            }
        }
    }
}

impl io::Read for UnixFdReader {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        if self.pos == self.buf.len() {
            self.fill()?;
        }
        let avail = self.buf.len() - self.pos;
        let n = avail.min(dst.len());
        dst[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Reader for client ingress. Unix connections retain their `SCM_RIGHTS`
/// receive queue; TCP connections are ordinary byte streams and therefore
/// cannot hold received file descriptors.
pub(crate) enum FdReader {
    Unix(UnixFdReader),
    Tcp(TcpStream),
}

impl FdReader {
    pub fn new(stream: Transport) -> Self {
        match stream {
            Transport::Unix(stream) => Self::Unix(UnixFdReader::new(stream)),
            Transport::Tcp(stream) => Self::Tcp(stream),
        }
    }

    /// Pop the next descriptor received on a Unix connection. TCP has no
    /// descriptor-passing mechanism and always returns `None`.
    pub fn pop_fd(&mut self) -> Option<RawFd> {
        match self {
            Self::Unix(reader) => reader.pop_fd(),
            Self::Tcp(_) => None,
        }
    }

    /// Raw fd backing this reader. Useful for `poll(2)` waits in the
    /// reader-thread WouldBlock retry loop.
    pub fn fd(&self) -> RawFd {
        match self {
            Self::Unix(reader) => reader.stream.as_raw_fd(),
            Self::Tcp(stream) => stream.as_raw_fd(),
        }
    }
}

impl io::Read for FdReader {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Unix(reader) => reader.read(dst),
            Self::Tcp(stream) => stream.read(dst),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FdReader, recv_with_fds, send_with_fd};
    use crate::transport::Transport;
    use std::{
        io::Write,
        net::{TcpListener, TcpStream},
        os::{fd::AsRawFd, unix::net::UnixStream},
    };

    fn tcp_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback TCP listener");
        let address = listener.local_addr().expect("loopback listener address");
        let client = TcpStream::connect(address).expect("connect loopback TCP client");
        let (server, _) = listener.accept().expect("accept loopback TCP client");
        (server, client)
    }

    #[test]
    fn round_trips_a_file_descriptor_through_a_unix_socket() {
        // Set up a connected pair of unix sockets.
        let (mut tx, mut rx) = UnixStream::pair().expect("socketpair");

        // Create a real file fd we can identify by content. /tmp/yserver-fd-test
        // is fine even on a read-only $HOME because /tmp is tmpfs.
        let path = format!("/tmp/yserver-fd-test-{}", std::process::id());
        {
            let mut f = std::fs::File::create(&path).expect("create temp");
            f.write_all(b"hello-from-fd").expect("write probe");
        }
        let f = std::fs::File::open(&path).expect("reopen for fd");
        let raw = f.as_raw_fd();

        send_with_fd(&mut tx, b"PAYLOAD", raw).expect("sendmsg");

        let mut buf = [0u8; 32];
        let (n, fds) = recv_with_fds(&mut rx, &mut buf).expect("recvmsg");
        assert_eq!(&buf[..n], b"PAYLOAD");
        assert_eq!(fds.len(), 1, "exactly one fd received");

        // Read through the *received* fd: contents must match what we wrote.
        use std::{io::Read, os::fd::FromRawFd};
        let mut received = unsafe { std::fs::File::from_raw_fd(fds[0]) };
        let mut got = String::new();
        received
            .read_to_string(&mut got)
            .expect("read via received fd");
        assert_eq!(got, "hello-from-fd");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_reply_with_a_descriptor_to_a_full_nonblocking_client_arrives_whole() {
        use std::io::Read;
        let (mut tx, mut rx) = UnixStream::pair().expect("socketpair");
        tx.set_nonblocking(true).expect("nonblocking");
        // Fill the client's socket with filler a byte short of an odd size,
        // so the reply cannot fit whole.
        let filler = [0x55_u8; 333];
        let mut queued = 0_usize;
        loop {
            match tx.write(&filler) {
                Ok(n) => queued += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("filling: {e}"),
            }
        }
        let f = std::fs::File::open("/dev/null").expect("open /dev/null");
        let reply: Vec<u8> = (0..32_u8).collect();
        let reader = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let mut sink = vec![0_u8; queued];
            rx.read_exact(&mut sink).expect("drain filler");
            let mut got = vec![0_u8; 32];
            let mut have = 0;
            let mut fds = Vec::new();
            while have < got.len() {
                let (n, more) = recv_with_fds(&mut rx, &mut got[have..]).expect("recvmsg");
                assert!(n > 0, "the reply ended early");
                have += n;
                fds.extend(more);
            }
            (got, fds)
        });
        send_with_fd(&mut tx, &reply, f.as_raw_fd()).expect("send the reply");
        let (got, fds) = reader.join().expect("reader");
        assert_eq!(got, reply, "the reply arrived torn");
        assert_eq!(fds.len(), 1, "the descriptor came with it");
    }

    #[test]
    fn returns_no_fds_when_sender_did_not_attach_one() {
        let (mut tx, mut rx) = UnixStream::pair().expect("socketpair");
        tx.write_all(b"plain bytes").expect("write");
        let mut buf = [0u8; 32];
        let (n, fds) = recv_with_fds(&mut rx, &mut buf).expect("recvmsg");
        assert_eq!(&buf[..n], b"plain bytes");
        assert!(fds.is_empty(), "no fds in cmsg");
    }

    #[test]
    fn fd_reader_yields_bytes_via_read_and_makes_fds_available() {
        use std::io::Read;

        let (mut tx, rx) = UnixStream::pair().expect("socketpair");

        let path = format!("/tmp/yserver-fd-reader-test-{}", std::process::id());
        {
            let mut f = std::fs::File::create(&path).expect("create");
            f.write_all(b"ok").expect("write");
        }
        let f = std::fs::File::open(&path).expect("reopen");
        send_with_fd(&mut tx, b"helloworld", f.as_raw_fd()).expect("send");
        // Send one more chunk without an fd, to verify the reader buffers
        // properly across multiple recvmsg calls.
        tx.write_all(b"trailing").expect("write");
        drop(tx);

        let mut reader = FdReader::new(Transport::Unix(rx));
        let mut buf = [0u8; 5];
        reader.read_exact(&mut buf).expect("first chunk");
        assert_eq!(&buf, b"hello");
        let mut buf = [0u8; 5];
        reader.read_exact(&mut buf).expect("rest of first message");
        assert_eq!(&buf, b"world");

        // The FD that came with the first message must be drainable now.
        let fd = reader.pop_fd().expect("fd from queue");
        assert!(reader.pop_fd().is_none(), "queue empty after one fd");

        // Second message should still be readable, no fd attached.
        let mut buf = [0u8; 8];
        reader.read_exact(&mut buf).expect("second chunk");
        assert_eq!(&buf, b"trailing");

        // Verify the FD points at the file we sent.
        use std::os::fd::FromRawFd;
        let mut received = unsafe { std::fs::File::from_raw_fd(fd) };
        let mut got = String::new();
        received.read_to_string(&mut got).expect("read fd");
        assert_eq!(got, "ok");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fd_reader_reads_tcp_bytes_without_file_descriptors() {
        use std::io::Read;

        let (server, mut client) = tcp_pair();
        client.write_all(b"tcp payload").expect("write TCP payload");

        let mut reader = FdReader::new(Transport::Tcp(server));
        assert!(
            matches!(reader, FdReader::Tcp(_)),
            "TCP has its own reader variant"
        );

        let mut bytes = [0; 11];
        reader.read_exact(&mut bytes).expect("read TCP payload");
        assert_eq!(&bytes, b"tcp payload");
        assert!(
            reader.pop_fd().is_none(),
            "TCP cannot receive SCM_RIGHTS fds"
        );
    }
}
