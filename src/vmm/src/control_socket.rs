// Control socket for runtime VM management.
//
// Provides a Unix domain socket that accepts JSON commands for
// balloon control and other runtime operations.

use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use devices::virtio::Balloon;
use polly::event_manager::{EventManager, Subscriber};
use serde_json::{json, Value};
use utils::epoll::{EpollEvent, EventSet};

/// Maximum commands per second before rate limiting kicks in.
const RATE_LIMIT_MAX: usize = 10;
const MAX_COMMAND_BYTES: usize = 4096;

pub struct ControlSocket {
    listener: UnixListener,
    socket_path: PathBuf,
    client: Option<ClientConn>,
    balloon: Option<Arc<Mutex<Balloon>>>,
    /// Rate limiting: timestamps of recent commands within the current second.
    rate_window_start: Instant,
    rate_count: usize,
}

struct ClientConn {
    stream: UnixStream,
    recv_buf: Vec<u8>,
}

impl ControlSocket {
    /// Create a new control socket bound to `socket_path`.
    ///
    /// Removes stale sockets, binds, sets permissions to 0700, and
    /// sets the listener to non-blocking mode.
    pub fn new(socket_path: &Path, balloon: Option<Arc<Mutex<Balloon>>>) -> std::io::Result<Self> {
        // Remove stale socket if it exists. Only remove actual socket files —
        // refuse to delete regular files or symlinks to prevent misconfiguration damage.
        if socket_path.exists() {
            use std::os::unix::fs::FileTypeExt;
            let metadata = std::fs::symlink_metadata(socket_path)?;
            if !metadata.file_type().is_socket() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!(
                        "control socket path {} exists and is not a socket",
                        socket_path.display()
                    ),
                ));
            }
            warn!(
                "control_socket: removing stale socket at {}",
                socket_path.display()
            );
            std::fs::remove_file(socket_path)?;
        }

        // Ensure parent directory exists.
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let listener = UnixListener::bind(socket_path)?;
        listener.set_nonblocking(true)?;

        // chmod 0700 on the socket file.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            if let Ok(c_path) = std::ffi::CString::new(socket_path.as_os_str().as_bytes()) {
                let ret = unsafe { libc::chmod(c_path.as_ptr(), 0o700) };
                if ret != 0 {
                    warn!(
                        "control_socket: chmod failed on {}: {}",
                        socket_path.display(),
                        std::io::Error::last_os_error()
                    );
                }
            }
        }

        Ok(ControlSocket {
            listener,
            socket_path: socket_path.to_path_buf(),
            client: None,
            balloon,
            rate_window_start: Instant::now(),
            rate_count: 0,
        })
    }

    /// Handle a new connection on the listener socket.
    fn handle_accept(&mut self, event_manager: &mut EventManager) {
        match self.listener.accept() {
            Ok((stream, _addr)) => {
                // Drop existing client if any (single client at a time).
                if let Some(old) = self.client.take() {
                    let old_fd = old.stream.as_raw_fd();
                    let _ = event_manager.unregister(old_fd);
                }

                if stream.set_nonblocking(true).is_err() {
                    error!("control_socket: failed to set client non-blocking");
                    return;
                }

                let client_fd = stream.as_raw_fd();
                // Register client FD for reading.
                let self_sub = match event_manager.subscriber(self.listener.as_raw_fd()) {
                    Ok(s) => s,
                    Err(e) => {
                        error!("control_socket: failed to get self subscriber: {e:?}");
                        return;
                    }
                };

                if let Err(e) = event_manager.register(
                    client_fd,
                    EpollEvent::new(EventSet::IN, client_fd as u64),
                    self_sub,
                ) {
                    error!("control_socket: failed to register client fd: {e:?}");
                    return;
                }

                self.client = Some(ClientConn {
                    stream,
                    recv_buf: Vec::new(),
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                error!("control_socket: accept error: {e}");
            }
        }
    }

    /// Handle data from the connected client.
    ///
    /// Retains fragmented commands across reads and drains data before
    /// disconnecting at EOF, including when a hangup accompanies readable data.
    fn handle_client_data(&mut self, _event: &EpollEvent, event_manager: &mut EventManager) {
        let mut lines = Vec::new();
        let mut should_disconnect = false;
        let mut command_too_long = false;

        if let Some(client) = self.client.as_mut() {
            let mut buffer = [0; MAX_COMMAND_BYTES];
            'drain: loop {
                match client.stream.read(&mut buffer) {
                    Ok(0) => {
                        // EOF also terminates a final command without a newline.
                        if !client.recv_buf.is_empty() {
                            lines.push(std::mem::take(&mut client.recv_buf));
                        }
                        should_disconnect = true;
                        break;
                    }
                    Ok(len) => {
                        for &byte in &buffer[..len] {
                            if byte == b'\n' {
                                lines.push(std::mem::take(&mut client.recv_buf));
                                // Bound work and memory even if a client floods commands.
                                if lines.len() > RATE_LIMIT_MAX {
                                    should_disconnect = true;
                                    break 'drain;
                                }
                            } else if client.recv_buf.len() >= MAX_COMMAND_BYTES {
                                command_too_long = true;
                                should_disconnect = true;
                                break 'drain;
                            } else {
                                client.recv_buf.push(byte);
                            }
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        error!("control_socket: read error: {e}");
                        should_disconnect = true;
                        break;
                    }
                }
            }
        } else {
            return;
        }

        // Process collected lines.
        for line in &lines {
            let now = Instant::now();
            if now.duration_since(self.rate_window_start).as_secs() >= 1 {
                self.rate_window_start = now;
                self.rate_count = 0;
            }
            self.rate_count += 1;
            if self.rate_count > RATE_LIMIT_MAX {
                self.send_response(r#"{"ok":false,"error":"rate limit exceeded"}"#);
                should_disconnect = true;
                break;
            }

            let response = match std::str::from_utf8(line) {
                Ok(line) => self.dispatch_command(line.trim()),
                Err(_) => r#"{"ok":false,"error":"invalid JSON"}"#.to_string(),
            };
            self.send_response(&response);
        }

        if command_too_long {
            self.send_response(r#"{"ok":false,"error":"command too long"}"#);
        }
        if should_disconnect {
            self.disconnect_client(event_manager);
        }
    }

    fn send_response(&mut self, response: &str) {
        if let Some(ref mut client) = self.client {
            let _ = writeln!(client.stream, "{response}");
            let _ = client.stream.flush();
        }
    }

    fn disconnect_client(&mut self, event_manager: &mut EventManager) {
        if let Some(client) = self.client.take() {
            let fd = client.stream.as_raw_fd();
            let _ = event_manager.unregister(fd);
        }
    }

    /// Parse and dispatch a JSON command line.
    fn dispatch_command(&self, line: &str) -> String {
        let command: Value = match serde_json::from_str(line) {
            Ok(command) => command,
            Err(_) => return r#"{"ok":false,"error":"invalid JSON"}"#.to_string(),
        };
        let cmd = match command.get("cmd").and_then(Value::as_str) {
            Some(c) => c,
            None => return r#"{"ok":false,"error":"missing or invalid 'cmd' field"}"#.to_string(),
        };

        match cmd {
            "balloon_set" => self.cmd_balloon_set(&command),
            "balloon_stats" => self.cmd_balloon_stats(),
            _ => r#"{"ok":false,"error":"unknown command"}"#.to_string(),
        }
    }

    fn cmd_balloon_set(&self, command: &Value) -> String {
        let balloon = match self.balloon.as_ref() {
            Some(b) => b,
            None => return r#"{"ok":false,"error":"balloon device not configured"}"#.to_string(),
        };

        let target_mib = match command
            .get("target_mib")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
        {
            Some(v) => v,
            None => {
                return r#"{"ok":false,"error":"missing or invalid 'target_mib' field"}"#
                    .to_string()
            }
        };

        // Convert MiB to 4KB pages (checked to prevent overflow).
        let pages = match target_mib.checked_mul(256) {
            Some(p) => p,
            None => return r#"{"ok":false,"error":"target_mib too large"}"#.to_string(),
        };
        balloon.lock().unwrap().set_num_pages(pages);

        r#"{"ok":true}"#.to_string()
    }

    fn cmd_balloon_stats(&self) -> String {
        let balloon = match self.balloon.as_ref() {
            Some(b) => b,
            None => return r#"{"ok":false,"error":"balloon device not configured"}"#.to_string(),
        };

        let b = balloon.lock().unwrap();
        let actual_pages = b.actual();
        let target_pages = b.num_pages();

        // Convert pages to MiB (pages * 4KB / 1024KB).
        let actual_mib = actual_pages / 256;
        let target_mib = target_pages / 256;

        json!({"ok": true, "actual_mib": actual_mib, "target_mib": target_mib, "free_mib": 0})
            .to_string()
    }
}

impl Subscriber for ControlSocket {
    fn process(&mut self, event: &EpollEvent, event_manager: &mut EventManager) {
        let source = event.fd();

        if source == self.listener.as_raw_fd() {
            self.handle_accept(event_manager);
        } else if self
            .client
            .as_ref()
            .is_some_and(|c| c.stream.as_raw_fd() == source)
        {
            self.handle_client_data(event, event_manager);
        } else {
            warn!("control_socket: unexpected event source fd={source}");
        }
    }

    fn interest_list(&self) -> Vec<EpollEvent> {
        vec![EpollEvent::new(
            EventSet::IN,
            self.listener.as_raw_fd() as u64,
        )]
    }
}

impl Drop for ControlSocket {
    fn drop(&mut self) {
        if self.socket_path.exists() {
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::net::Shutdown;

    #[test]
    fn test_dispatch_rejects_invalid_numbers_and_json() {
        let path = std::env::temp_dir().join("test_ctrl_json.sock");
        let balloon = Arc::new(Mutex::new(Balloon::new().unwrap()));
        let cs = ControlSocket::new(&path, Some(balloon.clone())).unwrap();
        for value in [
            "123.5",
            "1e2",
            "1E10",
            "-1",
            "4294967296",
            "12garbage",
            "01",
            "true",
            "\"64\"",
        ] {
            let request = format!(r#"{{"cmd":"balloon_set","target_mib":{value}}}"#);
            let response: Value = serde_json::from_str(&cs.dispatch_command(&request)).unwrap();
            assert_eq!(response["ok"], false, "accepted {value}");
            assert_eq!(balloon.lock().unwrap().num_pages(), 0);
        }
        let response: Value = serde_json::from_str(
            &cs.dispatch_command(r#"{"cmd":"balloon_\u0073et","target_mib":0}"#),
        )
        .unwrap();
        assert_eq!(response["ok"], true);
    }

    fn socket_with_client(name: &str) -> (ControlSocket, UnixStream, EventManager, EpollEvent) {
        let path = std::env::temp_dir().join(format!("test_ctrl_{name}.sock"));
        let mut cs = ControlSocket::new(&path, None).unwrap();
        let (stream, peer) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let event = EpollEvent::new(EventSet::IN, stream.as_raw_fd() as u64);
        cs.client = Some(ClientConn {
            stream,
            recv_buf: Vec::new(),
        });
        (cs, peer, EventManager::new().unwrap(), event)
    }

    #[test]
    fn test_fragmented_command_survives_would_block() {
        let (mut cs, mut peer, mut manager, event) = socket_with_client("fragmented");
        peer.write_all(br#"{"cmd":"balloon_"#).unwrap();
        cs.handle_client_data(&event, &mut manager);
        assert_eq!(cs.rate_count, 0);
        assert!(!cs.client.as_ref().unwrap().recv_buf.is_empty());

        peer.write_all(b"stats\"}\n").unwrap();
        cs.handle_client_data(&event, &mut manager);
        let mut response = String::new();
        std::io::BufReader::new(peer)
            .read_line(&mut response)
            .unwrap();
        assert!(response.contains("balloon device not configured"));
        assert_eq!(cs.rate_count, 1);
        assert!(cs.client.as_ref().unwrap().recv_buf.is_empty());
    }

    #[test]
    fn test_multiple_commands_are_drained_before_eof() {
        let (mut cs, mut peer, mut manager, event) = socket_with_client("drain");
        peer.write_all(b"{\"cmd\":\"balloon_stats\"}\n{\"cmd\":\"reboot\"}\n")
            .unwrap();
        peer.shutdown(Shutdown::Write).unwrap();
        let event = EpollEvent::new(EventSet::IN | EventSet::READ_HANG_UP, event.data());
        cs.handle_client_data(&event, &mut manager);

        let responses: Vec<String> = std::io::BufReader::new(peer)
            .lines()
            .map(Result::unwrap)
            .collect();
        assert_eq!(responses.len(), 2);
        assert!(responses[0].contains("balloon device not configured"));
        assert!(responses[1].contains("unknown command"));
        assert!(cs.client.is_none());
    }

    #[test]
    fn test_overlong_command_disconnects_client() {
        let (mut cs, mut peer, mut manager, event) = socket_with_client("overlong");
        peer.write_all(&vec![b'x'; MAX_COMMAND_BYTES + 1]).unwrap();
        cs.handle_client_data(&event, &mut manager);
        let mut response = String::new();
        std::io::BufReader::new(peer)
            .read_line(&mut response)
            .unwrap();
        assert!(response.contains("command too long"));
        assert!(cs.client.is_none());
    }

    #[test]
    fn test_command_flood_is_rate_limited_and_disconnected() {
        let (mut cs, mut peer, mut manager, event) = socket_with_client("flood");
        peer.write_all(
            b"{\"cmd\":\"balloon_stats\"}\n"
                .repeat(RATE_LIMIT_MAX + 1)
                .as_slice(),
        )
        .unwrap();
        cs.handle_client_data(&event, &mut manager);
        let responses: Vec<String> = std::io::BufReader::new(peer)
            .lines()
            .map(Result::unwrap)
            .collect();
        assert_eq!(responses.len(), RATE_LIMIT_MAX + 1);
        assert!(responses.last().unwrap().contains("rate limit exceeded"));
        assert!(cs.client.is_none());
    }

    #[test]
    fn test_dispatch_unknown_command_no_echo() {
        let socket_path = std::env::temp_dir().join("test_ctrl_no_echo.sock");
        let _ = std::fs::remove_file(&socket_path);
        let cs = ControlSocket::new(&socket_path, None).unwrap();
        let resp = cs.dispatch_command(r#"{"cmd": "evil\"inject"}"#);
        assert!(resp.contains("unknown command"));
        // Must NOT echo the command back (prevents JSON injection).
        assert!(!resp.contains("evil"));
        drop(cs);
    }

    #[test]
    fn test_dispatch_unknown_command() {
        let socket_path = std::env::temp_dir().join("test_ctrl_unknown.sock");
        let _ = std::fs::remove_file(&socket_path);
        let cs = ControlSocket::new(&socket_path, None).unwrap();
        let resp = cs.dispatch_command(r#"{"cmd": "reboot"}"#);
        assert!(resp.contains("unknown command"));
        drop(cs);
    }

    #[test]
    fn test_dispatch_balloon_stats_no_device() {
        let socket_path = std::env::temp_dir().join("test_ctrl_stats.sock");
        let _ = std::fs::remove_file(&socket_path);
        let cs = ControlSocket::new(&socket_path, None).unwrap();
        let resp = cs.dispatch_command(r#"{"cmd": "balloon_stats"}"#);
        assert!(resp.contains("balloon device not configured"));
        drop(cs);
    }

    #[test]
    fn test_dispatch_balloon_set_no_device() {
        let socket_path = std::env::temp_dir().join("test_ctrl_set.sock");
        let _ = std::fs::remove_file(&socket_path);
        let cs = ControlSocket::new(&socket_path, None).unwrap();
        let resp = cs.dispatch_command(r#"{"cmd": "balloon_set", "target_mib": 64}"#);
        assert!(resp.contains("balloon device not configured"));
        drop(cs);
    }

    #[test]
    fn test_dispatch_balloon_with_device() {
        let socket_path = std::env::temp_dir().join("test_ctrl_with_dev.sock");
        let _ = std::fs::remove_file(&socket_path);
        let balloon = Arc::new(Mutex::new(Balloon::new().unwrap()));
        let cs = ControlSocket::new(&socket_path, Some(balloon.clone())).unwrap();

        // Set target to 64 MiB (= 64*256 = 16384 pages).
        let resp = cs.dispatch_command(r#"{"cmd": "balloon_set", "target_mib": 64}"#);
        assert!(resp.contains(r#""ok":true"#));
        assert_eq!(balloon.lock().unwrap().num_pages(), 64 * 256);

        // Get stats.
        let resp = cs.dispatch_command(r#"{"cmd": "balloon_stats"}"#);
        assert!(resp.contains(r#""target_mib":64"#));
        assert!(resp.contains(r#""actual_mib":0"#));

        drop(cs);
    }

    #[test]
    fn test_socket_cleanup_on_drop() {
        let socket_path = std::env::temp_dir().join("test_ctrl_drop.sock");
        let _ = std::fs::remove_file(&socket_path);
        {
            let _cs = ControlSocket::new(&socket_path, None).unwrap();
            assert!(socket_path.exists());
        }
        assert!(!socket_path.exists());
    }

    #[test]
    fn test_missing_cmd_field() {
        let socket_path = std::env::temp_dir().join("test_ctrl_nocmd.sock");
        let _ = std::fs::remove_file(&socket_path);
        let cs = ControlSocket::new(&socket_path, None).unwrap();
        let resp = cs.dispatch_command(r#"{"target_mib": 64}"#);
        assert!(resp.contains("missing or invalid"));
        drop(cs);
    }

    #[test]
    fn test_new_rejects_non_socket_path() {
        let path = std::env::temp_dir().join("test_ctrl_not_a_socket.txt");
        // Create a regular file at the path.
        std::fs::write(&path, "not a socket").unwrap();
        let result = ControlSocket::new(&path, None);
        assert!(result.is_err());
        // The regular file should NOT have been deleted.
        assert!(path.exists());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn test_balloon_set_overflow() {
        let socket_path = std::env::temp_dir().join("test_ctrl_overflow.sock");
        let _ = std::fs::remove_file(&socket_path);
        let balloon = Arc::new(Mutex::new(Balloon::new().unwrap()));
        let cs = ControlSocket::new(&socket_path, Some(balloon.clone())).unwrap();

        // u32::MAX * 256 would overflow. Should return error, not wrap.
        let resp = cs.dispatch_command(r#"{"cmd": "balloon_set", "target_mib": 4294967295}"#);
        assert!(resp.contains("target_mib too large"));
        // Balloon target should be unchanged (0).
        assert_eq!(balloon.lock().unwrap().num_pages(), 0);
        drop(cs);
    }
}
