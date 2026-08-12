use std::fmt;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::future::poll_fn;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use yamux::{Config, Connection, Mode, Stream};

const STREAM_MAGIC: &[u8; 4] = b"CXS2";
const STREAM_HEADER_BYTES: usize = 5;
const MAX_STREAMS: usize = 32;
const MAX_CONNECTION_RECEIVE_WINDOW: usize = 32 * 1024 * 1024;
const STREAM_HEADER_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Bridge,
    Agent,
}

impl Role {
    const fn yamux_mode(self) -> Mode {
        match self {
            Self::Bridge => Mode::Client,
            Self::Agent => Mode::Server,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    App,
    Exec,
    Host,
}

impl ChannelKind {
    const fn code(self) -> u8 {
        match self {
            Self::App => 1,
            Self::Exec => 2,
            Self::Host => 3,
        }
    }

    fn from_code(code: u8) -> Result<Self> {
        match code {
            1 => Ok(Self::App),
            2 => Ok(Self::Exec),
            3 => Ok(Self::Host),
            _ => bail!("unknown Yamux channel kind {code}"),
        }
    }
}

pub struct MuxStream {
    inner: Compat<Stream>,
    commands: mpsc::Sender<DriverCommand>,
}

impl MuxStream {
    fn new(stream: Stream, commands: mpsc::Sender<DriverCommand>) -> Self {
        Self {
            inner: stream.compat(),
            commands,
        }
    }

    fn wake_driver(&self) {
        let _ = self.commands.try_send(DriverCommand::Wake);
    }
}

impl fmt::Debug for MuxStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("MuxStream").finish_non_exhaustive()
    }
}

impl AsyncRead for MuxStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(context, buffer)
    }
}

impl AsyncWrite for MuxStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.wake_driver();
        let result = Pin::new(&mut self.inner).poll_write(context, buffer);
        self.wake_driver();
        result
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.wake_driver();
        let result = Pin::new(&mut self.inner).poll_flush(context);
        self.wake_driver();
        result
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.wake_driver();
        let result = Pin::new(&mut self.inner).poll_shutdown(context);
        self.wake_driver();
        result
    }
}

#[derive(Debug)]
pub struct IncomingChannel {
    pub kind: ChannelKind,
    pub stream: MuxStream,
}

#[derive(Debug, Clone)]
pub struct MuxHandle {
    commands: mpsc::Sender<DriverCommand>,
}

impl MuxHandle {
    pub async fn open(&self, kind: ChannelKind) -> Result<MuxStream> {
        let (response_tx, response_rx) = oneshot::channel();
        self.commands
            .send(DriverCommand::Open(response_tx))
            .await
            .map_err(|_| anyhow::anyhow!("multiplexed SSH session is closed"))?;
        let stream = response_rx
            .await
            .context("Yamux driver stopped while opening a channel")??;
        let mut stream = MuxStream::new(stream, self.commands.clone());
        stream.write_all(STREAM_MAGIC).await?;
        stream.write_u8(kind.code()).await?;
        stream.flush().await?;
        Ok(stream)
    }
}

pub struct MuxSession {
    pub handle: MuxHandle,
    pub incoming: mpsc::Receiver<IncomingChannel>,
    pub task: JoinHandle<Result<()>>,
}

pub fn start<R, W>(reader: R, writer: W, role: Role) -> MuxSession
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut config = Config::default();
    config.set_max_num_streams(MAX_STREAMS);
    config.set_max_connection_receive_window(Some(MAX_CONNECTION_RECEIVE_WINDOW));
    config.set_read_after_close(true);
    config.set_split_send_size(16 * 1024);

    let transport = SplitIo { reader, writer }.compat();
    let connection = Connection::new(transport, config, role.yamux_mode());
    let (command_tx, command_rx) = mpsc::channel(32);
    let (incoming_tx, incoming_rx) = mpsc::channel(32);
    let (fault_tx, fault_rx) = mpsc::channel(1);
    let task = tokio::spawn(run_driver(
        connection,
        command_rx,
        command_tx.clone(),
        incoming_tx,
        fault_tx,
        fault_rx,
    ));
    MuxSession {
        handle: MuxHandle {
            commands: command_tx,
        },
        incoming: incoming_rx,
        task,
    }
}

enum DriverCommand {
    Open(oneshot::Sender<yamux::Result<Stream>>),
    Wake,
}

async fn run_driver<T>(
    mut connection: Connection<T>,
    mut commands: mpsc::Receiver<DriverCommand>,
    command_tx: mpsc::Sender<DriverCommand>,
    incoming: mpsc::Sender<IncomingChannel>,
    fault_tx: mpsc::Sender<anyhow::Error>,
    mut faults: mpsc::Receiver<anyhow::Error>,
) -> Result<()>
where
    T: futures_util::AsyncRead + futures_util::AsyncWrite + Unpin + Send + 'static,
{
    let mut commands_open = true;
    loop {
        tokio::select! {
            command = commands.recv(), if commands_open => {
                let Some(command) = command else {
                    commands_open = false;
                    continue;
                };
                if let DriverCommand::Open(response) = command {
                    let stream = poll_fn(|context| connection.poll_new_outbound(context)).await;
                    let _ = response.send(stream);
                }
            }
            inbound = poll_fn(|context| connection.poll_next_inbound(context)) => {
                handle_inbound(inbound, &command_tx, &incoming, &fault_tx)?;
            }
            fault = faults.recv() => {
                if let Some(error) = fault {
                    return Err(error).context("invalid inbound Yamux channel");
                }
            }
        }
    }
}

fn handle_inbound(
    inbound: Option<yamux::Result<Stream>>,
    commands: &mpsc::Sender<DriverCommand>,
    incoming: &mpsc::Sender<IncomingChannel>,
    faults: &mpsc::Sender<anyhow::Error>,
) -> Result<()> {
    match inbound {
        Some(Ok(stream)) => {
            let incoming = incoming.clone();
            let commands = commands.clone();
            let faults = faults.clone();
            tokio::spawn(async move {
                if let Err(error) = identify_inbound(stream, commands, incoming).await {
                    let _ = faults.send(error).await;
                }
            });
            Ok(())
        }
        Some(Err(error)) => Err(error).context("Yamux connection failed"),
        None => bail!("Yamux connection closed"),
    }
}

async fn identify_inbound(
    stream: Stream,
    commands: mpsc::Sender<DriverCommand>,
    incoming: mpsc::Sender<IncomingChannel>,
) -> Result<()> {
    let mut stream = stream.compat();
    let mut header = [0_u8; STREAM_HEADER_BYTES];
    tokio::time::timeout(STREAM_HEADER_TIMEOUT, stream.read_exact(&mut header))
        .await
        .context("timed out waiting for the Yamux stream type header")?
        .context("Yamux stream closed before its type header")?;
    if &header[..STREAM_MAGIC.len()] != STREAM_MAGIC {
        bail!("Yamux stream used an incompatible Shuttle protocol");
    }
    let kind = ChannelKind::from_code(header[STREAM_MAGIC.len()])?;
    incoming
        .send(IncomingChannel {
            kind,
            stream: MuxStream {
                inner: stream,
                commands,
            },
        })
        .await
        .map_err(|_| anyhow::anyhow!("multiplexed channel receiver is closed"))
}

struct SplitIo<R, W> {
    reader: R,
    writer: W,
}

impl<R, W> AsyncRead for SplitIo<R, W>
where
    R: AsyncRead + Unpin,
    W: Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.reader).poll_read(context, buffer)
    }
}

impl<R, W> AsyncWrite for SplitIo<R, W>
where
    R: Unpin,
    W: AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.writer).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.writer).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.writer).poll_shutdown(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn multiplexes_bidirectional_typed_streams() -> Result<()> {
        let (left, right) = tokio::io::duplex(1024 * 1024);
        let (left_read, left_write) = tokio::io::split(left);
        let (right_read, right_write) = tokio::io::split(right);
        let local = start(left_read, left_write, Role::Bridge);
        let mut remote = start(right_read, right_write, Role::Agent);

        let mut opened = local.handle.open(ChannelKind::Exec).await?;
        let incoming = remote.incoming.recv().await.context("missing channel")?;
        assert_eq!(incoming.kind, ChannelKind::Exec);
        let mut accepted = incoming.stream;
        opened.write_all(b"request").await?;
        let mut request = [0_u8; 7];
        accepted.read_exact(&mut request).await?;
        assert_eq!(&request, b"request");
        accepted.write_all(b"response").await?;
        let mut response = [0_u8; 8];
        opened.read_exact(&mut response).await?;
        assert_eq!(&response, b"response");

        drop(opened);
        drop(accepted);
        let mut reopened = local.handle.open(ChannelKind::Host).await?;
        let incoming = remote
            .incoming
            .recv()
            .await
            .context("missing reopened channel")?;
        assert_eq!(incoming.kind, ChannelKind::Host);
        let mut accepted = incoming.stream;
        reopened.write_all(b"still-alive").await?;
        let mut message = [0_u8; 11];
        accepted.read_exact(&mut message).await?;
        assert_eq!(&message, b"still-alive");
        Ok(())
    }

    #[tokio::test]
    async fn opens_streams_concurrently_from_both_roles() -> Result<()> {
        let (left, right) = tokio::io::duplex(1024 * 1024);
        let (left_read, left_write) = tokio::io::split(left);
        let (right_read, right_write) = tokio::io::split(right);
        let mut bridge = start(left_read, left_write, Role::Bridge);
        let mut agent = start(right_read, right_write, Role::Agent);

        let (bridge_opened, agent_opened) = tokio::join!(
            bridge.handle.open(ChannelKind::Exec),
            agent.handle.open(ChannelKind::App)
        );
        let mut bridge_opened = bridge_opened?;
        let mut agent_opened = agent_opened?;
        let bridge_incoming = bridge.incoming.recv().await.context("missing App stream")?;
        let agent_incoming = agent.incoming.recv().await.context("missing Exec stream")?;
        assert_eq!(bridge_incoming.kind, ChannelKind::App);
        assert_eq!(agent_incoming.kind, ChannelKind::Exec);

        let mut bridge_accepted = bridge_incoming.stream;
        let mut agent_accepted = agent_incoming.stream;
        let app_flow = async {
            agent_opened.write_all(b"app").await?;
            let mut bytes = [0_u8; 3];
            bridge_accepted.read_exact(&mut bytes).await?;
            Ok::<_, anyhow::Error>(bytes)
        };
        let exec_flow = async {
            bridge_opened.write_all(b"exec").await?;
            let mut bytes = [0_u8; 4];
            agent_accepted.read_exact(&mut bytes).await?;
            Ok::<_, anyhow::Error>(bytes)
        };
        let (app, exec) = tokio::join!(app_flow, exec_flow);
        assert_eq!(&app?, b"app");
        assert_eq!(&exec?, b"exec");
        Ok(())
    }

    #[tokio::test]
    async fn agent_can_open_the_first_stream_without_bridge_traffic() -> Result<()> {
        let (left, right) = tokio::io::duplex(1024 * 1024);
        let (left_read, left_write) = tokio::io::split(left);
        let (right_read, right_write) = tokio::io::split(right);
        let mut bridge = start(left_read, left_write, Role::Bridge);
        let agent = start(right_read, right_write, Role::Agent);

        let mut opened =
            tokio::time::timeout(Duration::from_secs(1), agent.handle.open(ChannelKind::App))
                .await
                .context("agent opening the first stream stalled")??;
        let incoming = tokio::time::timeout(Duration::from_secs(1), bridge.incoming.recv())
            .await
            .context("bridge did not receive the first Agent stream")?
            .context("bridge channel receiver closed")?;
        assert_eq!(incoming.kind, ChannelKind::App);
        let mut accepted = incoming.stream;
        opened.write_all(b"hello").await?;
        let mut bytes = [0_u8; 5];
        accepted.read_exact(&mut bytes).await?;
        assert_eq!(&bytes, b"hello");
        Ok(())
    }
}
