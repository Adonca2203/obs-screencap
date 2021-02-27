//! # XDG ScreenCast Portal utilities
//!
//! This module defines an interface for interacting with the ScreenCast portal.
//!
//! The general interaction pattern with the `ScreenCast` portal is to open a
//! session, set which source types are of interest, and call `start()`.

use bitflags::bitflags;
use dbus::{
    arg::{OwnedFd, RefArg, Variant},
    blocking::{Connection, Proxy},
    channel::Token,
    Message, Path,
};
use generated::{
    OrgFreedesktopPortalRequestResponse, OrgFreedesktopPortalScreenCast,
    OrgFreedesktopPortalSession,
};
use std::{
    collections::HashMap,
    convert::TryInto,
    os::unix::prelude::RawFd,
    sync::mpsc::{self, Receiver},
    time::Duration,
};

mod generated;

// - - - - - - - - - - - - - - -  Public Interface - - - - - - - - - - - - - -

#[derive(Debug)]
pub enum PortalError {
    Generic(String),
    DBus(dbus::Error),
    Parse,
}

impl std::convert::From<String> for PortalError {
    fn from(error_string: String) -> Self {
        PortalError::Generic(error_string)
    }
}

impl std::convert::From<dbus::Error> for PortalError {
    fn from(err: dbus::Error) -> Self {
        PortalError::DBus(err)
    }
}

impl std::fmt::Display for PortalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "D-Bus Portal error: {0:?}", self)
    }
}

impl std::error::Error for PortalError {}

pub struct ScreenCast {
    state: ConnectionState,
    session: String,
    source_types: Option<SourceType>,
}

impl ScreenCast {
    /// Create a new ScreenCast Session
    ///
    /// Connects to D-Bus and initaialises a ScreenCast object.
    pub fn new() -> Result<Self, PortalError> {
        let state = ConnectionState::open_new()?;

        let session = {
            let request = Request::with_handler(&state, |a| {
                a.results
                    .get("session_handle")
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned()
            })?;
            // Make the initail call to open the session.
            let mut session_args = HashMap::<String, Variant<Box<dyn RefArg>>>::new();
            session_args.insert(
                "handle_token".into(),
                Variant(Box::new(String::from(&request.handle))),
            );
            session_args.insert(
                "session_handle_token".into(),
                Variant(Box::new(String::from(&request.handle))),
            );
            state.desktop_proxy().create_session(session_args)?;
            request.wait_response()?
        };

        Ok(ScreenCast {
            state,
            session,
            source_types: None,
        })
    }

    /// Get the supported source types for this connection
    pub fn source_types(&self) -> Result<SourceType, PortalError> {
        let types = self.state.desktop_proxy().available_source_types()?;
        Ok(SourceType::from_bits_truncate(types))
    }

    /// Set the source types to capture. This should be a subset of
    /// those from `source_types()`.
    pub fn set_source_types(&mut self, types: SourceType) {
        self.source_types = Some(types);
    }

    /// Try to start the screen cast.
    pub fn start(self, parent_window: Option<&str>) -> Result<ActiveScreenCast, PortalError> {
        let desktop_proxy = self.state.desktop_proxy();

        {
            let request = Request::new(&self.state)?;
            let session = dbus::Path::from(&self.session);
            let mut select_args = HashMap::<String, Variant<Box<dyn RefArg>>>::new();
            select_args.insert(
                "handle_token".into(),
                Variant(Box::new(String::from(&request.handle))),
            );
            select_args.insert(
                "types".into(),
                Variant(Box::new(match self.source_types {
                    Some(types) => types.bits(),
                    None => desktop_proxy.available_source_types()?,
                })),
            );
            desktop_proxy.select_sources(session, select_args)?;
            request.wait_response()?;
        }

        let streams = {
            let request = Request::with_handler(&self.state, |response| {
                match response.results.get("streams") {
                    Some(streams) => match streams.as_iter() {
                        Some(streams) => streams
                            .flat_map(|s| {
                                s.as_iter()
                                    .into_iter()
                                    .flat_map(|t| t.map(|u| u.try_into()))
                            })
                            .collect(),
                        None => Err(PortalError::Parse),
                    },
                    None => Err(PortalError::Parse),
                }
            })?;
            let session = dbus::Path::from(&self.session);
            let mut select_args = HashMap::<String, Variant<Box<dyn RefArg>>>::new();
            select_args.insert(
                "handle_token".into(),
                Variant(Box::new(String::from(&request.handle))),
            );
            desktop_proxy.start(session, parent_window.unwrap_or(""), select_args)?;
            request.wait_response()?
        }?;

        let pipewire_fd =
            desktop_proxy.open_pipe_wire_remote(dbus::Path::from(&self.session), HashMap::new())?;

        Ok(ActiveScreenCast {
            state: self.state,
            session_path: self.session,
            pipewire_fd,
            streams,
        })
    }
}

/// An active ScreenCast session. This holds a file descriptor for connecting
/// to PipeWire along with metadata for the active streams.
pub struct ActiveScreenCast {
    state: ConnectionState,
    session_path: String,
    pipewire_fd: OwnedFd,
    streams: Vec<ScreenCastStream>,
}

impl ActiveScreenCast {
    /// Get the fille descriptor for the PipeWire session.
    pub fn pipewire_fd(&self) -> RawFd {
        self.pipewire_fd.clone().into_fd()
    }

    /// Get the streams active in this ScreenCast.
    pub fn streams(&self) -> impl Iterator<Item = &ScreenCastStream> {
        self.streams.iter()
    }

    /// Close the ScreenCast session. This ends the cast.
    pub fn close(&self) -> Result<(), PortalError> {
        // Open a handle to the active session, and close it.
        let session = Session::open(&self.state, &self.session_path)?;
        session.close()?;
        Ok(())
    }
}

impl std::ops::Drop for ActiveScreenCast {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[derive(Debug)]
pub struct ScreenCastStream {
    pipewire_node: u64,
    // TODO: other stream metadata.
}

impl ScreenCastStream {
    /// Get the PipeWire node ID for this stream.
    pub fn pipewire_node(&self) -> u64 {
        self.pipewire_node
    }
}

impl std::convert::TryFrom<&dyn RefArg> for ScreenCastStream {
    type Error = PortalError;

    fn try_from(value: &dyn RefArg) -> Result<Self, Self::Error> {
        let mut parts_iter = value.as_iter().ok_or(PortalError::Parse)?;
        let node_id = parts_iter
            .next()
            .and_then(|r| r.as_u64())
            .ok_or(PortalError::Parse)?;
        // TODO: parse other metdata here.
        Ok(ScreenCastStream {
            pipewire_node: node_id,
        })
    }
}

bitflags! {
    pub struct SourceType : u32  {
        const MONITOR = 0b00001;
        const WINDOW = 0b00010;
    }
}

// - - - - - - - - - - - - - -  Private Implementation - - - - - - - - - - - -

/// D-Bus connection state. Used to access the Desktop portal
/// and open our screencast.
struct ConnectionState {
    connection: Connection,
    sender_token: String,
}

impl ConnectionState {
    /// Open a new D-Bus connection to use for all our requests
    pub fn open_new() -> Result<Self, dbus::Error> {
        // Create a new session and work out our session's sender token. Portal
        // requests will send responses to paths based on this token.
        let connection = Connection::new_session()?;
        let sender_token = String::from(&connection.unique_name().replace(".", "_")[1..]);
        Ok(ConnectionState {
            connection,
            sender_token,
        })
    }

    /// Create a proxy to the main desktop portal object
    pub fn desktop_proxy(&self) -> Proxy<&Connection> {
        self.connection.with_proxy(
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            Duration::from_secs(20),
        )
    }
}

/// A request object. Portal requests are used to wait for responses to ongoing
/// portal operations.
struct Request<'a, Response> {
    /// A proxy connected to this reuqest object on the bus.
    proxy: Proxy<'a, &'a Connection>,
    /// The handle for this request.
    handle: String,
    /// The channel reciever that we can read responses from.
    response: Receiver<Response>,
    /// The match token to remove our D-Bus matcher.
    match_token: Token,
}

impl<'a> Request<'a, ()> {
    /// Create a new request object with the given connection. This generates
    /// a random token for the handle.
    pub fn new(state: &'a ConnectionState) -> Result<Self, PortalError> {
        Self::with_handler(state, |_| {})
    }
}

impl<'a, Response> Request<'a, Response> {
    /// Create a new request object with the given connection and handler. This
    /// generates a random token for the handle. The results of the handler can
    /// be retrieved by calling `wait_result()`.
    pub fn with_handler<ResponseHandler>(
        state: &'a ConnectionState,
        mut on_response: ResponseHandler,
    ) -> Result<Self, PortalError>
    where
        ResponseHandler: FnMut(OrgFreedesktopPortalRequestResponse) -> Response + Send + 'static,
        Response: Send + 'static,
    {
        let handle = format!("screencap{0}", rand::random::<usize>());
        let resp_path = Path::new(format!(
            "/org/freedesktop/portal/desktop/request/{0}/{1}",
            state.sender_token, handle
        ))?;
        let proxy = state.connection.with_proxy(
            "org.freedesktop.portal.Desktop",
            resp_path,
            Duration::from_secs(20),
        );
        let (sender, response) = mpsc::channel();
        let match_token = proxy.match_signal(
            move |a: OrgFreedesktopPortalRequestResponse, _: &Connection, _: &Message| {
                // FIXME: handle error responses here somehow? Currently it is
                //        just up to the `on_response` to deal with it.
                let res = on_response(a);
                sender.send(res).is_ok()
            },
        )?;
        Ok(Request {
            proxy,
            handle,
            response,
            match_token,
        })
    }

    pub fn wait_response(&self) -> Result<Response, PortalError> {
        // Pump the event loop until we receive our expected result
        loop {
            if let Ok(data) = self.response.try_recv() {
                return Ok(data);
            } else {
                self.proxy.connection.process(Duration::from_millis(100))?;
            }
        }
    }
}

impl<'a, T> std::ops::Drop for Request<'a, T> {
    fn drop(&mut self) {
        let _ = self.proxy.match_stop(self.match_token, true);
    }
}

/// A session handle.
struct Session<'a> {
    proxy: Proxy<'a, &'a Connection>,
}

impl<'a> Session<'a> {
    pub fn open(state: &'a ConnectionState, path: &str) -> Result<Self, PortalError> {
        let path = dbus::Path::new(path)?;
        let proxy = state.connection.with_proxy(
            "org.freedesktop.portal.Desktop",
            path,
            Duration::from_secs(20),
        );
        Ok(Session { proxy })
    }

    pub fn close(&self) -> Result<(), PortalError> {
        self.proxy.close()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::SourceType;

    #[test]
    pub fn check_source_types() {
        assert_eq!(1, SourceType::MONITOR.bits());
        assert_eq!(2, SourceType::WINDOW.bits());
        assert_eq!(3, (SourceType::WINDOW | SourceType::MONITOR).bits());
    }
}
