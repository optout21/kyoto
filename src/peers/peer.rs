use std::net::IpAddr;

use bitcoin::{BlockHash, Network};
use thiserror::Error;
use tokio::{
    io::AsyncWriteExt,
    net::{tcp::OwnedWriteHalf, TcpStream},
    select,
    sync::mpsc::{self, Receiver, Sender},
};

use crate::{
    node::channel_messages::{MainThreadMessage, PeerMessage, PeerThreadMessage},
    p2p::outbound_messages::V1OutboundMessage,
};

use super::reader::Reader;

pub(crate) struct Peer {
    nonce: u32,
    time: Option<i32>,
    height: Option<u32>,
    best_hash: Option<BlockHash>,
    ip_addr: IpAddr,
    port: u16,
    last_message: Option<u64>,
    main_thread_sender: Sender<PeerThreadMessage>,
    main_thread_recv: Receiver<MainThreadMessage>,
    network: Network,
}

impl Peer {
    pub fn new(
        nonce: u32,
        ip_addr: IpAddr,
        port: Option<u16>,
        network: Network,
        main_thread_sender: Sender<PeerThreadMessage>,
        main_thread_recv: Receiver<MainThreadMessage>,
    ) -> Self {
        let default_port = match network {
            Network::Bitcoin => 8333,
            Network::Testnet => 18333,
            Network::Signet => 38333,
            Network::Regtest => panic!("unimplemented"),
            _ => unreachable!(),
        };

        Self {
            nonce,
            time: None,
            height: None,
            best_hash: None,
            ip_addr,
            port: port.unwrap_or(default_port),
            last_message: None,
            main_thread_sender,
            main_thread_recv,
            network,
        }
    }

    pub async fn connect(&mut self) -> Result<(), PeerError> {
        println!("Trying TCP connection");
        let mut stream = TcpStream::connect((self.ip_addr, self.port))
            .await
            .map_err(|_| PeerError::TcpConnectionFailed)?;
        let outbound_messages = V1OutboundMessage::new(self.network);
        println!("Writing version message to remote");
        let version_message = outbound_messages.new_version_message(None);
        stream
            .write_all(&version_message)
            .await
            .map_err(|_| PeerError::BufferWriteError)?;
        let (reader, mut writer) = stream.into_split();
        let (tx, mut rx) = mpsc::channel(32);
        let mut peer_reader = Reader::new(reader, tx, self.network);
        tokio::spawn(async move {
            match peer_reader.read_from_remote().await {
                Ok(_) => (),
                Err(_) => {
                    println!("Finished connection with a read error");
                }
            }
        });
        loop {
            select! {
                // the buffer sent us a message
                peer_message = rx.recv() => {
                    match peer_message {
                        Some(message) => {
                            match self.handle_peer_message(message, &mut writer, &outbound_messages).await {
                                Ok(()) => continue,
                                Err(e) => {
                                    match e {
                                        // we were told by the reader thread to disconnect from this peer
                                        PeerError::DisconnectCommand => return Ok(()),
                                        _ => continue,
                                    }
                                },
                            }
                        },
                        None => continue,
                    }
                }
                // the main thread sent us a message
                node_message = self.main_thread_recv.recv() => {
                    match node_message {
                        Some(message) => {
                            match self.main_thread_request(message, &mut writer, &outbound_messages).await {
                                Ok(()) => continue,
                                Err(e) => {
                                    match e {
                                        // we were told by the main thread to disconnect from this peer
                                        PeerError::DisconnectCommand => return Ok(()),
                                        _ => continue,
                                    }
                                },
                            }
                        },
                        None => continue,
                    }
                }
            }
        }
    }

    async fn handle_peer_message(
        &mut self,
        message: PeerMessage,
        writer: &mut OwnedWriteHalf,
        message_generator: &V1OutboundMessage,
    ) -> Result<(), PeerError> {
        match message {
            PeerMessage::Version(version) => {
                self.main_thread_sender
                    .send(PeerThreadMessage {
                        nonce: self.nonce,
                        message: PeerMessage::Version(version),
                    })
                    .await
                    .map_err(|_| PeerError::ThreadChannelError)?;
                println!("Sending Verack");
                writer
                    .write_all(&message_generator.new_verack())
                    .await
                    .map_err(|_| PeerError::BufferWriteError)?;
                // can ask for addresses here depending on if we need them
                return Ok(());
            }
            PeerMessage::Addr(addrs) => {
                self.main_thread_sender
                    .send(PeerThreadMessage {
                        nonce: self.nonce,
                        message: PeerMessage::Addr(addrs),
                    })
                    .await
                    .map_err(|_| PeerError::ThreadChannelError)?;
                return Ok(());
            }
            PeerMessage::Headers(headers) => {
                self.main_thread_sender
                    .send(PeerThreadMessage {
                        nonce: self.nonce,
                        message: PeerMessage::Headers(headers),
                    })
                    .await
                    .map_err(|_| PeerError::ThreadChannelError)?;
                return Ok(());
            }
            PeerMessage::Disconnect => {
                self.main_thread_sender
                    .send(PeerThreadMessage {
                        nonce: self.nonce,
                        message,
                    })
                    .await
                    .map_err(|_| PeerError::ThreadChannelError)?;
                return Err(PeerError::DisconnectCommand);
            }
            PeerMessage::Verack => Ok(()),
            PeerMessage::Ping(nonce) => {
                writer
                    .write_all(&message_generator.new_pong(nonce))
                    .await
                    .map_err(|_| PeerError::BufferWriteError)?;
                Ok(())
            }
            PeerMessage::Pong(_) => Ok(()),
        }
    }

    async fn main_thread_request(
        &mut self,
        request: MainThreadMessage,
        writer: &mut OwnedWriteHalf,
        message_generator: &V1OutboundMessage,
    ) -> Result<(), PeerError> {
        match request {
            MainThreadMessage::GetAddr => {
                writer
                    .write_all(&message_generator.new_get_addr())
                    .await
                    .map_err(|_| PeerError::BufferWriteError)?;
            }
            MainThreadMessage::GetHeaders(config) => {
                let message = message_generator.new_get_headers(config.locators, config.stop_hash);
                writer
                    .write_all(&message)
                    .await
                    .map_err(|_| PeerError::BufferWriteError)?;
            }
            MainThreadMessage::Disconnect => return Err(PeerError::DisconnectCommand),
        }
        Ok(())
    }
}

#[derive(Error, Debug)]
pub enum PeerError {
    #[error("the peer's TCP port was closed or we could not connect")]
    TcpConnectionFailed,
    #[error("a message could not be written to the peer")]
    BufferWriteError,
    #[error("experienced an error sending a message over the channel")]
    ThreadChannelError,
    #[error("the main thread advised this peer to disconnect")]
    DisconnectCommand,
}
