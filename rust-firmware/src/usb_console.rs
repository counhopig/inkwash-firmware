//! Non-blocking USB serial console command reader for the shared
//! `CONFIG_ESP_CONSOLE_USB_SERIAL_JTAG` port.
//!
//! Commands and log output share the same USB serial port. To avoid colliding
//! with `log::info!`/`log::warn!` output, commands are framed with a sentinel
//! prefix `>>IW ` (note the trailing space). A dedicated receiver thread reads
//! stdin, splits it into `\n`-terminated lines, and parses every line that
//! starts with `>>IW ` into a `Command`. Parsed commands are sent to the main
//! loop and drained one per `poll_command()` call, so command reception never
//! waits behind Wi-Fi, EPD, or other main-loop work.
//!
//! Replies are written back to stdout with the `<<IW ` prefix to distinguish
//! them as control output (not ordinary logs).
//!
//! The receiver uses a standard Rust thread and channel, avoiding dependence
//! on platform-specific stdin readiness or fcntl/termios behavior.

use crate::control;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;

/// Prefix that marks an incoming line as a command, not a log message.
/// Must exactly match what the PC tool sends.
const COMMAND_PREFIX: &str = ">>IW ";

/// Prefix for outgoing replies, so the PC tool can distinguish control
/// responses from ordinary log output.
const REPLY_PREFIX: &str = "<<IW ";
const MAX_COMMAND_LINE_BYTES: usize = 512;

/// Reader state. The receiver thread owns stdin; the main loop only drains its
/// channel and never waits on ESP-IDF's VFS buffering.
pub struct UsbConsole {
    rx: Receiver<(Option<String>, control::Command)>,
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
        let (tx, rx) = mpsc::sync_channel(8);
        thread::Builder::new()
            .name("usb-console-rx".into())
            .spawn(move || read_commands(tx))
            .expect("USB console receiver thread must start");
        Self {
            pending: VecDeque::new(),
            rx,
        }
    }

    /// Non-blocking poll for the next command. Returns `Some((id, Command))`
    /// if a complete command line was received and parsed successfully
    /// (`id` is the client's correlation id, if it sent one), or `None` if
    /// there are no queued commands, or if a line was queued but failed to
    /// parse (in which case a warning is logged).
    pub fn poll_command(&mut self) -> Option<(Option<String>, control::Command)> {
        while let Ok(cmd) = self.rx.try_recv() {
            self.pending.push_back(cmd);
        }
        self.pending.pop_front()
    }
}

fn read_commands(tx: SyncSender<(Option<String>, control::Command)>) {
    let mut stdin = io::stdin();
    let mut byte = [0u8; 1];
    let mut line_buf = Vec::new();
    let mut discarding_oversized_line = false;

    loop {
        match stdin.read(&mut byte) {
            Ok(0) => thread::sleep(std::time::Duration::from_millis(10)),
            Ok(_) => {
                if byte[0] == b'\n' {
                    if discarding_oversized_line {
                        discarding_oversized_line = false;
                        line_buf.clear();
                        continue;
                    }
                    let line = String::from_utf8_lossy(&line_buf)
                        .trim_end_matches('\r')
                        .to_string();
                    line_buf.clear();
                    if let Some(json) = line.strip_prefix(COMMAND_PREFIX) {
                        match control::parse_command(json) {
                            Ok(parsed) => {
                                if tx.send(parsed).is_err() {
                                    return;
                                }
                            }
                            Err(err) => {
                                log::warn!("USB console: failed to parse command: {err}");
                            }
                        }
                    }
                } else if discarding_oversized_line {
                    continue;
                } else if line_buf.len() >= MAX_COMMAND_LINE_BYTES {
                    log::warn!(
                        "USB console: command line exceeds {MAX_COMMAND_LINE_BYTES} bytes; discarding"
                    );
                    line_buf.clear();
                    discarding_oversized_line = true;
                } else {
                    line_buf.push(byte[0]);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(err) => {
                log::warn!("USB console reader I/O error: {err}");
                thread::sleep(std::time::Duration::from_millis(100));
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
    let _ = io::stdout().flush();
}
