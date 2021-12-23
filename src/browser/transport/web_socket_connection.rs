use std::io;
use std::net::TcpStream;
use std::sync::mpsc::{self, TryRecvError};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{bail, Result};
use log::*;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::WebSocket;
use url::Url;

use crate::protocol;

#[derive(Debug)]
pub struct WebSocketConnection {
    send_tx: Mutex<Option<mpsc::SyncSender<String>>>,
    process_id: Option<u32>,
    thread_handle: Option<std::thread::JoinHandle<()>>,
}

impl WebSocketConnection {
    pub fn new(
        ws_url: &Url,
        process_id: Option<u32>,
        messages_tx: mpsc::Sender<protocol::Message>,
    ) -> Result<Self> {
        let web_socket = Self::websocket_connection(ws_url)?;
        let (send_tx, send_rx) = mpsc::sync_channel(4);

        let thread_handle = std::thread::Builder::new()
            .name(format!("ws-{}", process_id.unwrap_or(0)))
            .spawn(move || {
                trace!("Starting msg dispatching loop");
                Self::dispatch_incoming_messages(web_socket, send_rx, messages_tx, process_id);
                trace!("Quit loop msg dispatching loop");
            })
            .expect("failed to spawn thread");

        Ok(Self {
            send_tx: Mutex::new(Some(send_tx)),
            process_id,
            thread_handle: Some(thread_handle),
        })
    }

    fn websocket_connection(ws_url: &Url) -> Result<WebSocket<MaybeTlsStream<TcpStream>>> {
        let (mut web_socket, _response) = tungstenite::connect(ws_url)?;
        match web_socket.get_mut() {
            MaybeTlsStream::Plain(tcp_stream) => {
                tcp_stream.set_nonblocking(true)?;
            }
            _ => unimplemented!(),
        }
        debug!("Successfully connected to WebSocket: {}", ws_url);
        Ok(web_socket)
    }

    fn dispatch_incoming_messages(
        mut socket: WebSocket<MaybeTlsStream<TcpStream>>,
        send_rx: mpsc::Receiver<String>,
        messages_tx: mpsc::Sender<protocol::Message>,
        process_id: Option<u32>,
    ) {
        let min_sleep = Duration::from_millis(0);
        let max_sleep = Duration::from_millis(100);
        let sleep_increment = Duration::from_millis(5);
        let mut sleep_duration = min_sleep;

        loop {
            match send_rx.try_recv() {
                Ok(message_text) => {
                    sleep_duration = min_sleep;
                    let dispatch_result = Self::dispatch_message(
                        &mut socket,
                        tungstenite::Message::Text(message_text),
                    );
                    match dispatch_result {
                        Ok(()) => continue,
                        Err(error) => match error {
                            tungstenite::Error::ConnectionClosed => break,
                            tungstenite::Error::Io(io_error) => {
                                debug!("WS IO Error for Chrome #{:?}: {}", process_id, io_error);
                                break;
                            }
                            _ => panic!(
                                "Unhandled WebSocket error for Chrome #{:?}: {:?}",
                                process_id, error
                            ),
                        },
                    }
                }
                Err(try_recv_error) => match try_recv_error {
                    TryRecvError::Empty => {}
                    TryRecvError::Disconnected => {
                        debug!("Closing WebSocket connection for Chrome {:?}", process_id);
                        match socket.close(None) {
                            Ok(()) => {}
                            Err(error) => match error {
                                tungstenite::Error::ConnectionClosed => break,
                                tungstenite::Error::Io(io_error) => match io_error.kind() {
                                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => {
                                        std::thread::sleep(sleep_duration);
                                        sleep_duration += sleep_increment;
                                        if sleep_duration > max_sleep {
                                            sleep_duration = max_sleep;
                                        }
                                    }
                                    _ => {
                                        debug!(
                                            "WS IO Error for Chrome #{:?}: {}",
                                            process_id, io_error
                                        );
                                        break;
                                    }
                                },
                                _ => panic!(
                                    "Unhandled WebSocket error for Chrome #{:?}: {:?}",
                                    process_id, error
                                ),
                            },
                        }
                    }
                },
            }

            match socket.read_message() {
                Ok(message) => {
                    sleep_duration = min_sleep;

                    match message {
                        tungstenite::Message::Text(message_text) => {
                            match protocol::parse_raw_message(&message_text) {
                                Ok(message) => {
                                    if messages_tx.send(message).is_err() {
                                        break;
                                    }
                                }
                                Err(_error) => {
                                    trace!("Incoming message isn't recognised as event or method response: {}", message_text);
                                }
                            }
                        }
                        tungstenite::Message::Close(_) => {}
                        _ => panic!("Got a weird message: {:?}", message),
                    }
                }
                Err(error) => match error {
                    tungstenite::Error::ConnectionClosed => break,
                    tungstenite::Error::Io(io_error) => match io_error.kind() {
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => {
                            std::thread::sleep(sleep_duration);
                            sleep_duration += sleep_increment;
                            if sleep_duration > max_sleep {
                                sleep_duration = max_sleep;
                            }
                        }
                        _ => {
                            debug!("WS IO Error for Chrome #{:?}: {}", process_id, io_error);
                            break;
                        }
                    },
                    _ => panic!(
                        "Unhandled WebSocket error for Chrome #{:?}: {:?}",
                        process_id, error
                    ),
                },
            }
        }

        info!("Sending shutdown message to message handling loop");
        if messages_tx
            .send(protocol::Message::ConnectionShutdown)
            .is_err()
        {
            warn!("Couldn't send message to transport loop telling it to shut down")
        }
    }

    fn dispatch_message(
        socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
        message: tungstenite::Message,
    ) -> tungstenite::Result<()> {
        trace!("dispatching message: {:?}", message);

        let min_sleep = Duration::from_millis(0);
        let max_sleep = Duration::from_secs(1);
        let sleep_increment = Duration::from_millis(5);
        let mut sleep_duration = min_sleep;

        loop {
            match socket.write_message(message.clone()) {
                Ok(()) => break,
                Err(error) => match error {
                    tungstenite::Error::Io(io_error) => match io_error.kind() {
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => {
                            std::thread::sleep(sleep_duration);
                            sleep_duration += sleep_increment;
                            if sleep_duration > max_sleep {
                                sleep_duration = max_sleep;
                            }
                        }
                        _ => return Err(tungstenite::Error::Io(io_error)),
                    },
                    tungstenite::Error::SendQueueFull(_message) => {
                        debug!("send queue is full, retrying...");
                        socket.write_pending()?;
                    }
                    _ => return Err(error),
                },
            }
        }

        socket.write_pending()?;
        Ok(())
    }

    pub fn send_message(&self, message_text: String) -> Result<()> {
        match self.send_tx.lock().unwrap().as_mut() {
            Some(send_tx) => send_tx.send(message_text)?,
            None => bail!(
                "WS connection for Chrome #{:?} was already shut down",
                self.process_id
            ),
        }
        Ok(())
    }

    pub fn shutdown(&self) {
        trace!(
            "Shutting down WebSocket connection for Chrome {:?}",
            self.process_id
        );
        match self.send_tx.lock().unwrap().take() {
            Some(send_tx) => drop(send_tx),
            None => trace!(
                "WS connection for Chrome #{:?} was already shut down",
                self.process_id
            ),
        }
    }
}

impl Drop for WebSocketConnection {
    fn drop(&mut self) {
        info!("dropping websocket connection");
        self.shutdown();
        let _ = self.thread_handle.take().unwrap().join();
    }
}
