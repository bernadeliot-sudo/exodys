//! The desktop holds a lease on an administrator-provisioned TUN service.
#[cfg(not(target_os = "macos"))]
use crate::Result;

#[cfg(target_os = "macos")]
mod macos {
    use std::io::{ErrorKind, Read, Write};
    use std::net::Shutdown;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    use socket2::{Domain, SockAddr, Socket, Type};

    use crate::Result;
    use crate::tun_config::{REQUEST_PREFIX, UNAPPROVED};

    const SOCKET: &str = "/var/run/exodys-tun.sock";
    const CONNECT_WAIT: Duration = Duration::from_secs(2);
    const START_WAIT: Duration = Duration::from_secs(70);
    const STOP_WAIT: Duration = Duration::from_secs(8);

    pub(crate) struct Lease(UnixStream);

    impl Lease {
        pub(crate) fn acquire(id: &str) -> Result<Self> {
            let stream = (|| {
                let server = SockAddr::unix(SOCKET)?;
                let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
                socket.connect_timeout(&server, CONNECT_WAIT)?;
                std::io::Result::Ok(UnixStream::from(socket))
            })()
            .map_err(|_| "无法连接 TUN 服务。请确认管理员已安装服务并授权当前账户。")?;
            Self::handshake(stream, 0, id)
        }

        fn handshake(mut stream: UnixStream, expected_uid: libc::uid_t, id: &str) -> Result<Self> {
            let mut uid = 0;
            let mut gid = 0;
            // Darwin supplies the effective IDs of the peer over the local socket.
            let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
            if result != 0 || uid != expected_uid {
                return Err("无法验证本机 TUN 服务的身份。".into());
            }
            let request = [REQUEST_PREFIX, id.as_bytes(), b"\n"].concat();
            stream
                .set_write_timeout(Some(START_WAIT))
                .and_then(|_| stream.set_read_timeout(Some(START_WAIT)))
                .and_then(|_| stream.write_all(&request))
                .map_err(|_| "无法请求启动 TUN。")?;
            let mut reply = [0; 6];
            stream
                .read_exact(&mut reply)
                .map_err(|_| "TUN 服务未能就绪。请检查服务授权和引擎配置。")?;
            if &reply == UNAPPROVED {
                return Err(format!(
                    "当前 TUN 配置尚未批准或完整性检查失败。请管理员核对配置编号 {id}。"
                ));
            }
            if &reply != b"READY\n" {
                return Err("TUN 服务启动失败。请管理员检查已安装的引擎和配置。".into());
            }
            stream
                .set_nonblocking(true)
                .map_err(|_| "无法监视 TUN 服务。")?;
            Ok(Self(stream))
        }

        pub(crate) fn alive(&mut self) -> bool {
            matches!(self.0.read(&mut [0]), Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted))
        }

        pub(crate) fn release(mut self) -> Result<()> {
            // A write-side EOF asks the supervisor to stop the engine.
            self.0
                .set_nonblocking(false)
                .and_then(|_| self.0.set_read_timeout(Some(STOP_WAIT)))
                .and_then(|_| self.0.shutdown(Shutdown::Write))
                .map_err(|_| "无法确认 TUN 服务已停止。")?;
            let mut reply = [0; 8];
            if self.0.read_exact(&mut reply).is_ok() && &reply == b"STOPPED\n" {
                Ok(())
            } else {
                Err("未收到 TUN 服务的停止确认。请检查本机 TUN 服务。".into())
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::tun_config::{REQUEST_LENGTH, requested_id};

        const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

        #[test]
        fn lease_waits_for_ready_and_release_waits_for_peer_exit() {
            let (client, mut service) = UnixStream::pair().unwrap();
            let uid = unsafe { libc::geteuid() };
            service
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            std::thread::scope(|scope| {
                let worker = scope.spawn(move || {
                    let mut request = [0; REQUEST_LENGTH];
                    service.read_exact(&mut request).unwrap();
                    assert_eq!(requested_id(&request), Some(ID));
                    service.write_all(b"READY\n").unwrap();
                    assert_eq!(service.read(&mut [0]).unwrap(), 0);
                    service.write_all(b"STOPPED\n").unwrap();
                });
                let mut lease = Lease::handshake(client, uid, ID).unwrap();
                assert!(lease.alive());
                lease.release().unwrap();
                worker.join().unwrap();
            });
        }

        #[test]
        fn rejects_unexpected_peer_before_requesting_start() {
            let (client, mut service) = UnixStream::pair().unwrap();
            let uid = unsafe { libc::geteuid() };
            assert!(Lease::handshake(client, uid ^ 1, ID).is_err());
            assert_eq!(service.read(&mut [0]).unwrap(), 0);
        }

        #[test]
        fn missing_approval_identifies_the_requested_version() {
            let (client, mut service) = UnixStream::pair().unwrap();
            service.write_all(UNAPPROVED).unwrap();
            let uid = unsafe { libc::geteuid() };
            let Err(error) = Lease::handshake(client, uid, ID) else {
                panic!("unapproved configuration accepted");
            };
            assert!(error.contains("尚未批准"));
            assert!(error.contains(ID));
        }

        #[test]
        fn startup_error_and_unexpected_exit_do_not_keep_a_lease() {
            let uid = unsafe { libc::geteuid() };
            for (reply, accepted) in [(b"ERROR\n", false), (b"READY\n", true)] {
                let (client, mut service) = UnixStream::pair().unwrap();
                service.write_all(reply).unwrap();
                let lease = Lease::handshake(client, uid, ID);
                assert_eq!(lease.is_ok(), accepted);
                if let Ok(mut lease) = lease {
                    service.shutdown(Shutdown::Write).unwrap();
                    assert!(!lease.alive());
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) use macos::Lease;

#[cfg(not(target_os = "macos"))]
pub(crate) struct Lease;

#[cfg(not(target_os = "macos"))]
impl Lease {
    pub(crate) fn acquire(_: &str) -> Result<Self> {
        Err("TUN 服务仅支持 macOS。".into())
    }

    pub(crate) fn alive(&mut self) -> bool {
        false
    }

    pub(crate) fn release(self) -> Result<()> {
        Ok(())
    }
}
