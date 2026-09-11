use crate::control;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread;

const COMMAND_PREFIX: &str = ">>IW ";

const REPLY_PREFIX: &str = "<<IW ";
const MAX_COMMAND_LINE_BYTES: usize = 512;

const READER_TASK_STACK_SIZE: usize = 12 * 1024;
const WRITER_TASK_STACK_SIZE: usize = 8 * 1024;

const PENDING_COMMAND_CAPACITY: usize = 8;

pub const REPLY_WRITER_CAPACITY: usize = 8;

pub struct UsbConsole {
    rx: Receiver<(Option<String>, control::Command)>,

    pending: VecDeque<(Option<String>, control::Command)>,
}

impl UsbConsole {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::sync_channel(PENDING_COMMAND_CAPACITY);
        thread::Builder::new()
            .name("usb-console-rx".into())
            .stack_size(READER_TASK_STACK_SIZE)
            .spawn(move || read_commands(tx))
            .expect("USB console receiver thread must start");
        Self {
            pending: VecDeque::with_capacity(PENDING_COMMAND_CAPACITY),
            rx,
        }
    }

    pub fn poll_command(&mut self) -> Option<(Option<String>, control::Command)> {
        self.stage_pending();
        self.pending.pop_front()
    }

    fn stage_pending(&mut self) {
        while self.pending.len() < PENDING_COMMAND_CAPACITY {
            match self.rx.try_recv() {
                Ok(cmd) => self.pending.push_back(cmd),
                Err(_) => break,
            }
        }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedReply {
    pub sequence: u64,
    pub frame: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyWriteAck {
    pub sequence: u64,
    pub result: Result<(), String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyQueueError {
    Full(QueuedReply),
    Disconnected(QueuedReply),
}

pub struct UsbReplyWriter {
    tx: SyncSender<QueuedReply>,
    completion_rx: Receiver<ReplyWriteAck>,
    next_sequence: u64,
}

impl UsbReplyWriter {
    pub fn start() -> io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(REPLY_WRITER_CAPACITY);
        let (completion_tx, completion_rx) = mpsc::sync_channel(REPLY_WRITER_CAPACITY);
        thread::Builder::new()
            .name("usb-console-writer".into())
            .stack_size(WRITER_TASK_STACK_SIZE)
            .spawn(move || run_reply_writer(rx, completion_tx))?;
        Ok(Self {
            tx,
            completion_rx,
            next_sequence: 1,
        })
    }

    pub fn prepare(&mut self, reply: &control::Reply, id: Option<&str>) -> QueuedReply {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1).max(1);
        QueuedReply {
            sequence,
            frame: render_reply_frame(reply, id),
        }
    }

    pub fn enqueue_owned(&self, queued: QueuedReply) -> Result<u64, ReplyQueueError> {
        self.try_enqueue(queued)
    }

    pub fn retry(&self, queued: QueuedReply) -> Result<(), ReplyQueueError> {
        self.try_enqueue(queued).map(|_| ())
    }

    pub fn try_completion(&self) -> Result<Option<ReplyWriteAck>, mpsc::RecvError> {
        match self.completion_rx.try_recv() {
            Ok(ack) => Ok(Some(ack)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(mpsc::RecvError),
        }
    }

    fn try_enqueue(&self, queued: QueuedReply) -> Result<u64, ReplyQueueError> {
        let sequence = queued.sequence;
        match self.tx.try_send(queued) {
            Ok(()) => Ok(sequence),
            Err(TrySendError::Full(queued)) => Err(ReplyQueueError::Full(queued)),
            Err(TrySendError::Disconnected(queued)) => Err(ReplyQueueError::Disconnected(queued)),
        }
    }
}

fn run_reply_writer(rx: Receiver<QueuedReply>, completion_tx: SyncSender<ReplyWriteAck>) {
    while let Ok(queued) = rx.recv() {
        let result = write_frame(&queued.frame).map_err(|err| err.to_string());
        if completion_tx
            .send(ReplyWriteAck {
                sequence: queued.sequence,
                result,
            })
            .is_err()
        {
            break;
        }
    }
}

fn render_reply_frame(reply: &control::Reply, id: Option<&str>) -> String {
    format!("{REPLY_PREFIX}{}\n", control::render_reply(reply, id))
}

fn write_frame(frame: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(frame.as_bytes())?;
    stdout.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_staging_is_bounded_without_dropping_channel_commands() {
        let (tx, rx) = mpsc::sync_channel(PENDING_COMMAND_CAPACITY);
        let mut console = UsbConsole {
            rx,
            pending: VecDeque::with_capacity(PENDING_COMMAND_CAPACITY),
        };
        for _ in 0..PENDING_COMMAND_CAPACITY {
            console
                .pending
                .push_back((None, control::Command::GetStatus));
            tx.send((None, control::Command::GetStatus)).unwrap();
        }

        console.stage_pending();
        assert_eq!(console.pending.len(), PENDING_COMMAND_CAPACITY);
        assert_eq!(console.rx.try_iter().count(), PENDING_COMMAND_CAPACITY);

        assert!(console.poll_command().is_some());
        assert_eq!(console.pending.len(), PENDING_COMMAND_CAPACITY - 1);
        console.stage_pending();
        assert_eq!(console.pending.len(), PENDING_COMMAND_CAPACITY);
        assert_eq!(console.rx.try_iter().count(), PENDING_COMMAND_CAPACITY - 1);
    }

    #[test]
    fn reply_writer_returns_owned_frame_when_full() {
        let (tx, rx) = mpsc::sync_channel(1);
        let (_completion_tx, completion_rx) = mpsc::sync_channel(1);
        let mut writer = UsbReplyWriter {
            tx,
            completion_rx,
            next_sequence: 1,
        };
        rx.try_iter();
        writer
            .tx
            .try_send(QueuedReply {
                sequence: 77,
                frame: "occupied".into(),
            })
            .unwrap();

        let queued = writer.prepare(&control::Reply::Busy, Some("retry"));
        let error = writer.enqueue_owned(queued).unwrap_err();
        let ReplyQueueError::Full(queued) = error else {
            panic!("expected bounded writer backpressure");
        };
        assert_eq!(queued.sequence, 1);
        assert!(queued.frame.contains("retry"));
    }

    #[test]
    fn owned_reply_retries_with_the_same_sequence_after_backpressure() {
        let (tx, rx) = mpsc::sync_channel(1);
        let (_completion_tx, completion_rx) = mpsc::sync_channel(1);
        let mut writer = UsbReplyWriter {
            tx,
            completion_rx,
            next_sequence: 1,
        };
        writer
            .tx
            .try_send(QueuedReply {
                sequence: 99,
                frame: "occupied".into(),
            })
            .unwrap();

        let queued = writer.prepare(&control::Reply::Busy, Some("owned"));
        let returned = match writer.enqueue_owned(queued) {
            Err(ReplyQueueError::Full(queued)) => queued,
            other => panic!("expected owned full reply, got {other:?}"),
        };
        assert_eq!(returned.sequence, 1);
        assert!(returned.frame.contains("owned"));
        assert_eq!(rx.try_recv().unwrap().sequence, 99);
        assert_eq!(writer.retry(returned).unwrap(), ());
        assert_eq!(rx.try_recv().unwrap().sequence, 1);
    }
}
