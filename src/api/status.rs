use std::io;
use std::path::Path;
use std::time::Duration;

use crate::api::schema::{Method, Request, ResponseResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeStatus {
    pub version: Option<String>,
    pub protocol: Option<u32>,
    pub capabilities: Option<crate::api::schema::ServerCapabilities>,
}

/// What the status probe learned about the server on a status socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeStatusProbe {
    /// The status API answered with a runtime status.
    Status(RuntimeStatus),
    /// Nothing listened, or the listener has no status API (an older server).
    NoApi,
    /// A listener accepted the connection but did not answer within the timeout: a
    /// slow, held or still-starting server - not evidence of an incompatible one.
    /// (A recv/send timeout expires as EAGAIN, `io::ErrorKind::WouldBlock`.)
    Unanswered,
}

pub fn read_runtime_status_at(
    socket_path: &Path,
    timeout: Duration,
) -> io::Result<RuntimeStatusProbe> {
    if !socket_path.exists() {
        return Ok(RuntimeStatusProbe::NoApi);
    }

    let client = crate::api::client::ApiClient::for_target(
        crate::api::client::ConnectionTarget::SocketPath(socket_path.to_path_buf()),
    );
    let request = Request {
        id: "runtime:status".into(),
        method: Method::Ping(crate::api::schema::PingParams::default()),
    };
    let response = client
        .request_value_with_timeout(&request, timeout)
        .and_then(crate::api::client::parse_response_value);
    let response = match response {
        Ok(response) => response,
        Err(crate::api::client::ApiClientError::Io(err))
            if matches!(
                err.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            return Ok(RuntimeStatusProbe::NoApi);
        }
        // A timeout (SO_RCVTIMEO expiry is EAGAIN, reported as WouldBlock) means the
        // server did not answer in time: slow, held or still starting. Ledger #54: when
        // this arrived as a fatal error the client aborted with a bare
        // `Resource temporarily unavailable (os error 35)` before it ever attached.
        Err(crate::api::client::ApiClientError::Io(err))
            if matches!(
                err.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) =>
        {
            return Ok(RuntimeStatusProbe::Unanswered);
        }
        Err(err) => return Err(io::Error::other(err)),
    };
    match response.result {
        ResponseResult::Pong {
            version,
            protocol,
            capabilities,
        } => Ok(RuntimeStatusProbe::Status(RuntimeStatus {
            version: Some(version),
            protocol: Some(protocol),
            capabilities,
        })),
        result => Err(io::Error::other(format!(
            "server status request returned unexpected result: {result:?}"
        ))),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::thread;

    struct TempDir(std::path::PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn unanswered_socket(name: &str) -> (TempDir, std::path::PathBuf, thread::JoinHandle<()>) {
        let dir = std::env::temp_dir().join(format!(
            "herdr-status-tests-{}-{}",
            std::process::id(),
            name
        ));
        let cleanup = TempDir(dir.clone());
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("api.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let handle = thread::spawn(move || {
            // Accept the probe's connection and never answer: a held, overloaded or
            // still-starting server looks exactly like this to the status probe.
            let accepted = listener.accept();
            thread::sleep(Duration::from_millis(800));
            drop(accepted);
        });
        (cleanup, path, handle)
    }

    #[test]
    fn an_unanswered_status_socket_is_never_a_fatal_error() {
        // Ledger #54: with the server slow to answer during startup the client exited
        // with `herdr: Resource temporarily unavailable (os error 35)` before it ever
        // attached. A probe that cannot classify a busy server must report "no timely
        // answer", never fail the client.
        let (_dir, path, handle) = unanswered_socket("unanswered");
        let probe = read_runtime_status_at(&path, Duration::from_millis(300));
        let _ = handle.join();
        assert!(
            probe.is_ok(),
            "an accepting-but-silent status socket must not be an error, got {probe:?}"
        );
    }

    #[test]
    fn a_missing_status_socket_is_not_an_error() {
        let dir =
            std::env::temp_dir().join(format!("herdr-status-tests-{}-missing", std::process::id()));
        let _cleanup = TempDir(dir.clone());
        let probe = read_runtime_status_at(&dir.join("absent.sock"), Duration::from_millis(300));
        assert!(
            probe.is_ok(),
            "no listener must not be an error, got {probe:?}"
        );
    }

    #[test]
    fn a_refused_status_socket_is_not_an_error() {
        let dir =
            std::env::temp_dir().join(format!("herdr-status-tests-{}-refused", std::process::id()));
        let _cleanup = TempDir(dir.clone());
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("api.sock");
        drop(UnixListener::bind(&path).unwrap());
        let probe = read_runtime_status_at(&path, Duration::from_millis(300));
        assert!(
            probe.is_ok(),
            "a stale socket (connection refused) must not be an error, got {probe:?}"
        );
    }

    #[test]
    fn an_answering_status_socket_reports_its_status() {
        // Guards the fix from its vacuous opposite: classifying every outcome as
        // "unanswered" would keep the slow-server tests green while silently dropping
        // the compatibility gate, which needs the real status.
        use std::io::{BufRead, BufReader, Write};
        let dir = std::env::temp_dir().join(format!(
            "herdr-status-tests-{}-answering",
            std::process::id()
        ));
        let _cleanup = TempDir(dir.clone());
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("api.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let pong = crate::api::schema::SuccessResponse {
                id: "runtime:status".into(),
                result: ResponseResult::Pong {
                    version: "test".into(),
                    protocol: crate::protocol::PROTOCOL_VERSION,
                    capabilities: None,
                },
            };
            let _ = stream.write_all(serde_json::to_string(&pong).unwrap().as_bytes());
            let _ = stream.write_all(b"\n");
        });
        let probe = read_runtime_status_at(&path, Duration::from_millis(2_000));
        let _ = handle.join();
        assert!(
            matches!(probe, Ok(RuntimeStatusProbe::Status(_))),
            "an answering status socket must report its status, got {probe:?}"
        );
    }
}
