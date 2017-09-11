use std::cmp;
use std::io::prelude::*;
use std::io::{self, ErrorKind};
use std::slice;
use libc::{c_uint, c_int, size_t, c_char, c_void, c_uchar, c_ulong};

use {raw, Session, Error};
use util::{Binding, SessionBinding};

/// A channel represents a portion of an SSH connection on which data can be
/// read and written.
///
/// Channels denote all of SCP uploads and downloads, shell sessions, remote
/// process executions, and other general-purpose sessions. Each channel
/// implements the `Reader` and `Writer` traits to send and receive data.
/// Whether or not I/O operations are blocking is mandated by the `blocking`
/// flag on a channel's corresponding `Session`.
pub struct Channel<'sess> {
    raw: *mut raw::LIBSSH2_CHANNEL,
    sess: &'sess Session,
    read_limit: Option<u64>,
    freed: bool,
}

/// Does libssh work across threads? Documentation seems unclear, so
/// mark unsafe for now.
unsafe impl<'sess> Send for Channel<'sess> {}

/// A channel can have a number of streams, each identified by an id, each of
/// which implements the `Read` and `Write` traits.
pub struct Stream<'channel, 'sess: 'channel> {
    channel: &'channel mut Channel<'sess>,
    id: i32,
}

/// Data received from when a program exits with a signal.
pub struct ExitSignal {
    /// The exit signal received, if the program did not exit cleanly. Does not
    /// contain a SIG prefix
    pub exit_signal: Option<String>,
    /// Error message provided by the remote server (if any)
    pub error_message: Option<String>,
    /// Language tag provided by the remote server (if any)
    pub lang_tag: Option<String>,
}

/// Description of the read window as returned by `Channel::read_window`
#[derive(Copy, Clone)]
pub struct ReadWindow {
    /// The number of bytes which the remote end may send without overflowing
    /// the window limit.
    pub remaining: u32,
    /// The number of bytes actually available to be read.
    pub available: u32,
    /// The window_size_initial as defined by the channel open request
    pub window_size_initial: u32,
}

/// Description of the write window as returned by `Channel::write_window`
#[derive(Copy, Clone)]
pub struct WriteWindow {
    /// The number of bytes which may be safely written on the channel without
    /// blocking.
    pub remaining: u32,
    /// The window_size_initial as defined by the channel open request
    pub window_size_initial: u32,
}

impl<'sess> Channel<'sess> {
    /// Set an environment variable in the remote channel's process space.
    ///
    /// Note that this does not make sense for all channel types and may be
    /// ignored by the server despite returning success.
    pub fn setenv(&mut self, var: &str, val: &str) -> Result<(), Error> {
        unsafe {
            self.sess.rc(raw::libssh2_channel_setenv_ex(self.raw,
                                                        var.as_ptr() as *const _,
                                                        var.len() as c_uint,
                                                        val.as_ptr() as *const _,
                                                        val.len() as c_uint))
        }
    }

    /// Request a PTY on an established channel.
    ///
    /// Note that this does not make sense for all channel types and may be
    /// ignored by the server despite returning success.
    ///
    /// The dimensions argument is a tuple of (width, height, width_px,
    /// height_px)
    pub fn request_pty(&mut self, term: &str,
                       mode: Option<&str>,
                       dim: Option<(u32, u32, u32, u32)>)
                       -> Result<(), Error>{
        self.sess.rc(unsafe {
            let (width, height, width_px, height_px) =
                dim.unwrap_or((80, 24, 0, 0));
            raw::libssh2_channel_request_pty_ex(self.raw,
                                                term.as_ptr() as *const _,
                                                term.len() as c_uint,
                                                mode.map(|s| s.as_ptr())
                                                    .unwrap_or(0 as *const _)
                                                        as *const _,
                                                mode.map(|s| s.len())
                                                    .unwrap_or(0) as c_uint,
                                                width as c_int,
                                                height as c_int,
                                                width_px as c_int,
                                                height_px as c_int)
        })
    }

    /// Request a PTY of a specified size
    pub fn request_pty_size(&mut self, width: u32, height: u32,
                            width_px: Option<u32>, height_px: Option<u32>)
                            -> Result<(), Error> {
        let width_px = width_px.unwrap_or(0);
        let height_px = height_px.unwrap_or(0);
        self.sess.rc(unsafe {
            raw::libssh2_channel_request_pty_size_ex(self.raw,
                                                     width as c_int,
                                                     height as c_int,
                                                     width_px as c_int,
                                                     height_px as c_int)
        })
    }

    /// Execute a command
    ///
    /// An execution is one of the standard process services defined by the SSH2
    /// protocol.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::io::prelude::*;
    /// # use ssh2::Session;
    /// # let session: Session = panic!();
    /// let mut channel = session.channel_session().unwrap();
    /// channel.exec("ls").unwrap();
    /// let mut s = String::new();
    /// channel.read_to_string(&mut s).unwrap();
    /// println!("{}", s);
    /// ```
    pub fn exec(&mut self, command: &str) -> Result<(), Error> {
        self.process_startup("exec", Some(command))
    }

    /// Start a shell
    ///
    /// A shell is one of the standard process services defined by the SSH2
    /// protocol.
    pub fn shell(&mut self) -> Result<(), Error> {
        self.process_startup("shell", None)
    }

    /// Request a subsystem be started.
    ///
    /// A subsystem is one of the standard process services defined by the SSH2
    /// protocol.
    pub fn subsystem(&mut self, system: &str) -> Result<(), Error> {
        self.process_startup("subsystem", Some(system))
    }

    /// Initiate a request on a session type channel.
    ///
    /// The SSH2 protocol currently defines shell, exec, and subsystem as
    /// standard process services.
    pub fn process_startup(&mut self, request: &str, message: Option<&str>)
                           -> Result<(), Error> {
        let message_len = message.map(|s| s.len()).unwrap_or(0);
        let message = message.map(|s| s.as_ptr()).unwrap_or(0 as *const _);
        unsafe {
            let rc = raw::libssh2_channel_process_startup(self.raw,
                        request.as_ptr() as *const _, request.len() as c_uint,
                        message as *const _, message_len as c_uint);
            self.sess.rc(rc)
        }
    }

    /// Get a handle to the stderr stream of this channel.
    ///
    /// The returned handle implements the `Read` and `Write` traits.
    pub fn stderr<'a>(&'a mut self) -> Stream<'a, 'sess> {
        self.stream(::EXTENDED_DATA_STDERR)
    }

    /// Get a handle to a particular stream for this channel.
    ///
    /// The returned handle implements the `Read` and `Write` traits.
    ///
    /// Groups of substreams may be flushed by passing on of the following
    /// constants and then calling `flush()`.
    ///
    /// * FLUSH_EXTENDED_DATA - Flush all extended data substreams
    /// * FLUSH_ALL - Flush all substreams
    pub fn stream<'a>(&'a mut self, stream_id: i32) -> Stream<'a, 'sess> {
        Stream { channel: self, id: stream_id }
    }

    /// Returns the exit code raised by the process running on the remote host
    /// at the other end of the named channel.
    ///
    /// Note that the exit status may not be available if the remote end has not
    /// yet set its status to closed.
    pub fn exit_status(&self) -> Result<i32, Error> {
        // Should really store existing error, call function, check for error
        // after and restore previous error if no new one...but the only error
        // condition right now is a NULL pointer check on self.raw, so let's
        // assume that's not the case.
        Ok(unsafe { raw::libssh2_channel_get_exit_status(self.raw) })
    }

    /// Get the remote exit signal.
    pub fn exit_signal(&self) -> Result<ExitSignal, Error> {
        unsafe {
            let mut sig = 0 as *mut _;
            let mut siglen = 0;
            let mut msg = 0 as *mut _;
            let mut msglen = 0;
            let mut lang = 0 as *mut _;
            let mut langlen = 0;
            let rc = raw::libssh2_channel_get_exit_signal(self.raw,
                                                          &mut sig, &mut siglen,
                                                          &mut msg, &mut msglen,
                                                          &mut lang,
                                                          &mut langlen);
            try!(self.sess.rc(rc));
            return Ok(ExitSignal {
                exit_signal: convert(self, sig, siglen),
                error_message: convert(self, msg, msglen),
                lang_tag: convert(self, lang, langlen),
            })
        }

        unsafe fn convert(chan: &Channel, ptr: *mut c_char,
                          len: size_t) -> Option<String> {
            if ptr.is_null() { return None }
            let slice = slice::from_raw_parts(ptr as *const u8, len as usize);
            let ret = slice.to_vec();
            raw::libssh2_free(chan.sess.raw(), ptr as *mut c_void);
            String::from_utf8(ret).ok()
        }
    }

    /// Check the status of the read window.
    pub fn read_window(&self) -> ReadWindow {
        unsafe {
            let mut avail = 0;
            let mut init = 0;
            let remaining = raw::libssh2_channel_window_read_ex(self.raw,
                                                                &mut avail,
                                                                &mut init);
            ReadWindow {
                remaining: remaining as u32,
                available: avail as u32,
                window_size_initial: init as u32,
            }
        }
    }

    /// Check the status of the write window.
    pub fn write_window(&self) -> WriteWindow {
        unsafe {
            let mut init = 0;
            let remaining = raw::libssh2_channel_window_write_ex(self.raw,
                                                                 &mut init);
            WriteWindow {
                remaining: remaining as u32,
                window_size_initial: init as u32,
            }
        }
    }

    /// Adjust the receive window for a channel by adjustment bytes.
    ///
    /// If the amount to be adjusted is less than the minimum adjustment and
    /// force is false, the adjustment amount will be queued for a later packet.
    ///
    /// This function returns the new size of the receive window (as understood
    /// by remote end) on success.
    pub fn adjust_receive_window(&mut self, adjust: u64, force: bool)
                                 -> Result<u64, Error> {
        let mut ret = 0;
        let rc = unsafe {
            raw::libssh2_channel_receive_window_adjust2(self.raw,
                                                        adjust as c_ulong,
                                                        force as c_uchar,
                                                        &mut ret)
        };
        try!(self.sess.rc(rc));
        Ok(ret as u64)
    }

    /// Artificially limit the number of bytes that will be read from this
    /// channel. Hack intended for use by scp_recv only.
    #[doc(hidden)]
    pub fn limit_read(&mut self, limit: u64) {
        self.read_limit = Some(limit);
    }

    /// Check if the remote host has sent an EOF status for the selected stream.
    pub fn eof(&self) -> bool {
        self.read_limit == Some(0) ||
            unsafe { raw::libssh2_channel_eof(self.raw) != 0 }
    }

    /// Tell the remote host that no further data will be sent on the specified
    /// channel.
    ///
    /// Processes typically interpret this as a closed stdin descriptor.
    pub fn send_eof(&mut self) -> Result<(), Error> {
        unsafe {
            self.sess.rc(raw::libssh2_channel_send_eof(self.raw))
        }
    }

    /// Wait for the remote end to send EOF.
    pub fn wait_eof(&mut self) -> Result<(), Error> {
        unsafe { self.sess.rc(raw::libssh2_channel_wait_eof(self.raw)) }
    }

    /// Wait for the channel to be free.
    pub fn free(&mut self) -> Result<(), Error> {
        if self.freed {
            Ok(())
        } else {
            let f = unsafe {
                self.sess.rc(raw::libssh2_channel_free(self.raw))
            };
            if let Ok(_) = f { self.freed = true; }
            f
        }
    }

    /// Close an active data channel.
    ///
    /// In practice this means sending an SSH_MSG_CLOSE packet to the remote
    /// host which serves as instruction that no further data will be sent to
    /// it. The remote host may still send data back until it sends its own
    /// close message in response.
    ///
    /// To wait for the remote end to close its connection as well, follow this
    /// command with `wait_closed`
    pub fn close(&mut self) -> Result<(), Error> {
        unsafe {
            self.sess.rc(raw::libssh2_channel_close(self.raw))
        }
    }

    /// Enter a temporary blocking state until the remote host closes the named
    /// channel.
    ///
    /// Typically sent after `close` in order to examine the exit status.
    pub fn wait_close(&mut self) -> Result<(), Error> {
        unsafe { self.sess.rc(raw::libssh2_channel_wait_closed(self.raw)) }
    }
}

impl<'sess> SessionBinding<'sess> for Channel<'sess> {
    type Raw = raw::LIBSSH2_CHANNEL;

    unsafe fn from_raw(sess: &'sess Session,
                       raw: *mut raw::LIBSSH2_CHANNEL) -> Channel<'sess> {
        Channel {
            raw: raw,
            sess: sess,
            read_limit: None,
            freed: false,
        }
    }
    fn raw(&self) -> *mut raw::LIBSSH2_CHANNEL { self.raw }
}

impl<'sess> Write for Channel<'sess> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream(0).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream(0).flush()
    }
}

impl<'sess> Read for Channel<'sess> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream(0).read(buf)
    }
}

impl<'sess> Drop for Channel<'sess> {
    fn drop(&mut self) {
        // We can't allow async freeing operations during Drop.
        if !self.freed {
            let b = self.sess.is_blocking();
            if !b { self.sess.set_blocking(true); }
            let r = unsafe { raw::libssh2_channel_free(self.raw) };
            if !b { self.sess.set_blocking(false); }
            assert_eq!(r, 0)
        }
    }
}

impl<'channel, 'sess> Read for Stream<'channel, 'sess> {
    fn read(&mut self, data: &mut [u8]) -> io::Result<usize> {
        if self.channel.eof() { return Ok(0) }

        let data = match self.channel.read_limit {
            Some(amt) => {
                let len = data.len();
                &mut data[..cmp::min(amt as usize, len)]
            }
            None => data,
        };
        let ret = unsafe {
            let rc = raw::libssh2_channel_read_ex(self.channel.raw,
                                                  self.id as c_int,
                                                  data.as_mut_ptr() as *mut _,
                                                  data.len() as size_t);
            self.channel.sess.rc(rc as c_int).map(|()| rc as usize)
        };
        match ret {
            Ok(n) => {
                if let Some(ref mut amt) = self.channel.read_limit {
                    *amt -= n as u64;
                }
                Ok(n)
            }
            Err(e) => Err(io::Error::new(ErrorKind::Other, e)),
        }
    }
}

impl<'channel, 'sess> Write for Stream<'channel, 'sess> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        unsafe {
            let rc = raw::libssh2_channel_write_ex(self.channel.raw,
                                                   self.id as c_int,
                                                   data.as_ptr() as *mut _,
                                                   data.len() as size_t);
            self.channel.sess.rc(rc as c_int).map(|()| rc as usize)
        }.map_err(|e| {
            io::Error::new(ErrorKind::Other, e)
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        unsafe {
            let rc = raw::libssh2_channel_flush_ex(self.channel.raw,
                                                   self.id as c_int);
            self.channel.sess.rc(rc)
        }.map_err(|e| {
            io::Error::new(ErrorKind::Other, e)
        })
    }
}
