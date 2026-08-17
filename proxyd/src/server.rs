//! The WebSocket front end: many clients, one radio.
//!
//! Each connection is its own task with its own request loop, so a client
//! awaiting a transmission never holds up another's status query. Everything
//! they ask for funnels into the arbiter's single command channel, which is
//! where serialisation actually belongs.
//!
//! Nothing here knows what the timings mean. The daemon is a radio proxy.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::{engine, wire};

/// Queued outbound messages per connection. A client that stops reading gets
/// disconnected rather than being allowed to back up into the radio thread.
const CLIENT_QUEUE: usize = 256;

pub struct Server {
    pub commands: mpsc::Sender<engine::Command>,
    pub events: broadcast::Sender<wire::Message>,
}

/// Accept connections until `shutdown` resolves.
pub async fn serve(
    listener: TcpListener,
    server: Arc<Server>,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let local = listener.local_addr().context("listener has no address")?;
    log::info!("listening on ws://{local}");

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(pair) => pair,
                    Err(error) => {
                        log::warn!("accept failed: {error}");
                        continue;
                    }
                };
                let server = Arc::clone(&server);
                tokio::spawn(async move {
                    if let Err(error) = connection(stream, peer, server).await {
                        log::info!("{peer} disconnected: {error:#}");
                    } else {
                        log::info!("{peer} disconnected");
                    }
                });
            }
            () = &mut shutdown => {
                log::info!("shutting down");
                return Ok(());
            }
        }
    }
}

async fn connection(stream: TcpStream, peer: SocketAddr, server: Arc<Server>) -> Result<()> {
    let websocket =
        tokio_tungstenite::accept_async(stream).await.context("WebSocket handshake failed")?;
    log::info!("{peer} connected");

    let (mut sink, mut source) = websocket.split();
    let (outbound, mut queued) = mpsc::channel::<wire::Message>(CLIENT_QUEUE);

    // One task owns the sink, so replies and events can be produced
    // independently without contending for it.
    let writer = tokio::spawn(async move {
        while let Some(message) = queued.recv().await {
            let json = match serde_json::to_string(&message) {
                Ok(json) => json,
                Err(error) => {
                    log::error!("failed to encode a message: {error}");
                    continue;
                }
            };
            if sink.send(WsMessage::Text(json.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    // Forward the radio's events to this client.
    let mut subscription = server.events.subscribe();
    let events_to_client = outbound.clone();
    let forwarder = tokio::spawn(async move {
        loop {
            match subscription.recv().await {
                Ok(message) => {
                    if events_to_client.send(message).await.is_err() {
                        break;
                    }
                }
                // A client too slow to keep up loses frames rather than
                // stalling the radio thread. Saying so is better than
                // letting it wonder why a keypress never arrived.
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    log::warn!("{peer} fell behind, dropped {missed} event(s)");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    while let Some(received) = source.next().await {
        let message = received.context("connection failed")?;
        let text = match message {
            WsMessage::Text(text) => text.to_string(),
            WsMessage::Binary(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => continue,
            WsMessage::Close(_) => break,
        };

        let reply = dispatch(&text, &server).await;
        if outbound.send(reply).await.is_err() {
            break;
        }
    }

    drop(outbound);
    forwarder.abort();
    let _ = writer.await;
    Ok(())
}

/// Parse one request, run it, and produce the reply to send back.
async fn dispatch(text: &str, server: &Server) -> wire::Message {
    let request: wire::Request = match serde_json::from_str(text) {
        Ok(request) => request,
        Err(error) => {
            return wire::Message::event(wire::MessagePayload::Error {
                message: format!("could not parse request: {error}"),
            })
        }
    };
    let id = request.id.clone();

    if request.v != wire::PROTOCOL_VERSION {
        return error(
            id,
            format!(
                "protocol version {} is not supported; this daemon speaks version {}",
                request.v,
                wire::PROTOCOL_VERSION
            ),
        );
    }

    match request.payload {
        wire::RequestPayload::Transmit(transmit) => {
            // Validate before troubling the radio: a bad request should be a
            // fast, specific refusal, not a wait behind the transmit queue.
            if let Err(invalid) = transmit.validate() {
                return error(id, invalid.to_string());
            }
            match ask(server, |reply| engine::Command::Transmit { request: transmit, reply }).await
            {
                Ok(Ok(duration_us)) => {
                    wire::Message::reply(id, wire::MessagePayload::Transmitted { duration_us })
                }
                Ok(Err(error_from_radio)) => error(id, format!("{error_from_radio:#}")),
                Err(gone) => error(id, gone),
            }
        }
        wire::RequestPayload::ConfigureRx(configure) => {
            if let Some(frequency) = configure.frequency {
                if !(1_000_000..=6_000_000_000).contains(&frequency) {
                    return error(
                        id,
                        format!("{frequency} Hz is outside the radio's 1 MHz–6 GHz range"),
                    );
                }
            }
            match ask(server, |reply| engine::Command::ConfigureRx {
                frequency: configure.frequency,
                enabled: configure.enabled,
                reply,
            })
            .await
            {
                Ok(Ok(())) => status_reply(id, server).await,
                Ok(Err(error_from_radio)) => error(id, format!("{error_from_radio:#}")),
                Err(gone) => error(id, gone),
            }
        }
        wire::RequestPayload::Status => status_reply(id, server).await,
    }
}

async fn status_reply(id: Option<String>, server: &Server) -> wire::Message {
    match ask(server, |reply| engine::Command::Status { reply }).await {
        Ok(status) => wire::Message::reply(id, wire::MessagePayload::Status(status)),
        Err(gone) => error(id, gone),
    }
}

/// Send a command to the arbiter and await its reply.
async fn ask<R>(
    server: &Server,
    build: impl FnOnce(oneshot::Sender<R>) -> engine::Command,
) -> Result<R, String> {
    let (reply, response) = oneshot::channel();
    server.commands.send(build(reply)).await.map_err(|_| "the radio thread is gone".to_string())?;
    response.await.map_err(|_| "the radio thread dropped the request".to_string())
}

fn error(id: Option<String>, message: String) -> wire::Message {
    wire::Message::reply(id, wire::MessagePayload::Error { message })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    /// A server with no radio behind it: the command channel's receiver is
    /// held here, so tests can answer requests however they like.
    struct Harness {
        address: SocketAddr,
        commands: mpsc::Receiver<engine::Command>,
        events: broadcast::Sender<wire::Message>,
        _shutdown: oneshot::Sender<()>,
    }

    async fn harness() -> Harness {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (commands, command_rx) = mpsc::channel(8);
        let (events, _) = broadcast::channel(64);
        let (shutdown, shutdown_rx) = oneshot::channel();

        let server = Arc::new(Server { commands, events: events.clone() });
        tokio::spawn(async move {
            let _ = serve(listener, server, async {
                let _ = shutdown_rx.await;
            })
            .await;
        });

        Harness { address, commands: command_rx, events, _shutdown: shutdown }
    }

    async fn connect(
        address: SocketAddr,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>> {
        let url = format!("ws://{address}").into_client_request().unwrap();
        let (socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        socket
    }

    async fn round_trip(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<TcpStream>,
        >,
        request: &str,
    ) -> serde_json::Value {
        socket.send(WsMessage::Text(request.into())).await.unwrap();
        loop {
            let message = socket.next().await.unwrap().unwrap();
            if let WsMessage::Text(text) = message {
                return serde_json::from_str(&text).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn a_malformed_request_gets_a_parse_error_not_a_dropped_connection() {
        let harness = harness().await;
        let mut socket = connect(harness.address).await;

        let reply = round_trip(&mut socket, "not json at all").await;

        assert_eq!(reply["type"], "error");
        assert!(reply["message"].as_str().unwrap().contains("could not parse"));

        // The connection must still be usable afterwards.
        let reply = round_trip(&mut socket, r#"{"type":"nonsense"}"#).await;
        assert_eq!(reply["type"], "error");
    }

    #[tokio::test]
    async fn a_future_protocol_version_is_refused_by_name() {
        let harness = harness().await;
        let mut socket = connect(harness.address).await;

        let reply = round_trip(&mut socket, r#"{"v":99,"type":"status"}"#).await;

        assert_eq!(reply["type"], "error");
        assert!(reply["message"].as_str().unwrap().contains("version 99 is not supported"));
    }

    #[tokio::test]
    async fn an_invalid_transmit_is_refused_without_reaching_the_radio() {
        let mut harness = harness().await;
        let mut socket = connect(harness.address).await;

        let reply = round_trip(
            &mut socket,
            r#"{"id":"x","type":"transmit","frequency":315000000,"timings":[]}"#,
        )
        .await;

        assert_eq!(reply["type"], "error");
        assert_eq!(reply["id"], "x");
        assert!(reply["message"].as_str().unwrap().contains("no timings"));
        assert!(
            harness.commands.try_recv().is_err(),
            "a request the wire layer can refuse must not reach the arbiter"
        );
    }

    #[tokio::test]
    async fn a_transmit_is_forwarded_and_its_reply_carries_the_id() {
        let mut harness = harness().await;
        let mut socket = connect(harness.address).await;

        let answering = tokio::spawn(async move {
            match harness.commands.recv().await.expect("a command should arrive") {
                engine::Command::Transmit { request, reply } => {
                    assert_eq!(request.frequency, 315_000_000);
                    assert_eq!(request.repeat, 9);
                    reply.send(Ok(811_000)).unwrap();
                }
                _ => panic!("wrong command"),
            }
        });

        let reply = round_trip(
            &mut socket,
            r#"{"id":"7","type":"transmit","frequency":315000000,"timings":[500,-500],"repeat":9}"#,
        )
        .await;

        answering.await.unwrap();
        assert_eq!(reply["type"], "transmitted");
        assert_eq!(reply["id"], "7");
        assert_eq!(reply["duration_us"], 811_000);
    }

    #[tokio::test]
    async fn events_reach_every_connected_client() {
        let harness = harness().await;
        let mut first = connect(harness.address).await;
        let mut second = connect(harness.address).await;

        // An event sent before both forwarder tasks have subscribed would
        // reach nobody, so wait for the subscriptions themselves rather than
        // for a duration that happens to be long enough.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while harness.events.receiver_count() < 2 {
            assert!(tokio::time::Instant::now() < deadline, "clients never subscribed");
            tokio::task::yield_now().await;
        }

        harness
            .events
            .send(wire::Message::event(wire::MessagePayload::DeviceState {
                state: wire::DeviceState::Receiving,
            }))
            .unwrap();

        for socket in [&mut first, &mut second] {
            let message = socket.next().await.unwrap().unwrap();
            let WsMessage::Text(text) = message else { panic!("expected text") };
            let event: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(event["type"], "device_state");
            assert_eq!(event["state"], "receiving");
        }
    }
}
