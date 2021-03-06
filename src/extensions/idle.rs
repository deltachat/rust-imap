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
    keepalive: Duration,
    done: bool,
    old_timeout: Option<Duration>,
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
        let mut h = Handle {
            session,
            keepalive: Duration::from_secs(29 * 60),
            done: false,
            old_timeout: None,
        };
        h.init()?;
        Ok(h)
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
            self.done = false;
            return Ok(());
        }

        self.session.read_response_onto(&mut v)?;
        // We should *only* get a continuation on an error (i.e., it gives BAD or NO).
        unreachable!();
    }

    fn terminate(&mut self) -> Result<()> {
        if !self.done {
            self.done = true;
            self.session.write_line(b"DONE")?;
            self.session.stream.get_mut().flush()?;
            self.session.read_response().map(|_| ())
        } else {
            Ok(())
        }
    }

    /// Internal helper that doesn't consume self.
    ///
    /// This is necessary so that we can keep using the inner `Session` in `wait_keepalive`.
    /// return Ok(true) if server reported data, Ok(false) if we ran
    /// into a timeout but idle-waiting can continue.  Any error means
    /// that the underlying stream was closed and a reconnect is neccessary
    fn wait_inner(&mut self) -> Result<bool> {
        let mut v = Vec::new();
        match self.session.readline(&mut v) {
            Err(Error::Io(ref e))
                if e.kind() == io::ErrorKind::TimedOut || e.kind() == io::ErrorKind::WouldBlock =>
            {
                if self.session.debug {
                    eprintln!("wait_inner got error {:?}", e);
                }
                self.terminate()?;
                Ok(false)
            }
            Err(err) => Err(err),
            Ok(_) => Ok(true),
        }
    }

    /// Block until the selected mailbox changes.
    pub fn wait(mut self) -> Result<bool> {
        self.wait_inner()
    }
}

impl<'a, T: SetReadTimeout + Read + Write + 'a> Handle<'a, T> {
    /// Set the keep-alive interval to use when `wait_keepalive` is called.
    ///
    /// The interval defaults to 29 minutes as dictated by RFC 2177.
    pub fn set_keepalive(&mut self, interval: Duration) {
        self.keepalive = interval;
    }

    /// Block until the selected mailbox changes.
    ///
    /// This method differs from [`Handle::wait`] in that it will periodically refresh the IDLE
    /// connection, to prevent the server from timing out our connection. The keepalive interval is
    /// set to 29 minutes by default, as dictated by RFC 2177, but can be changed using
    /// [`Handle::set_keepalive`].
    ///
    /// This is the recommended method to use for waiting.
    pub fn wait_keepalive(self) -> Result<bool> {
        // The server MAY consider a client inactive if it has an IDLE command
        // running, and if such a server has an inactivity timeout it MAY log
        // the client off implicitly at the end of its timeout period.  Because
        // of that, clients using IDLE are advised to terminate the IDLE and
        // re-issue it at least every 29 minutes to avoid being logged off.
        // This still allows a client to receive immediate mailbox updates even
        // though it need only "poll" at half hour intervals.
        let keepalive = self.keepalive;
        self.wait_timeout(keepalive)
    }

    /// Block until the selected mailbox changes, or until the given amount of time has expired.
    pub fn wait_timeout(mut self, timeout: Duration) -> Result<bool> {
        self.old_timeout = self.session.stream.get_mut().read_timeout()?;
        self.session
            .stream
            .get_mut()
            .set_read_timeout(Some(timeout))?;
        self.wait_inner_keepalive()
    }

    fn wait_inner_keepalive(&mut self) -> Result<bool> {
        let mut v = Vec::new();

        match self.session.readline(&mut v).map(|_| true) {
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
            v => {
                self.restore_timeout()?;
                v
            }
        }
    }

    fn restore_timeout(&mut self) -> Result<()> {
        self.session
            .stream
            .get_mut()
            .set_read_timeout(self.old_timeout.take())
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
