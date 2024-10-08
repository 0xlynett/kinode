use crate::net::types::{
    HandshakePayload, OnchainPKI, Peers, PendingPassthroughs, PendingStream, RoutingRequest,
    TCP_PROTOCOL, WS_PROTOCOL,
};
use lib::types::core::{
    Identity, KernelMessage, KnsUpdate, Message, MessageSender, NetAction, NetworkErrorSender,
    NodeRouting, PrintSender, Printout, Request, Response, SendError, SendErrorKind,
    WrappedSendError,
};
use {
    futures::{SinkExt, StreamExt},
    ring::signature::{self},
    snow::params::NoiseParams,
    tokio::time,
    tokio_tungstenite::connect_async,
};

lazy_static::lazy_static! {
    pub static ref PARAMS: NoiseParams = "Noise_XX_25519_ChaChaPoly_BLAKE2s"
                                        .parse()
                                        .expect("net: couldn't build noise params?");
}

/// 10 MB -- TODO analyze as desired, apps can always chunk data into many messages
/// note that this only applies to cross-network messages, not local ones.
pub const MESSAGE_MAX_SIZE: u32 = 10_485_800;

pub const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub async fn create_passthrough(
    our: &Identity,
    our_ip: &str,
    from_id: Identity,
    target_id: Identity,
    peers: &Peers,
    pending_passthroughs: &PendingPassthroughs,
    socket_1: PendingStream,
) -> anyhow::Result<()> {
    // if the target has already generated a pending passthrough for this source,
    // immediately match them
    if let Some(((_target, _from), pending_stream)) =
        pending_passthroughs.remove(&(target_id.name.clone(), from_id.name.clone()))
    {
        tokio::spawn(maintain_passthrough(socket_1, pending_stream));
        return Ok(());
    }
    if socket_1.is_tcp() {
        if let Some((ip, tcp_port)) = target_id.tcp_routing() {
            // create passthrough to direct node over tcp
            let tcp_url = make_conn_url(our_ip, ip, tcp_port, TCP_PROTOCOL)?;
            let Ok(Ok(stream_2)) =
                time::timeout(TIMEOUT, tokio::net::TcpStream::connect(tcp_url.to_string())).await
            else {
                return Err(anyhow::anyhow!(
                    "failed to connect to {} for passthrough requested by {}",
                    target_id.name,
                    from_id.name
                ));
            };
            tokio::spawn(maintain_passthrough(socket_1, PendingStream::Tcp(stream_2)));
            return Ok(());
        }
    } else if socket_1.is_ws() {
        if let Some((ip, ws_port)) = target_id.ws_routing() {
            // create passthrough to direct node over websocket
            let ws_url = make_conn_url(our_ip, ip, ws_port, WS_PROTOCOL)?;
            let Ok(Ok((socket_2, _response))) = time::timeout(TIMEOUT, connect_async(ws_url)).await
            else {
                return Err(anyhow::anyhow!(
                    "failed to connect to {} for passthrough requested by {}",
                    target_id.name,
                    from_id.name
                ));
            };
            tokio::spawn(maintain_passthrough(
                socket_1,
                PendingStream::WebSocket(socket_2),
            ));
            return Ok(());
        }
    }
    // create passthrough to indirect node that we do routing for
    let target_peer = peers.get(&target_id.name).ok_or(anyhow::anyhow!(
        "can't route to {}, not a peer, for passthrough requested by {}",
        target_id.name,
        from_id.name
    ))?;
    if !target_peer.routing_for {
        return Err(anyhow::anyhow!(
            "we don't do routing for {}, for passthrough requested by {}",
            target_id.name,
            from_id.name
        ));
    }
    // send their net:distro:sys process a message, notifying it to create a *matching*
    // passthrough request, which we can pair with this pending one.
    target_peer.sender.send(
        KernelMessage::builder()
            .id(rand::random())
            .source((our.name.as_str(), "net", "distro", "sys"))
            .target((target_id.name.as_str(), "net", "distro", "sys"))
            .message(Message::Request(Request {
                inherit: false,
                expects_response: Some(5),
                body: rmp_serde::to_vec(&NetAction::ConnectionRequest(from_id.name.clone()))?,
                metadata: None,
                capabilities: vec![],
            }))
            .build()
            .unwrap(),
    )?;
    // we'll remove this either if the above message gets a negative response,
    // or if the target node connects to us with a matching passthrough.
    // TODO it is currently possible to have dangling passthroughs in the map
    // if the target is "connected" to us but nonresponsive.
    pending_passthroughs.insert((from_id.name, target_id.name), socket_1);
    Ok(())
}

/// cross the streams -- spawn on own task
pub async fn maintain_passthrough(socket_1: PendingStream, socket_2: PendingStream) {
    match (socket_1, socket_2) {
        (PendingStream::Tcp(socket_1), PendingStream::Tcp(socket_2)) => {
            // do not use bidirectional because if one side closes,
            // we want to close the entire passthrough
            use tokio::io::copy;
            let (mut r1, mut w1) = tokio::io::split(socket_1);
            let (mut r2, mut w2) = tokio::io::split(socket_2);
            tokio::select! {
                _ = copy(&mut r1, &mut w2) => {},
                _ = copy(&mut r2, &mut w1) => {},
            }
        }
        (PendingStream::WebSocket(mut socket_1), PendingStream::WebSocket(mut socket_2)) => {
            let mut last_message = std::time::Instant::now();
            loop {
                tokio::select! {
                    maybe_recv = socket_1.next() => {
                        match maybe_recv {
                            Some(Ok(msg)) => {
                                let Ok(()) = socket_2.send(msg).await else {
                                    break
                                };
                                last_message = std::time::Instant::now();
                            }
                            _ => break,
                        }
                    },
                    maybe_recv = socket_2.next() => {
                        match maybe_recv {
                            Some(Ok(msg)) => {
                                let Ok(()) = socket_1.send(msg).await else {
                                    break
                                };
                                last_message = std::time::Instant::now();
                            }
                            _ => break,
                        }
                    },
                    // if a message has not been sent or received in 2-4 hours, close the connection
                    _ = tokio::time::sleep(std::time::Duration::from_secs(7200)) => {
                        if last_message.elapsed().as_secs() > 7200 {
                            break
                        }
                    }
                }
            }
            let _ = socket_1.close(None).await;
            let _ = socket_2.close(None).await;
        }
        _ => {
            // these foolish combinations must never occur
            return;
        }
    }
}

pub fn ingest_log(log: KnsUpdate, pki: &OnchainPKI) {
    pki.insert(
        log.name.clone(),
        Identity {
            name: log.name.clone(),
            networking_key: log.public_key,
            routing: if log.ips.is_empty() {
                NodeRouting::Routers(log.routers)
            } else {
                NodeRouting::Direct {
                    ip: log.ips[0].clone(),
                    ports: log.ports,
                }
            },
        },
    );
}

pub fn validate_signature(from: &str, signature: &[u8], message: &[u8], pki: &OnchainPKI) -> bool {
    if let Some(peer_id) = pki.get(from) {
        let their_networking_key = signature::UnparsedPublicKey::new(
            &signature::ED25519,
            net_key_string_to_hex(&peer_id.networking_key),
        );
        their_networking_key.verify(message, signature).is_ok()
    } else {
        false
    }
}

pub fn validate_routing_request(
    our_name: &String,
    buf: &[u8],
    pki: &OnchainPKI,
) -> anyhow::Result<(Identity, Identity)> {
    let routing_request: RoutingRequest = rmp_serde::from_slice(buf)?;
    let from_id = pki.get(&routing_request.source).ok_or(anyhow::anyhow!(
        "unknown KNS name '{}'",
        routing_request.source
    ))?;
    let their_networking_key = signature::UnparsedPublicKey::new(
        &signature::ED25519,
        net_key_string_to_hex(&from_id.networking_key),
    );
    their_networking_key
        .verify(
            format!("{}{}", routing_request.target, our_name).as_bytes(),
            &routing_request.signature,
        )
        .map_err(|e| anyhow::anyhow!("their_networking_key.verify failed: {:?}", e))?;
    let target_id = pki.get(&routing_request.target).ok_or(anyhow::anyhow!(
        "unknown KNS name '{}'",
        routing_request.target
    ))?;
    if routing_request.target == routing_request.source {
        return Err(anyhow::anyhow!("can't route to self"));
    }
    Ok((from_id.clone(), target_id.clone()))
}

pub fn validate_handshake(
    handshake: &HandshakePayload,
    their_static_key: &[u8],
    their_id: &Identity,
) -> anyhow::Result<()> {
    if handshake.protocol_version != 1 {
        return Err(anyhow::anyhow!("handshake protocol version mismatch"));
    }
    // verify their signature of their static key
    let their_networking_key = signature::UnparsedPublicKey::new(
        &signature::ED25519,
        net_key_string_to_hex(&their_id.networking_key),
    );
    their_networking_key
        .verify(their_static_key, &handshake.signature)
        .map_err(|e| anyhow::anyhow!("their_networking_key.verify handshake failed: {:?}", e))?;
    Ok(())
}

pub fn build_responder() -> (snow::HandshakeState, Vec<u8>) {
    let builder: snow::Builder<'_> = snow::Builder::new(PARAMS.clone());
    let keypair = builder
        .generate_keypair()
        .expect("net: couldn't generate keypair?");
    (
        builder
            .local_private_key(&keypair.private)
            .unwrap()
            .build_responder()
            .expect("net: couldn't build responder?"),
        keypair.public,
    )
}

pub fn build_initiator() -> (snow::HandshakeState, Vec<u8>) {
    let builder: snow::Builder<'_> = snow::Builder::new(PARAMS.clone());
    let keypair = builder
        .generate_keypair()
        .expect("net: couldn't generate keypair?");
    (
        builder
            .local_private_key(&keypair.private)
            .unwrap()
            .build_initiator()
            .expect("net: couldn't build initiator?"),
        keypair.public,
    )
}

pub fn make_conn_url(our_ip: &str, ip: &str, port: &u16, protocol: &str) -> anyhow::Result<String> {
    // if we have the same public IP as target, route locally,
    // otherwise they will appear offline due to loopback stuff
    let ip = if our_ip == ip { "localhost" } else { ip };
    match protocol {
        TCP_PROTOCOL => Ok(format!("{ip}:{port}")),
        WS_PROTOCOL => Ok(format!("ws://{ip}:{port}")),
        _ => Err(anyhow::anyhow!("unknown protocol: {}", protocol)),
    }
}

pub async fn error_offline(km: KernelMessage, network_error_tx: &NetworkErrorSender) {
    network_error_tx
        .send(WrappedSendError {
            id: km.id,
            source: km.source,
            error: SendError {
                kind: SendErrorKind::Offline,
                target: km.target,
                message: km.message,
                lazy_load_blob: km.lazy_load_blob,
            },
        })
        .await
        .expect("net: network_error_tx was dropped");
}

pub fn net_key_string_to_hex(s: &str) -> Vec<u8> {
    hex::decode(s.strip_prefix("0x").unwrap_or(s)).unwrap_or_default()
}

pub async fn parse_hello_message(
    our: &Identity,
    km: &KernelMessage,
    body: &[u8],
    kernel_message_tx: &MessageSender,
    print_tx: &PrintSender,
) {
    print_loud(
        print_tx,
        &format!(
            "\x1b[3;32m{}: {}\x1b[0m",
            km.source.node,
            std::str::from_utf8(body).unwrap_or("!!message parse error!!")
        ),
    )
    .await;
    KernelMessage::builder()
        .id(km.id)
        .source((our.name.as_str(), "net", "distro", "sys"))
        .target(km.rsvp.as_ref().unwrap_or(&km.source).clone())
        .message(Message::Response((
            Response {
                inherit: false,
                body: "delivered".as_bytes().to_vec(),
                metadata: None,
                capabilities: vec![],
            },
            None,
        )))
        .build()
        .unwrap()
        .send(kernel_message_tx)
        .await;
}

/// Create a terminal printout at verbosity level 0.
pub async fn print_loud(print_tx: &PrintSender, content: &str) {
    Printout::new(0, content).send(print_tx).await;
}

/// Create a terminal printout at verbosity level 2.
pub async fn print_debug(print_tx: &PrintSender, content: &str) {
    Printout::new(2, content).send(print_tx).await;
}
