//! Connect a UI to QEMU's D-Bus display (`-display dbus,p2p=yes`).
//!
//! The UI creates a socketpair, hands one end to QEMU through QMP (`getfd`,
//! then `add_client protocol=@dbus-display`) and speaks peer-to-peer D-Bus
//! on the other end: no session bus is involved, so nothing else on the
//! desktop can see or drive the VM display.

use crate::qmp::{Qmp, QmpError};
use serde_json::json;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;

/// Name under which the fd is registered with QEMU; reused by every
/// connection (QEMU consumes it in `add_client`).
const FD_NAME: &str = "lsw-display";

/// Open a peer-to-peer D-Bus connection to QEMU's display.
pub async fn open_p2p(qmp: &mut Qmp) -> Result<UnixStream, QmpError> {
    let (ours, theirs) = UnixStream::pair()?;
    qmp.execute_with_fd("getfd", Some(json!({ "fdname": FD_NAME })), theirs.as_fd())
        .await?;
    qmp.execute(
        "add_client",
        Some(json!({ "protocol": "@dbus-display", "fdname": FD_NAME, "skipauth": true })),
    )
    .await?;
    drop(theirs);
    Ok(ours)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, recvmsg};
    use std::io::{Read, Write};
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixListener;

    /// A fake QEMU that receives `getfd` with SCM_RIGHTS, then `add_client`,
    /// and writes a greeting through the received socket.
    fn fake_qemu(listener: UnixListener) -> std::thread::JoinHandle<Vec<String>> {
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.write_all(b"{\"QMP\": {\"version\": {\"qemu\": {\"major\": 8, \"minor\": 2, \"micro\": 2}}, \"capabilities\": []}}\r\n").unwrap();
            let mut commands = Vec::new();
            let mut passed: Option<OwnedFd> = None;
            let mut pending = Vec::new();
            while commands.len() < 3 {
                let mut buf = [0u8; 4096];
                let mut space =
                    [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
                let mut control = RecvAncillaryBuffer::new(&mut space);
                let mut iov = [std::io::IoSliceMut::new(&mut buf)];
                let r = recvmsg(&s, &mut iov, &mut control, RecvFlags::empty()).unwrap();
                for msg in control.drain() {
                    if let RecvAncillaryMessage::ScmRights(fds) = msg {
                        passed = fds.into_iter().next();
                    }
                }
                pending.extend_from_slice(&buf[..r.bytes]);
                while let Some(i) = pending.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = pending.drain(..=i).collect();
                    let req: serde_json::Value = serde_json::from_slice(&line).unwrap();
                    commands.push(req["execute"].as_str().unwrap().to_owned());
                    let reply = json!({ "return": {}, "id": req["id"] });
                    s.write_all(format!("{reply}\r\n").as_bytes()).unwrap();
                }
            }
            let mut client = UnixStream::from(passed.expect("fd received"));
            client.write_all(b"hello").unwrap();
            commands
        })
    }

    #[tokio::test]
    async fn passes_the_socket_to_qemu() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("ui.sock");
        let qemu = fake_qemu(UnixListener::bind(&sock).unwrap());
        let mut qmp = Qmp::connect_with_retry(&sock, std::time::Duration::from_secs(2))
            .await
            .unwrap();
        let mut ours = open_p2p(&mut qmp).await.unwrap();
        let commands = qemu.join().unwrap();
        assert_eq!(commands, ["qmp_capabilities", "getfd", "add_client"]);
        let mut got = [0u8; 5];
        ours.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hello");
    }
}
