//! Non-blocking USB serial console command reader for the shared
//! `CONFIG_ESP_CONSOLE_USB_SERIAL_JTAG` port.
//!
//! Commands and log output share the same USB serial port. To avoid colliding
//! with `log::info!`/`log::warn!` output, commands are framed with a sentinel
//! prefix `>>IW ` (note the trailing space). Each call to `poll_command()`
//! does a single non-blocking `read()` of whatever bytes are currently
//! available, splits them into `\n`-terminated lines, and parses every line
//! that starts with `>>IW ` into a `Command`. Since one read can contain more
//! than one complete command (e.g. two commands sent back-to-back land in the
//! same read), parsed commands are queued in `pending` and drained one per
//! `poll_command()` call — the whole read buffer is always fully consumed,
//! so no command is ever silently dropped because it shared a read with
//! another one.
//!
//! Replies are written back to stdout with the `<<IW ` prefix to distinguish
//! them as control output (not ordinary logs).
//!
//! This design avoids the complexity of raw fcntl/termios manipulation on the
//! USB-Serial-JTAG peripheral and leverages Rust's std threads, which work
//! reliably on this FreeRTOS+std platform.

use crate::control;
use std::collections::VecDeque;
use std::io::Read;

/// Prefix that marks an incoming line as a command, not a log message.
/// Must exactly match what the PC tool sends.
const COMMAND_PREFIX: &str = ">>IW ";

/// Prefix for outgoing replies, so the PC tool can distinguish control
/// responses from ordinary log output.
const REPLY_PREFIX: &str = "<<IW ";
const MAX_COMMAND_LINE_BYTES: usize = 512;

/// Reader state. USB Serial/JTAG stdin is non-blocking, so the main loop can
/// drain it directly without a second FreeRTOS task competing with Wi-Fi on
/// Core 0.
pub struct UsbConsole {
    line_buf: Vec<u8>,
    discarding_oversized_line: bool,
    /// Commands already parsed out of a previous read but not yet returned
    /// to the caller, paired with each one's optional client-supplied
    /// correlation `id` (see `control::parse_command`). A single 128-byte
    /// read can contain more than one complete `>>IW ...\n` line; every one
    /// of them is parsed here so none are lost, and `poll_command()` hands
    /// them out one at a time.
    pending: VecDeque<(Option<String>, control::Command)>,
}

impl UsbConsole {
    pub fn start() -> Self {
        Self {
            line_buf: Vec::new(),
            discarding_oversized_line: false,
            pending: VecDeque::new(),
        }
    }

    /// Non-blocking poll for the next command. Returns `Some((id, Command))`
    /// if a complete command line was received and parsed successfully
    /// (`id` is the client's correlation id, if it sent one), or `None` if
    /// there are no queued commands, or if a line was queued but failed to
    /// parse (in which case a warning is logged).
    pub fn poll_command(&mut self) -> Option<(Option<String>, control::Command)> {
        if let Some(cmd) = self.pending.pop_front() {
            return Some(cmd);
        }

        let mut chunk = [0u8; 128];
        match std::io::stdin().read(&mut chunk) {
            Ok(n) => {
                // Always consume the whole chunk - a single read can contain
                // more than one complete command line, and stopping early
                // would silently drop whatever came after the first one.
                for &byte in &chunk[..n] {
                    if byte == b'\n' {
                        if self.discarding_oversized_line {
                            self.discarding_oversized_line = false;
                            self.line_buf.clear();
                            continue;
                        }
                        let line = String::from_utf8_lossy(&self.line_buf)
                            .trim_end_matches('\r')
                            .to_string();
                        self.line_buf.clear();
                        if let Some(json) = line.strip_prefix(COMMAND_PREFIX) {
                            match control::parse_command(json) {
                                Ok(parsed) => self.pending.push_back(parsed),
                                Err(err) => {
                                    log::warn!("USB console: failed to parse command: {err}");
                                }
                            }
                        }
                    } else if self.discarding_oversized_line {
                        continue;
                    } else if self.line_buf.len() >= MAX_COMMAND_LINE_BYTES {
                        log::warn!(
                            "USB console: command line exceeds {MAX_COMMAND_LINE_BYTES} bytes; discarding"
                        );
                        self.line_buf.clear();
                        self.discarding_oversized_line = true;
                    } else {
                        self.line_buf.push(byte);
                    }
                }
                self.pending.pop_front()
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => None,
            Err(err) => {
                log::warn!("USB console reader I/O error: {err}");
                None
            }
        }
    }
}

/// Write a reply back to the PC tool, framed with the `<<IW ` prefix, echoing
/// back the triggering command's correlation `id` if it had one. This should
/// be called from the main loop after dispatching a command.
pub fn write_reply(reply: &control::Reply, id: Option<&str>) {
    let json = control::render_reply(reply, id);
    println!("{REPLY_PREFIX}{json}");
}
