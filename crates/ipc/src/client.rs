//! Blocking client (the tray and the status window run it on a thread).

use crate::{parse_reply, read_line, write_msg, Command, Reply};
use std::io::{self, BufReader};
use std::path::Path;
use std::time::Duration;

#[cfg(unix)]
type Stream = std::os::unix::net::UnixStream;
#[cfg(windows)]
type Stream = std::fs::File;

pub struct Client {
    reader: BufReader<Stream>,
    writer: Stream,
}

impl Client {
    /// Connects to the agent at `endpoint` (see [`crate::endpoint`]).
    pub fn connect(endpoint: &Path) -> io::Result<Client> {
        let s = open(endpoint)?;
        Ok(Client { writer: s.try_clone()?, reader: BufReader::new(s) })
    }

    /// Limits how long [`Client::recv`] waits (Unix; named pipes opened as
    /// files have no read timeout, the agent answers right away anyway).
    pub fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        #[cfg(unix)]
        return self.reader.get_ref().set_read_timeout(t);
        #[cfg(windows)]
        {
            let _ = t;
            Ok(())
        }
    }

    pub fn send(&mut self, cmd: &Command) -> io::Result<()> {
        write_msg(&mut self.writer, cmd)
    }

    /// Next reply or pushed update.
    pub fn recv(&mut self) -> io::Result<Reply> {
        match read_line(&mut self.reader)? {
            Some(l) => parse_reply(&l),
            None => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the agent closed the connection")),
        }
    }

    pub fn call(&mut self, cmd: &Command) -> io::Result<Reply> {
        self.send(cmd)?;
        self.recv()
    }
}

/// Connects, sends one command and returns its reply.
pub fn request(endpoint: &Path, cmd: &Command) -> io::Result<Reply> {
    let mut c = Client::connect(endpoint)?;
    c.set_read_timeout(Some(Duration::from_secs(10)))?;
    c.call(cmd)
}

#[cfg(unix)]
fn open(endpoint: &Path) -> io::Result<Stream> {
    Stream::connect(endpoint)
}

#[cfg(windows)]
fn open(endpoint: &Path) -> io::Result<Stream> {
    // All pipe instances can be busy for a moment while the agent creates
    // the next one.
    const ERROR_PIPE_BUSY: i32 = 231;
    let mut tries = 0;
    let f = loop {
        match std::fs::OpenOptions::new().read(true).write(true).open(endpoint) {
            Ok(f) => break f,
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && tries < 40 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(e),
        }
    };
    crate::windows::check_pipe_owner(&f)?;
    Ok(f)
}
