//!
//! Implementation of the [`Session`] trait through the libseat.
//!
//! This requires libseat to be available on the system.

use libseat::{Seat, SeatEvent};
use std::{
    cell::RefCell,
    collections::HashMap,
    os::unix::io::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
    path::Path,
    rc::{Rc, Weak},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use rustix::{fs::OFlags, io::Errno};

use calloop::{
    EventSource, Poll, PostAction, Readiness, Token, TokenFactory,
    channel::{self, Channel},
};

use crate::backend::session::{AsErrno, Event as SessionEvent, Session};

use tracing::{debug, error, info_span, instrument, warn};

#[derive(Debug)]
struct LibSeatSessionImpl {
    seat: RefCell<Seat>,
    active: Arc<AtomicBool>,
    devices: RefCell<HashMap<RawFd, libseat::Device>>,
}

impl Drop for LibSeatSessionImpl {
    fn drop(&mut self) {
        debug!("Closing seat")
    }
}

/// [`Session`] via the libseat
#[derive(Debug, Clone)]
pub struct LibSeatSession {
    internal: Weak<LibSeatSessionImpl>,
    seat_name: String,
    span: tracing::Span,
}

/// `SessionNotifier` via the libseat
#[derive(Debug)]
pub struct LibSeatSessionNotifier {
    internal: Rc<LibSeatSessionImpl>,
    rx: Channel<SeatEvent>,
    token: Option<Token>,
    span: tracing::Span,
}

impl LibSeatSession {
    /// Tries to create a new session via libseat.
    pub fn new() -> Result<(LibSeatSession, LibSeatSessionNotifier), Error> {
        let span = info_span!("backend_session", "type" = "libseat");
        let _guard = span.enter();
        let (tx, rx) = calloop::channel::channel();

        let seat = {
            Seat::open(move |_seat, event| {
                match event {
                    SeatEvent::Enable => debug!("Enable callback called"),
                    SeatEvent::Disable => debug!("Disable callback called"),
                }
                // The receiver lives as long as the notifier. If it is gone
                // nobody is listening any more, so the event can be dropped.
                if tx.send(event).is_err() {
                    debug!("Seat event dropped, the session notifier is gone");
                }
            })
        };

        drop(_guard);
        let mut seat = seat.map_err(|err| Error::FailedToOpenSession(to_errno(err)))?;
        let seat_name = seat.name().to_owned();

        // In some cases enable_seat event is available right after startup
        // so, we can dispatch it
        if let Err(err) = seat.dispatch(0) {
            error!("Initial libseat dispatch failed: {}", err);
            return Err(Error::FailedToOpenSession(to_errno(err)));
        }
        let active = matches!(rx.try_recv(), Ok(SeatEvent::Enable));

        let internal = Rc::new(LibSeatSessionImpl {
            seat: RefCell::new(seat),
            active: Arc::new(AtomicBool::new(active)),
            devices: RefCell::new(HashMap::new()),
        });

        let session = LibSeatSession {
            internal: Rc::downgrade(&internal),
            seat_name,
            span: span.clone(),
        };

        let notifier = LibSeatSessionNotifier {
            internal,
            rx,
            token: None,
            span,
        };

        Ok((session, notifier))
    }
}

impl Session for LibSeatSession {
    type Error = Error;

    #[instrument(parent = &self.span, skip(self))]
    fn open(&mut self, path: &Path, _flags: OFlags) -> Result<OwnedFd, Self::Error> {
        if let Some(session) = self.internal.upgrade() {
            debug!("Opening device: {:?}", path);

            session
                .seat
                .borrow_mut()
                .open_device(&path)
                .map(|device| {
                    let raw_fd = device.as_fd().as_raw_fd();

                    session.devices.borrow_mut().insert(raw_fd, device);

                    // SAFETY: `libseat::Device` does not close fd on drop
                    unsafe { OwnedFd::from_raw_fd(raw_fd) }
                })
                .map_err(|err| Error::FailedToOpenDevice(Errno::from_raw_os_error(err.into())))
        } else {
            Err(Error::SessionLost)
        }
    }

    #[instrument(parent = &self.span, skip(self))]
    fn close(&mut self, fd: OwnedFd) -> Result<(), Self::Error> {
        if let Some(session) = self.internal.upgrade() {
            debug!("Closing device: {:?}", fd);

            if let Some(dev) = session.devices.borrow_mut().remove(&fd.as_fd().as_raw_fd()) {
                session
                    .seat
                    .borrow_mut()
                    .close_device(dev)
                    .map_err(|err| Error::FailedToCloseDevice(Errno::from_raw_os_error(err.into())))
            } else {
                Ok(())
            }
            // `fd` is closed on drop
        } else {
            Err(Error::SessionLost)
        }
    }

    #[instrument(parent = &self.span, skip(self))]
    fn change_vt(&mut self, vt: i32) -> Result<(), Self::Error> {
        if let Some(session) = self.internal.upgrade() {
            debug!("Session switch: {:?}", vt);
            session
                .seat
                .borrow_mut()
                .switch_session(vt)
                .map_err(|err| Error::FailedToChangeVt(Errno::from_raw_os_error(err.into())))
        } else {
            Err(Error::SessionLost)
        }
    }

    fn is_active(&self) -> bool {
        if let Some(internal) = self.internal.upgrade() {
            internal.active.load(Ordering::SeqCst)
        } else {
            false
        }
    }

    fn seat(&self) -> String {
        self.seat_name.clone()
    }
}

impl LibSeatSessionNotifier {
    /// Creates a new session object belonging to this notifier.
    pub fn session(&self) -> LibSeatSession {
        LibSeatSession {
            internal: Rc::downgrade(&self.internal),
            seat_name: self.internal.seat.borrow_mut().name().to_owned(),
            span: self.span.clone(),
        }
    }
}

impl EventSource for LibSeatSessionNotifier {
    type Event = SessionEvent;
    type Metadata = ();
    type Ret = ();
    type Error = Error;

    #[profiling::function]
    fn process_events<F>(
        &mut self,
        readiness: Readiness,
        token: Token,
        mut callback: F,
    ) -> Result<PostAction, Error>
    where
        F: FnMut(SessionEvent, &mut ()),
    {
        // A failed dispatch means the seat is gone (seatd died, the session was
        // revoked). Events already queued are still delivered below, then the
        // session is paused and the source removed, so the loop never spins on
        // a dead fd.
        let dispatch_failed = Some(token) == self.token
            && match self.internal.seat.borrow_mut().dispatch(0) {
                Ok(_) => false,
                Err(err) => {
                    error!("libseat dispatch failed, removing the session source: {}", err);
                    true
                }
            };

        let internal = &self.internal;
        let action = self
            .rx
            .process_events(readiness, token, |event, _| match event {
                channel::Event::Msg(event) => match event {
                    SeatEvent::Enable => {
                        internal.active.store(true, Ordering::SeqCst);
                        callback(SessionEvent::ActivateSession, &mut ());
                    }
                    SeatEvent::Disable => {
                        internal.active.store(false, Ordering::SeqCst);
                        // The session is paused whether or not the seat
                        // accepted the acknowledgement.
                        if let Err(err) = internal.seat.borrow_mut().disable() {
                            warn!("Failed to acknowledge the seat disable: {}", err);
                        }
                        callback(SessionEvent::PauseSession, &mut ());
                    }
                },
                channel::Event::Closed => {
                    // Tx is stored inside of Seat, and Rc<Seat> is stored in LibSeatSessionNotifier so this is unreachable
                }
            })
            .map_err(|_| Error::SessionLost)?;

        if !dispatch_failed {
            return Ok(action);
        }
        if internal.active.swap(false, Ordering::SeqCst) {
            callback(SessionEvent::PauseSession, &mut ());
        }
        Ok(PostAction::Remove)
    }

    fn register(&mut self, poll: &mut Poll, factory: &mut TokenFactory) -> calloop::Result<()> {
        self.rx.register(poll, factory)?;

        let token = factory.token();
        self.token = Some(token);
        let mut seat = self.internal.seat.borrow_mut();
        let fd = seat_fd(&mut seat)?;
        // Safety: the seat fd cannot be close without removing the LibSeatSessionNotifier from the event loop
        unsafe { poll.register(fd, calloop::Interest::READ, calloop::Mode::Level, token) }
    }

    fn reregister(&mut self, poll: &mut Poll, factory: &mut TokenFactory) -> calloop::Result<()> {
        self.rx.reregister(poll, factory)?;

        let token = factory.token();
        self.token = Some(token);
        let mut seat = self.internal.seat.borrow_mut();
        let fd = seat_fd(&mut seat)?;
        poll.reregister(fd, calloop::Interest::READ, calloop::Mode::Level, token)
    }

    fn unregister(&mut self, poll: &mut Poll) -> calloop::Result<()> {
        self.rx.unregister(poll)?;

        self.token = None;
        let mut seat = self.internal.seat.borrow_mut();
        // If the seat has no fd any more there is nothing left to unregister,
        // the kernel dropped it from the poll set when the fd was closed.
        match seat_fd(&mut seat) {
            Ok(fd) => poll.unregister(fd),
            Err(err) => {
                debug!("No libseat fd to unregister: {}", err);
                Ok(())
            }
        }
    }
}

/// Converts a libseat error number into the errno type used by [`Error`].
fn to_errno(err: impl Into<i32>) -> Errno {
    Errno::from_raw_os_error(err.into())
}

/// The pollable fd of the seat, or the io error libseat reported for it.
fn seat_fd(seat: &mut Seat) -> std::io::Result<BorrowedFd<'_>> {
    seat.get_fd()
        .map_err(|err| std::io::Error::from_raw_os_error(err.into()))
}

/// Errors related to direct/tty sessions
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// Failed to open session
    #[error("Failed to open session: {0}")]
    FailedToOpenSession(Errno),

    /// Failed to open device
    #[error("Failed to open device: {0}")]
    FailedToOpenDevice(Errno),

    /// Failed to close device
    #[error("Failed to close device: {0}")]
    FailedToCloseDevice(Errno),

    /// Failed to close device
    #[error("Failed to change vt: {0}")]
    FailedToChangeVt(Errno),

    /// Session is already closed,
    #[error("Session is already closed")]
    SessionLost,
}

impl AsErrno for Error {
    fn as_errno(&self) -> Option<i32> {
        match self {
            &Self::FailedToOpenSession(errno)
            | &Self::FailedToOpenDevice(errno)
            | &Self::FailedToCloseDevice(errno)
            | &Self::FailedToChangeVt(errno) => Some(errno.raw_os_error()),
            _ => None,
        }
    }
}
