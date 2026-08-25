//! SCM_RIGHTS fd passing over Unix domain sockets.
//! The zero-copy spine of the same-machine data plane.
//! Currently works on Linux and MacOS, both support SCM_RIGHTS on SOCK_STREAM.

use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg};
use std::io::{IoSlice, IoSliceMut};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

/// Send a 1-byte marker carrying `fds` as ancillary data.
pub fn send_fds(sock: &UnixStream, fds: &[RawFd]) -> nix::Result<()> {
    let marker = [0xFDu8];
    let iov = [IoSlice::new(&marker)];
    let cmsg = [ControlMessage::ScmRights(fds)];
    sendmsg::<()>(sock.as_raw_fd(), &iov, &cmsg, MsgFlags::empty(), None)?;
    Ok(())
}

/// Receive the marker + up to `max` descriptors.
pub fn recv_fds(sock: &UnixStream, max: usize) -> nix::Result<Vec<OwnedFd>> {
    let mut marker = [0u8; 1];
    let mut iov = [IoSliceMut::new(&mut marker)];
    let mut cmsg_buf = nix::cmsg_space!([RawFd; 8]);
    let msg = recvmsg::<()>(
        sock.as_raw_fd(),
        &mut iov,
        Some(&mut cmsg_buf),
        MsgFlags::empty(),
    )?;
    let mut out = Vec::new();
    for c in msg.cmsgs()? {
        if let ControlMessageOwned::ScmRights(received) = c {
            for fd in received.into_iter().take(max) {
                // SAFETY: the kernel just handed us ownership of this fd.
                out.push(unsafe { OwnedFd::from_raw_fd(fd) });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::fd::AsFd;

    #[test]
    fn pass_a_file_descriptor_and_read_through_it() {
        let dir = std::env::temp_dir().join(format!("portos-fdpass-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("payload.txt");
        {
            let mut f = File::create(&path).unwrap();
            f.write_all(b"zero-copy says hi").unwrap();
        }
        let f = File::open(&path).unwrap();

        let (a, b) = UnixStream::pair().unwrap();
        send_fds(&a, &[f.as_fd().as_raw_fd()]).unwrap();
        let got = recv_fds(&b, 4).unwrap();
        assert_eq!(got.len(), 1);

        let mut through = File::from(got.into_iter().next().unwrap());
        through.seek(SeekFrom::Start(0)).unwrap();
        let mut s = String::new();
        through.read_to_string(&mut s).unwrap();
        assert_eq!(s, "zero-copy says hi");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
