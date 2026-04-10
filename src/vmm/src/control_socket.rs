// Control socket for runtime VM management.
//
// Provides a Unix domain socket that accepts JSON commands for
// balloon control and other runtime operations.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use devices::virtio::Balloon;
use polly::event_manager::{EventManager, Subscriber};
use utils::epoll::{EpollEvent, EventSet};

/// Maximum commands per second before rate limiting kicks in.
const RATE_LIMIT_MAX: usize = 10;

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
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl ControlSocket {
    /// Create a new control socket bound to `socket_path`.
    ///
    /// Removes stale sockets, binds, sets permissions to 0700, and
    /// sets the listener to non-blocking mode.
    pub fn new(
        socket_path: &Path,
        balloon: Option<Arc<Mutex<Balloon>>>,
    ) -> std::io::Result<Self> {
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
        unsafe {
            let c_path = std::ffi::CString::new(socket_path.to_str().unwrap()).unwrap();
            libc::chmod(c_path.as_ptr(), 0o700);
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
                    let old_fd = old.reader.get_ref().as_raw_fd();
                    let _ = event_manager.unregister(old_fd);
                }

                if stream.set_nonblocking(true).is_err() {
                    error!("control_socket: failed to set client non-blocking");
                    return;
                }

                let client_fd = stream.as_raw_fd();
                let writer = match stream.try_clone() {
                    Ok(w) => w,
                    Err(e) => {
                        error!("control_socket: failed to clone stream: {e}");
                        return;
                    }
                };

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
                    reader: BufReader::new(stream),
                    writer,
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                error!("control_socket: accept error: {e}");
            }
        }
    }

    /// Handle data from the connected client.
    fn handle_client_data(&mut self, event: &EpollEvent, event_manager: &mut EventManager) {
        let event_set = event.event_set();

        // Client disconnected or errored.
        if event_set.contains(EventSet::HANG_UP) || event_set.contains(EventSet::READ_HANG_UP) {
            self.disconnect_client(event_manager);
            return;
        }

        let client = match self.client.as_mut() {
            Some(c) => c,
            None => return,
        };

        let mut line = String::new();
        match client.reader.read_line(&mut line) {
            Ok(0) => {
                // EOF — client disconnected.
                self.disconnect_client(event_manager);
                return;
            }
            Ok(_) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
            Err(e) => {
                error!("control_socket: read error: {e}");
                self.disconnect_client(event_manager);
                return;
            }
        }

        // Rate limiting.
        let now = Instant::now();
        if now.duration_since(self.rate_window_start).as_secs() >= 1 {
            self.rate_window_start = now;
            self.rate_count = 0;
        }
        self.rate_count += 1;
        if self.rate_count > RATE_LIMIT_MAX {
            let resp = r#"{"ok":false,"error":"rate limit exceeded"}"#;
            self.send_response(resp);
            return;
        }

        let response = self.dispatch_command(line.trim());
        self.send_response(&response);
    }

    fn send_response(&mut self, response: &str) {
        if let Some(ref mut client) = self.client {
            let _ = writeln!(client.writer, "{response}");
            let _ = client.writer.flush();
        }
    }

    fn disconnect_client(&mut self, event_manager: &mut EventManager) {
        if let Some(client) = self.client.take() {
            let fd = client.reader.get_ref().as_raw_fd();
            let _ = event_manager.unregister(fd);
        }
    }

    /// Parse and dispatch a JSON command line.
    fn dispatch_command(&self, line: &str) -> String {
        // Minimal JSON parsing for our simple protocol.
        let cmd = match extract_string_field(line, "cmd") {
            Some(c) => c,
            None => return r#"{"ok":false,"error":"missing or invalid 'cmd' field"}"#.to_string(),
        };

        match cmd.as_str() {
            "balloon_set" => self.cmd_balloon_set(line),
            "balloon_stats" => self.cmd_balloon_stats(),
            _ => format!(r#"{{"ok":false,"error":"unknown command: {cmd}"}}"#),
        }
    }

    fn cmd_balloon_set(&self, line: &str) -> String {
        let balloon = match self.balloon.as_ref() {
            Some(b) => b,
            None => return r#"{"ok":false,"error":"balloon device not configured"}"#.to_string(),
        };

        let target_mib = match extract_u32_field(line, "target_mib") {
            Some(v) => v,
            None => {
                return r#"{"ok":false,"error":"missing or invalid 'target_mib' field"}"#
                    .to_string()
            }
        };

        // Convert MiB to 4KB pages (checked to prevent overflow).
        let pages = match target_mib.checked_mul(256) {
            Some(p) => p,
            None => {
                return r#"{"ok":false,"error":"target_mib too large"}"#.to_string()
            }
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

        format!(
            r#"{{"ok":true,"actual_mib":{actual_mib},"target_mib":{target_mib},"free_mib":0}}"#
        )
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
            .is_some_and(|c| c.reader.get_ref().as_raw_fd() == source)
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

// --- Minimal JSON field extraction (no serde dependency) ---

/// Extract a string value for the given key from a JSON object string.
/// Handles: {"key": "value", ...}
fn extract_string_field(json: &str, key: &str) -> Option<String> {
    let pattern = format!(r#""{}""#, key);
    let key_pos = json.find(&pattern)?;
    let after_key = &json[key_pos + pattern.len()..];

    // Skip whitespace and colon.
    let after_colon = after_key.trim_start().strip_prefix(':')?;
    let trimmed = after_colon.trim_start();

    if trimmed.starts_with('"') {
        let content = &trimmed[1..];
        let end = content.find('"')?;
        Some(content[..end].to_string())
    } else {
        None
    }
}

/// Extract a u32 value for the given key from a JSON object string.
/// Handles: {"key": 123, ...}
fn extract_u32_field(json: &str, key: &str) -> Option<u32> {
    let pattern = format!(r#""{}""#, key);
    let key_pos = json.find(&pattern)?;
    let after_key = &json[key_pos + pattern.len()..];

    let after_colon = after_key.trim_start().strip_prefix(':')?;
    let trimmed = after_colon.trim_start();

    // Parse digits until non-digit.
    let end = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    if end == 0 {
        return None;
    }
    trimmed[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_string_field() {
        let json = r#"{"cmd": "balloon_set", "target_mib": 128}"#;
        assert_eq!(
            extract_string_field(json, "cmd"),
            Some("balloon_set".to_string())
        );
        assert_eq!(extract_string_field(json, "target_mib"), None);
        assert_eq!(extract_string_field(json, "missing"), None);
    }

    #[test]
    fn test_extract_u32_field() {
        let json = r#"{"cmd": "balloon_set", "target_mib": 128}"#;
        assert_eq!(extract_u32_field(json, "target_mib"), Some(128));
        assert_eq!(extract_u32_field(json, "cmd"), None);
        assert_eq!(extract_u32_field(json, "missing"), None);
    }

    #[test]
    fn test_extract_u32_zero() {
        let json = r#"{"target_mib": 0}"#;
        assert_eq!(extract_u32_field(json, "target_mib"), Some(0));
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
        let resp =
            cs.dispatch_command(r#"{"cmd": "balloon_set", "target_mib": 4294967295}"#);
        assert!(resp.contains("target_mib too large"));
        // Balloon target should be unchanged (0).
        assert_eq!(balloon.lock().unwrap().num_pages(), 0);
        drop(cs);
    }
}
