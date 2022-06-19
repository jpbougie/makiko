use bytes::Bytes;
use parking_lot::Mutex;
use pin_project::pin_project;
use rand::rngs::OsRng;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use crate::{Error, Result, DisconnectError};
use crate::cipher::{self, CipherAlgo};
use crate::kex::{self, KexAlgo};
use crate::mac::{self, MacAlgo};
use crate::pubkey::{self, PubkeyAlgo};
use super::auth;
use super::auth_method::none::{AuthNone, AuthNoneResult};
use super::auth_method::password::{AuthPassword, AuthPasswordResult};
use super::channel::{Channel, ChannelReceiver};
use super::client_event::ClientEvent;
use super::client_state::{self, ClientState};
use super::conn::{self, OpenChannel};
use super::session::{Session, SessionReceiver};

/// Handle to an SSH connection.
///
/// Use this object to send requests to the SSH server. In tandem, you will also need to use
/// [`ClientReceiver`] to handle events that we receive from the server, and [`ClientFuture`] to
/// perform the actual I/O.
///
/// To open a connection, pass your I/O stream (such as `tokio::net::TcpStream`) to
/// [`Client::open()`] and perform authentication using one of the `auth_*` methods. Once
/// you are authenticated, you can open a [`Session`] and execute a program. You can also open
/// multiple sessions from a single connection.
///
/// At the same time, you must handle events from the [`ClientReceiver`] and poll the
/// [`ClientFuture`] (probably from a different task).
///
/// You can cheaply clone this object and safely share the clones between tasks.
#[derive(Clone)]
pub struct Client {
    client_st: Weak<Mutex<ClientState>>,
}

impl Client {
    /// Creates an SSH connection from an existing stream.
    ///
    /// We initialize the client, but do not perform any I/O in this method. You should use the
    /// returned objects as follows:
    ///
    /// - [`Client`] allows you to interact with the SSH client. You should use it to authenticate
    /// yourself to the server and then you can open channels or sessions.
    /// - [`ClientReceiver`] is the receiving half of the client. It produces [`ClientEvent`]s,
    /// which mostly correspond to actions initiated by the server. The only event that you need to
    /// handle is [`ClientEvent::ServerPubkey`]. However, you **must** receive these events in a
    /// timely manner, otherwise the client will stall.
    /// - [`ClientFuture`] is a future that you must poll to drive the connection state machine
    /// forward. You will usually spawn a task for this future.
    pub fn open<IO>(stream: IO, config: ClientConfig) -> Result<(Client, ClientReceiver, ClientFuture<IO>)>
        where IO: AsyncRead + AsyncWrite
    {
        let rng = Box::new(OsRng);
        let (event_tx, event_rx) = mpsc::channel(1);
        let client_st = client_state::new_client(config, rng, event_tx)?;
        let client_st = Arc::new(Mutex::new(client_st));

        let client = Client { client_st: Arc::downgrade(&client_st) };
        let client_rx = ClientReceiver { event_rx };
        let client_fut = ClientFuture { client_st, stream };
        Ok((client, client_rx, client_fut))
    }

    fn upgrade(&self) -> Result<Arc<Mutex<ClientState>>> {
        self.client_st.upgrade().ok_or(Error::ClientClosed)
    }

    /// Try to authenticate using the "none" method.
    ///
    /// The "none" method (RFC 4252, section 5.2) is useful in two situations:
    ///
    /// - The user can be "authorized" without any authorization, e.g. if the user has a blank
    /// password. Note that most SSH servers disable blank passwords by default.
    /// - You want to determine the list of authentication methods for this user, so you expect to
    /// get an [`AuthFailure`][auth::AuthFailure] (inside [`AuthNoneResult::Failure`]) and look at
    /// the [list of methods that can continue][auth::AuthFailure::methods_can_continue].
    ///
    /// If a previous authentication attempt was successful, this call immediately succeeds. If you
    /// start another authentication attempt before this attempt is resolved, it will fail with
    /// [`Error::AuthAborted`].
    pub async fn auth_none(&self, username: String) -> Result<AuthNoneResult> {
        let (result_tx, result_rx) = oneshot::channel();
        let method = AuthNone::new(username, result_tx);
        auth::start_method(&mut self.upgrade()?.lock(), Box::new(method))?;
        result_rx.await.map_err(|_| Error::AuthAborted)
    }

    /// Try to authenticate using the "password" method.
    ///
    /// The "password" method (RFC 4252, section 8) allows you to authorize using a password, but
    /// you can also use it to change the password at the same time. Indeed, the server might
    /// prompt you to change the password, in which case you will get an
    /// [`AuthPasswordResult::ChangePassword`].
    ///
    /// If a previous authentication attempt was successful, this call immediately succeeds
    /// (without changing the password). If you start another authentication attempt before this
    /// attempt is resolved, it will fail with [`Error::AuthAborted`].
    pub async fn auth_password(
        &self,
        username: String,
        password: String,
        new_password: Option<String>,
    ) -> Result<AuthPasswordResult> {
        let (result_tx, result_rx) = oneshot::channel();
        let method = AuthPassword::new(username, password, new_password, result_tx);
        auth::start_method(&mut self.upgrade()?.lock(), Box::new(method))?;
        result_rx.await.map_err(|_| Error::AuthAborted)
    }

    /// Returns true if the server has authenticated you.
    ///
    /// You must use one of the `auth_*` methods to authenticate.
    pub fn is_authenticated(&self) -> Result<bool> {
        Ok(auth::is_authenticated(&self.upgrade()?.lock()))
    }

    /// Opens an SSH session to execute a program or the shell.
    ///
    /// If the session is opened successfully, you receive two objects:
    ///
    /// - [`Session`] is the handle for interacting with the session and sending data to the
    /// server.
    /// - [`SessionReceiver`] receives the [`SessionEvent`][super::SessionEvent]s produced by the
    /// session. You **must** receive these events in time, otherwise the client will stall.
    ///
    /// You can open many sessions in parallel, the SSH protocol will multiplex the sessions over
    /// the underlying connection under the hood.
    ///
    /// This method will wait until you are authenticated before doing anything.
    pub async fn open_session(&self) -> Result<(Session, SessionReceiver)> {
        Session::open(self).await
    }

    /// Opens a raw SSH channel.
    ///
    /// Use this to directly open an SSH channel, as described in RFC 4254, section 5.
    /// The bytes in `open_payload` will be appended to the `SSH_MSG_CHANNEL_OPEN` packet as the
    /// "channel specific data".
    ///
    /// If the channel is opened successfully, you receive three objects:
    ///
    /// - [`Channel`] is the handle for interacting with the channel and sending data to the
    /// server.
    /// - [`ChannelReceiver`] receives the [`ChannelEvent`][super::ChannelEvent]s produced by the
    /// channel. You **must** receive these events in time, otherwise the client will stall.
    /// - The `Bytes` contain the channel specific data from the
    /// `SSH_MSG_CHANNEL_OPEN_CONFIRMATION` packet.
    ///
    /// You should use this method only if you really know what you are doing. To execute programs,
    /// please use [`open_session()`][Self::open_session()] and [`Session`], which wrap the
    /// [`Channel`] in an API that hides the details of the SSH protocol.
    ///
    /// This method will wait until you are authenticated before doing anything.
    pub async fn open_channel(&self, channel_type: String, open_payload: Bytes) 
        -> Result<(Channel, ChannelReceiver, Bytes)> 
    {
        let (confirmed_tx, confirmed_rx) = oneshot::channel();
        let open = OpenChannel {
            channel_type,
            recv_window_max: 100_000,
            recv_packet_len_max: 1_000_000,
            open_payload,
            confirmed_tx,
        };
        conn::open_channel(&mut self.upgrade()?.lock(), open);

        let confirmed = confirmed_rx.await.map_err(|_| Error::ChannelClosed)??;

        let channel = Channel {
            client_st: self.client_st.clone(), 
            channel_st: confirmed.channel_st,
        };
        let channel_rx = ChannelReceiver {
            event_rx: confirmed.event_rx,
        };
        Ok((channel, channel_rx, confirmed.confirm_payload))
    }

    /// Disconnects from the server and closes the client.
    ///
    /// We send a disconnection message to the server, so that they can be sure that we intended to
    /// close the connection (i.e., it was not closed by a man-in-the-middle attacker). After
    /// this message is sent, the [`ClientFuture`] returns.
    ///
    /// The `error` describes the reasons for the disconnection to the server. You may want to use
    /// [`DisconnectError::by_app()`] as a reasonable default value.
    pub fn disconnect(&self, error: DisconnectError) -> Result<()> {
        client_state::disconnect(&mut self.upgrade()?.lock(), error)
    }
}

/// Receiving half of a [`Client`].
///
/// [`ClientReceiver`] provides you with the [`ClientEvent`]s, various events that are produced
/// during the life of the connection. You can usually ignore them, except
/// [`ClientEvent::ServerPubkey`], which is used to verify the server's public key (if you ignore
/// that event, we assume that you reject the key and we abort the connection). However, you
/// **must** receive these events, otherwise the client will stall when the internal buffer of
/// events fills up.
pub struct ClientReceiver {
    event_rx: mpsc::Receiver<ClientEvent>,
}

impl ClientReceiver {
    /// Wait for the next event.
    ///
    /// Returns `None` if the connection was closed.
    pub async fn recv(&mut self) -> Option<ClientEvent> {
        self.event_rx.recv().await
    }

    /// Poll-friendly variant of [`.recv()`][Self::recv()].
    pub fn poll_recv(&mut self, cx: &mut Context) -> Poll<Option<ClientEvent>> {
        self.event_rx.poll_recv(cx)
    }
}

/// Future that drives the connection state machine.
///
/// This future performs the reads and writes on `IO` and stores the state of the connection. You
/// must poll this future, usually by spawning a task for it. The future completes when the
/// connection is closed or when an error happens.
#[pin_project]
pub struct ClientFuture<IO> {
    client_st: Arc<Mutex<client_state::ClientState>>,
    #[pin] stream: IO,
}

impl<IO> ClientFuture<IO> {
    /// Deconstructs the future and gives the `IO` back to you.
    pub fn into_stream(self) -> IO {
        self.stream
    }
}

impl<IO> Future for ClientFuture<IO>
    where IO: AsyncRead + AsyncWrite
{
    type Output = Result<()>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<()>> {
        let this = self.project();
        let mut client_st = this.client_st.lock();
        client_state::poll_client(&mut client_st, this.stream, cx)
    }
}

/// Configuration of a [`Client`].
///
/// You should start from the [default][Default] instance, which has reasonable default
/// configuration, and modify it according to your needs. You may also find the method
/// [`ClientConfig::with()`] syntactically convenient.
///
/// If you need compatibility with old SSH servers that use outdated crypto, you may use
/// [`ClientConfig::default_compatible_insecure()`]. However, this configuration is less secure.
///
/// This struct is `#[non_exhaustive]`, so we may add more fields without breaking backward
/// compatibility.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ClientConfig {
    /// Supported [key exchange algorithms][crate::kex].
    ///
    /// We will use the first algorithm that is also supported by the server. If there is no
    /// overlap, the connnection will abort.
    pub kex_algos: Vec<&'static KexAlgo>,

    /// Supported [server public key algorithms][crate::pubkey].
    ///
    /// We will use the first algorithm that is also supported by the server. If there is no
    /// overlap, the connnection will abort.
    pub server_pubkey_algos: Vec<&'static PubkeyAlgo>,

    /// Supported [encryption algorithms][crate::cipher].
    ///
    /// We will use the first algorithm that is also supported by the server. If there is no
    /// overlap, the connnection will abort.
    pub cipher_algos: Vec<&'static CipherAlgo>,

    /// Supported [message authentication algorithms][crate::mac].
    ///
    /// We will use the first algorithm that is also supported by the server. If there is no
    /// overlap, the connnection will abort.
    pub mac_algos: Vec<&'static MacAlgo>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            kex_algos: vec![&kex::CURVE25519_SHA256, &kex::CURVE25519_SHA256_LIBSSH],
            server_pubkey_algos: vec![&pubkey::SSH_ED25519],
            cipher_algos: vec![&cipher::AES128_CTR, &cipher::AES192_CTR, &cipher::AES256_CTR],
            mac_algos: vec![&mac::HMAC_SHA2_256],
        }
    }
}

impl ClientConfig {
    /// Default configuration with higher compatibility and low security.
    ///
    /// Returns a configuration that includes support for outdated and potentially insecure crypto.
    /// **Use at your own risk!**.
    pub fn default_compatible_insecure() -> ClientConfig {
        Self::default().with(|c| {
            c.kex_algos.push(&kex::DIFFIE_HELLMAN_GROUP14_SHA1);
            c.server_pubkey_algos.push(&pubkey::SSH_RSA);
            c.cipher_algos.push(&cipher::AES256_CBC);
            c.mac_algos.push(&mac::HMAC_SHA1);
        })
    }

    /// Mutate `self` in a closure.
    ///
    /// This method applies your closure to `self` and returns the mutated configuration.
    pub fn with<F: FnOnce(&mut ClientConfig)>(mut self, f: F) -> ClientConfig {
        f(&mut self);
        self
    }
}
