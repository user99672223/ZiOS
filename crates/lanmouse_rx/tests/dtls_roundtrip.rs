//! End-to-end check of the lan-mouse transport: a DTLS client configured exactly
//! like lan-mouse's `connect.rs` (v0.11.0) talks to the receiver over localhost.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use futures::StreamExt as _;
use lanmouse_rx::proto::{MAX_EVENT_SIZE, Position, ProtoEvent};
use lanmouse_rx::{Config, Receiver, ServiceEvent};
use sha2::{Digest as _, Sha256};
use tokio::net::UdpSocket;
use webrtc_dtls::{
    config::{Config as DtlsConfig, ExtendedMasterSecretType},
    conn::DTLSConn,
    crypto::Certificate,
};
use webrtc_util::Conn;

fn fingerprint(certificate: &Certificate) -> String {
    let der = certificate.certificate.first().expect("certificate");
    Sha256::digest(der)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

async fn connect(addr: SocketAddr, certificate: Certificate) -> anyhow::Result<Arc<dyn Conn + Send + Sync>> {
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
    socket.connect(addr).await?;
    let config = DtlsConfig {
        certificates: vec![certificate],
        server_name: "ignored".to_owned(),
        insecure_skip_verify: true,
        extended_master_secret: ExtendedMasterSecretType::Require,
        ..Default::default()
    };
    let connection = tokio::time::timeout(
        Duration::from_secs(10),
        DTLSConn::new(socket, config, true, None),
    )
    .await??;
    Ok(Arc::new(connection))
}

async fn send(conn: &Arc<dyn Conn + Send + Sync>, event: ProtoEvent) {
    let (buffer, length): ([u8; MAX_EVENT_SIZE], usize) = event.into();
    conn.send(&buffer[..length]).await.expect("send");
}

async fn receive(conn: &Arc<dyn Conn + Send + Sync>) -> ProtoEvent {
    let mut buffer = [0u8; MAX_EVENT_SIZE];
    tokio::time::timeout(Duration::from_secs(5), conn.recv(&mut buffer))
        .await
        .expect("reply in time")
        .expect("recv");
    buffer.try_into().expect("valid event")
}

async fn next_event(
    events: &mut futures::channel::mpsc::UnboundedReceiver<ServiceEvent>,
) -> ServiceEvent {
    tokio::time::timeout(Duration::from_secs(5), events.next())
        .await
        .expect("event in time")
        .expect("stream open")
}

#[test]
fn authorized_laptop_drives_the_receiver() {
    let directory = tempfile::tempdir().unwrap();
    // A fixed port in the dynamic range, distinct per test process.
    let port = 49152 + (std::process::id() % 10000) as u16;
    let (receiver, mut events) = Receiver::start(Config {
        port,
        certificate_path: directory.path().join("lan-mouse.pem"),
        authorized_path: directory.path().join("authorized.json"),
    })
    .unwrap();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        // Wait for the listener to bind.
        for _ in 0..50 {
            if receiver.status().port.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(receiver.status().port, Some(port), "{:?}", receiver.status().error);
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        // An unknown laptop is rejected and offered for authorization.
        let stranger = Certificate::generate_self_signed(["ignored".to_owned()]).unwrap();
        assert!(connect(addr, stranger.clone()).await.is_err());
        match next_event(&mut events).await {
            ServiceEvent::ConnectionAttempt { fingerprint: attempted } => {
                assert_eq!(attempted, fingerprint(&stranger));
            }
            other => panic!("unexpected {other:?}"),
        }

        // Once authorized, the same laptop connects and drives the cursor.
        receiver.authorize(&fingerprint(&stranger), "laptop").unwrap();
        let conn = connect(addr, stranger).await.unwrap();
        assert!(matches!(next_event(&mut events).await, ServiceEvent::Connected { .. }));

        send(&conn, ProtoEvent::Enter(Position::Left)).await;
        assert!(matches!(receive(&conn).await, ProtoEvent::Ack(_)));
        assert!(matches!(
            next_event(&mut events).await,
            ServiceEvent::Entered { position: Position::Left, .. }
        ));

        send(
            &conn,
            ProtoEvent::Input(input_event::Event::Pointer(input_event::PointerEvent::Motion {
                time: 0,
                dx: 5.0,
                dy: -2.0,
            })),
        )
        .await;
        match next_event(&mut events).await {
            ServiceEvent::Input { event, .. } => assert_eq!(
                event,
                input_event::Event::Pointer(input_event::PointerEvent::Motion {
                    time: 0,
                    dx: 5.0,
                    dy: -2.0
                })
            ),
            other => panic!("unexpected {other:?}"),
        }

        send(&conn, ProtoEvent::Ping).await;
        assert!(matches!(receive(&conn).await, ProtoEvent::Pong(true)));

        // The cursor hit the entry edge: the laptop is told to take over.
        receiver.release();
        assert!(matches!(receive(&conn).await, ProtoEvent::Leave(_)));

        let authorized = std::fs::read_to_string(directory.path().join("authorized.json")).unwrap();
        assert!(authorized.contains("laptop"));
    });
    receiver.stop();
}
