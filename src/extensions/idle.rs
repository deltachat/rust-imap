//! Adds support for the IMAP IDLE command specificed in [RFC
//! 2177](https://tools.ietf.org/html/rfc2177).

use crate::client::Session;
use crate::error::{Error, Result};
#[cfg(feature = "tls")]
use native_tls::TlsStream;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// `Handle` allows a client to block waiting for changes to the remote mailbox.
///
/// The handle blocks using the [`IDLE` command](https://tools.ietf.org/html/rfc2177#section-3)
/// specificed in [RFC 2177](https://tools.ietf.org/html/rfc2177) until the underlying server state
/// changes in some way. While idling does inform the client what changes happened on the server,
/// this implementation will currently just block until _anything_ changes, and then notify the
///
/// Note that the server MAY consider a client inactive if it has an IDLE command running, and if
/// such a server has an inactivity timeout it MAY log the client off implicitly at the end of its
/// timeout period.  Because of that, clients using IDLE are advised to terminate the IDLE and
/// re-issue it at least every 29 minutes to avoid being logged off. [`Handle::wait_keepalive`]
/// does this. This still allows a client to receive immediate mailbox updates even though it need
/// only "poll" at half hour intervals.
///
/// As long as a [`Handle`] is active, the mailbox cannot be otherwise accessed.
#[derive(Debug)]
pub struct Handle<'a, T: Read + Write> {
    session: &'a mut Session<T>,
    initialized: bool,
}

/// Must be implemented for a transport in order for a `Session` using that transport to support
/// operations with timeouts.
///
/// Examples of where this is useful is for `Handle::wait_keepalive` and
/// `Handle::wait_timeout`.
pub trait SetReadTimeout {
    /// Set the timeout for subsequent reads to the given one.
    ///
    /// If `timeout` is `None`, the read timeout should be removed.
    ///
    /// See also `std::net::TcpStream::set_read_timeout`.
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<()>;

    /// Returns the read timeout of this socket.
    fn read_timeout(&self) -> Result<Option<Duration>>;
}

impl<'a, T: Read + Write + 'a> Handle<'a, T> {
    pub(crate) fn make(session: &'a mut Session<T>) -> Result<Self> {
        let mut handle = Handle {
            session,
            initialized: false,
        };
        Ok(handle)
    }

    fn init(&mut self) -> Result<()> {
        // https://tools.ietf.org/html/rfc2177
        //
        // The IDLE command takes no arguments.
        self.session.run_command("IDLE")?;

        // A tagged response will be sent either
        //
        //   a) if there's an error, or
        //   b) *after* we send DONE
        let mut v = Vec::new();
        self.session.readline(&mut v)?;
        if v.starts_with(b"+") {
            self.initialized = true;
            return Ok(());
        }

        self.session.read_response_onto(&mut v)?;
        // We should *only* not get a continuation on an error (i.e., it gives BAD or NO).
        unreachable!();
    }

    fn terminate(&mut self) -> Result<()> {
        if self.initialized {
            self.session.write_line(b"DONE")?;
            self.initialized = false;
            self.session.stream.get_mut().flush()?;
            self.session.read_response().map(|_| ())
        } else {
            Ok(())
        }
    }
}

impl<'a, T: SetReadTimeout + Read + Write + 'a> Handle<'a, T> {
    /// Block until the selected mailbox changes.
    ///
    /// This method will periodically refresh the IDLE
    /// connection, to prevent the server from timing out our connection.
    /// a typical Duration many mail apps use is 23*60=1380 seconds although
    /// RFC 2177 recommends 29 minutes.
    ///
    pub fn idle_and_wait(self, keepalive_interval: Duration) -> Result<bool> {
        // The server MAY consider a client inactive if it has an IDLE command
        // running, and if such a server has an inactivity timeout it MAY log
        // the client off implicitly at the end of its timeout period.  Because
        // of that, clients using IDLE are advised to terminate the IDLE and
        // re-issue it at least every 29 minutes to avoid being logged off.
        // This still allows a client to receive immediate mailbox updates even
        // though it need only "poll" at half hour intervals.

        let mut old_timeout = self.session.stream.get_mut().read_timeout()?;
        self.init()?;

        self.session
            .stream
            .get_mut()
            .set_read_timeout(Some(keepalive_interval))?;

        let mut v = Vec::new();

        let res = match self.session.readline(&mut v).map(|_| true) {
            Err(Error::Io(ref e))
                if e.kind() == io::ErrorKind::TimedOut || e.kind() == io::ErrorKind::WouldBlock =>
            {
                if self.session.debug {
                    eprintln!("wait_inner got error {:?}", e);
                }
                self.session
                    .stream
                    .get_mut()
                    .set_read_timeout(Some(Duration::from_secs(60)))?;
                self.terminate()?;
                Ok(false)
            }
            v => v,
        };
        self.session
            .stream
            .get_mut()
            .set_read_timeout(old_timeout.take())?;
        res
    }
}

impl<'a, T: Read + Write + 'a> Drop for Handle<'a, T> {
    fn drop(&mut self) {
        // we don't want to panic here if we can't terminate the Idle
        let _ = self.terminate().is_ok();
    }
}

impl<'a> SetReadTimeout for TcpStream {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<()> {
        TcpStream::set_read_timeout(self, timeout).map_err(Error::Io)
    }

    fn read_timeout(&self) -> Result<Option<Duration>> {
        TcpStream::read_timeout(self).map_err(Error::Io)
    }
}

#[cfg(feature = "tls")]
impl<'a> SetReadTimeout for TlsStream<TcpStream> {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<()> {
        self.get_ref().set_read_timeout(timeout).map_err(Error::Io)
    }
    fn read_timeout(&self) -> Result<Option<Duration>> {
        self.get_ref().read_timeout().map_err(Error::Io)
    }
}
