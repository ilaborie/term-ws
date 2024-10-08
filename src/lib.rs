use std::env::current_dir;
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr};
use std::thread;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt as _};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info};

#[tracing::instrument]
pub async fn launch(port: u16) -> anyhow::Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    info!(%port, "Listening...");

    while let Ok((stream, _)) = listener.accept().await {
        let peer = stream.peer_addr()?;
        info!(%peer, "Incoming connection");
        tokio::spawn(accept_connection(peer, stream));
    }
    Ok(())
}

#[tracing::instrument(skip(stream))]
async fn accept_connection(peer: SocketAddr, stream: TcpStream) {
    if let Err(error) = handle_connection(peer, stream).await {
        error!(?error, "Error processing connection");
    }
}

#[tracing::instrument(skip(stream))]
async fn handle_connection(peer: SocketAddr, stream: TcpStream) -> anyhow::Result<()> {
    let ws_stream = accept_async(stream).await?;
    info!(%peer, "🚀 New websocket connection");
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    let pty_system = native_pty_system();
    let size = PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    };
    let pair = pty_system.openpty(size)?;
    let cwd = current_dir()?;
    info!(?cwd, "Workding dir");
    let mut cmd = CommandBuilder::new("fish");
    cmd.cwd(cwd);
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);

    let (tx_message, mut rx_message) = mpsc::unbounded_channel::<Message>();
    let (tx_pty, rx_pty) = std::sync::mpsc::sync_channel::<Vec<u8>>(128);

    // Reading PTY
    let mut reader = pair.master.try_clone_reader()?;
    let _reader_handle = thread::spawn(move || {
        info!("💊 Start reading PTY");
        let mut buffer = [0_u8; 255]; // Size ?
        loop {
            let read = reader.read(&mut buffer);
            match read {
                Ok(0) => {
                    // info!("💊 EOF of pty");
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }
                Ok(size) => {
                    let bytes = buffer[..size].to_vec();
                    let msg = Message::binary(bytes);
                    if let Err(error) = tx_message.send(msg) {
                        error!(%error, "💊 fail to send message");
                    }
                }
                Err(error) => {
                    error!(%error, "💊 fail to read data from PTY");
                    break;
                }
            }
        }
        info!("💊💊 End reading PTY");
    });

    // See [portable_pty example](https://github.com/wez/wezterm/blob/7e8fdc118d2d7ceb51c720a966090f6cb65089b7/pty/examples/whoami.rs)
    // > macOS quirk: the child and reader must be started and
    // > allowed a brief grace period to run before we allow
    // > the writer to drop. Otherwise, the data we send to
    // > the kernel to trigger EOF is interleaved with the
    // > data read by the reader! WTF!?
    // > This appears to be a race condition for very short
    // > lived processes on macOS.
    // > I'd love to find a more deterministic solution to
    // > this than sleeping.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Write to PTY
    let mut writer = pair.master.take_writer()?;
    thread::spawn(move || {
        info!("🌳 Start writing to PTY");
        while let Ok(bytes) = rx_pty.recv() {
            if let Err(error) = writer.write_all(&bytes) {
                error!(?error, "🌳 fail to write on PTY");
            }
        }
        info!("🌳🌳 End writing to PTY");
    });

    // Reader WS
    tokio::spawn(async move {
        info!("🌀 Start reading WS");
        while let Some(msg) = ws_receiver.next().await {
            let Ok(msg) = msg else {
                continue;
            };
            debug!(?msg, "🌀 incoming websocket message");
            let mut messages = vec![];
            match msg {
                Message::Text(txt) => {
                    messages.push(txt.as_bytes().to_vec());
                }
                Message::Binary(data) => {
                    let msg = serde_json::from_slice::<IncomingTermMessage>(&data)
                        .expect("valid payload");
                    info!("🌀 receive message {msg:?}");
                    match msg {
                        IncomingTermMessage::SetSize { rows, cols } => {
                            let size = PtySize {
                                rows,
                                cols,
                                ..Default::default()
                            };
                            pair.master.resize(size).expect("could resize");
                        }
                        IncomingTermMessage::WorkDir { path } => {
                            messages.push(workdir(path));
                        }
                        IncomingTermMessage::Helix { workdir, file } => {
                            messages.push(helix(workdir, file))
                        }
                    }
                }
                Message::Ping(_) | Message::Pong(_) | Message::Close(_) | Message::Frame(_) => {
                    continue;
                }
            };
            // info!("🫶 Having {} messages to run", messages.len());
            for msg in messages {
                if let Err(error) = tx_pty.send(msg) {
                    error!(?error, "🌀 fail to send data for PTY");
                }
            }
        }
        info!("🌀🌀 End reading WS");
    });

    // Sending WS
    while let Some(msg) = rx_message.recv().await {
        ws_sender.send(msg).await?;
    }

    info!("🛑 end of transmission");
    child.kill()?;

    Ok(())
}

fn workdir(workdir: String) -> Vec<u8> {
    let mut result = vec![];
    writeln!(&mut result, "cd {workdir:?}").expect("cd valid");
    writeln!(&mut result, "clear").expect("clear valid");
    result
}

fn helix(workdir: String, file: String) -> Vec<u8> {
    let mut result = vec![];

    writeln!(&mut result, "cd {workdir:?}").expect("cd valid");
    writeln!(&mut result, "clear").expect("clear valid");
    writeln!(&mut result, "hx {file:?}").expect("hx valid");

    result
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "message", rename_all = "kebab-case")]
enum IncomingTermMessage {
    SetSize { rows: u16, cols: u16 },
    WorkDir { path: String },
    Helix { workdir: String, file: String },
}
