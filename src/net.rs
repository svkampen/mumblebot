use std::{sync::Arc, time::Duration};

use log::{debug, info, trace, warn};
use num_traits::FromPrimitive;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
    time::sleep,
};

use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;
use tokio_util::sync::CancellationToken;

use crate::{
    mumble_proto,
    types::{
        self, Config, MumbleMsg, MumbleMsgSink, MumbleMsgSource, MumbleType, ReadStream,
        WriteStream,
    },
};

async fn read_message(stream: &mut ReadStream) -> anyhow::Result<MumbleMsg> {
    loop {
        let tag = MumbleType::from_u16(stream.read_u16().await?).expect("valid type tag");

        let len = stream.read_u32().await?;

        let mut buf = vec![0; len as usize];
        stream.read_exact(&mut buf).await?;

        if tag == MumbleType::UDPTunnel {
            continue;
        }

        break Ok(MumbleMsg::from_tagged_data(tag, &buf)?);
    }
}

async fn send_version(out: &mut MumbleMsgSink) -> anyhow::Result<()> {
    let our_version = mumble_proto::Version {
        release: Some("MumbleBot".into()),
        os: Some("Linux".into()),
        // we report version 1.4.0
        version_v2: Some(0x0001000400000000),
        ..Default::default()
    };

    out.send(MumbleMsg::Version(our_version)).await?;

    Ok(())
}

async fn send_auth(out: &mut MumbleMsgSink, cfg: &Config) -> anyhow::Result<()> {
    let our_auth = mumble_proto::Authenticate {
        client_type: Some(1), // BOT
        opus: Some(true),
        username: Some(cfg.username.clone()),
        ..Default::default()
    };

    out.send(MumbleMsg::Authenticate(our_auth)).await?;

    Ok(())
}

pub async fn send_text_message(out: &MumbleMsgSink, text: impl Into<String>) -> anyhow::Result<()> {
    let msg = mumble_proto::TextMessage {
        channel_id: vec![0],
        message: text.into(),
        ..Default::default()
    };

    out.send(MumbleMsg::TextMessage(msg)).await?;

    Ok(())
}

async fn receiver_task(
    mut stream: ReadStream,
    channel: mpsc::Sender<MumbleMsg>,
    ct: CancellationToken,
) -> mpsc::Sender<MumbleMsg> {
    loop {
        let msg = tokio::select! {
            msg = read_message(&mut stream) => {
                msg
            },
            _ = ct.cancelled() => {
                break
            }
        };
        let res = match msg {
            Ok(msg) => {
                debug!("Received message from server: {:?}", msg);
                channel.send(msg).await
            }
            Err(e) => {
                warn!("Error reading from server: {:?}", e);
                break;
            }
        };

        if res.is_err() {
            break;
        }
    }

    channel
}

async fn try_send_voice_data(
    stream: &mut WriteStream,
    seq_nr: u64,
    data: &[u8],
) -> anyhow::Result<()> {
    let seq_nr_encoded = types::varint_encode(seq_nr);
    let len_encoded = types::varint_encode(data.len() as u64);

    let total_packet_len = 1 + seq_nr_encoded.len() + len_encoded.len() + data.len();

    stream.write_u16(MumbleType::UDPTunnel as u16).await?;
    stream.write_u32(total_packet_len as u32).await?;

    stream.write_u8(128).await?; // type + target

    stream.write_all(&seq_nr_encoded).await?;
    stream.write_all(&len_encoded).await?;

    stream.write_all(data).await?;

    Ok(())
}

async fn try_send_msg(stream: &mut WriteStream, msg: &MumbleMsg) -> anyhow::Result<()> {
    let tag = msg.tag();
    let data = msg.as_data();
    debug!("Sending message: {:?} {:?}", tag, data);

    stream.write_u16(tag as u16).await?;

    stream.write_u32(data.len() as u32).await?;
    stream.write_all(&data).await?;

    Ok(())
}

/**
 * Task that sends encoded MumbleMsgs over the wire.
 * The task returns the MumbleMsg receiver channel on exiting so it can be reused.
 */
async fn sender_task(
    mut stream: WriteStream,
    mut channel: mpsc::Receiver<MumbleMsg>,
    ct: CancellationToken,
) -> mpsc::Receiver<MumbleMsg> {
    let mut packet_sequence_nr: u64 = 0;

    let mut ping_interval = tokio::time::interval(Duration::from_secs(15));

    loop {
        let msg = tokio::select! {
            _ = ping_interval.tick() => {
                MumbleMsg::Ping(mumble_proto::Ping::default())
            }
            res = channel.recv() => {
                ping_interval.reset();
                match res {
                    Some(res) => res,
                    None => break
                }
            }
            _ = ct.cancelled() => {
                break
            }
        };

        if let MumbleMsg::UDPTunnel(audio_data) = msg {
            trace!(target: "mumblebot::net::voice",
                "Sending voice data! Seq NR: {:?} Length: {:?}",
                packet_sequence_nr,
                audio_data.len()
            );

            let res = try_send_voice_data(&mut stream, packet_sequence_nr, &audio_data).await;
            if res.is_err() {
                break;
            }

            packet_sequence_nr += 1;
        } else {
            let res = try_send_msg(&mut stream, &msg).await;
            if res.is_err() {
                break;
            }
        }
    }

    channel
}

/**
 * Set up a connection to the Mumble server.
 *
 * Returns the read and write halves of a TlsStream.
 */
async fn connect(server_name: &str, port: u16) -> anyhow::Result<(ReadStream, WriteStream)> {
    let mut root_cert_store = RootCertStore::empty();
    root_cert_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let cert_chain = vec![CertificateDer::from_pem_file("cert.pem")?];
    let key_der = PrivateKeyDer::from_pem_file("key.pem")?;

    let config = ClientConfig::builder()
        .with_root_certificates(root_cert_store)
        .with_client_auth_cert(cert_chain, key_der)?;
    let connector = TlsConnector::from(Arc::new(config));

    let dnsname = ServerName::try_from(String::from(server_name)).unwrap();

    let url_with_port = format!("{}:{}", server_name, port);
    let stream = TcpStream::connect(url_with_port).await?;
    let tls_stream = connector.connect(dnsname, stream).await?;

    Ok(tokio::io::split(tls_stream))
}

async fn reconnect_task(
    cfg: Config,
    mut sender_rd: mpsc::Receiver<MumbleMsg>,
    mut sender_wr: mpsc::Sender<MumbleMsg>,
    mut receiver_wr: mpsc::Sender<MumbleMsg>,
) {
    let mut sender_handle;
    let mut receiver_handle;
    let mut ct;

    loop {
        let (net_rd, net_wr) = loop {
            match connect(&cfg.host, cfg.port).await {
                Ok(res) => break res,
                Err(e) => {
                    warn!("Failed to connect (error {:?}), waiting a minute...", e);
                    sleep(Duration::from_mins(1)).await
                }
            }
        };

        ct = CancellationToken::new();

        sender_handle = tokio::spawn(sender_task(net_wr, sender_rd, ct.child_token()));

        receiver_handle = tokio::spawn(receiver_task(net_rd, receiver_wr, ct.child_token()));

        send_version(&mut sender_wr)
            .await
            .expect("able to put msg in channel");

        send_auth(&mut sender_wr, &cfg)
            .await
            .expect("able to put msg in channel");

        (sender_rd, receiver_wr) = tokio::select! {
            wr_chan = &mut sender_handle => {
                ct.cancel();
                let rd_chan = receiver_handle.await;
                (wr_chan.expect("no panic in write task"), rd_chan.expect("no error in read task"))
            },
            rd_chan = &mut receiver_handle => {
                ct.cancel();
                let wr_chan = sender_handle.await;
                (wr_chan.expect("no panic in write task"), rd_chan.expect("no error in read task"))
            }
        };
    }
}

pub async fn init(cfg: Config) -> anyhow::Result<(MumbleMsgSink, MumbleMsgSource)> {
    info!("Connecting...");

    let (sender_wr, sender_rd) = mpsc::channel(16);
    let (receiver_wr, receiver_rd) = mpsc::channel(16);

    let sender_wr2 = sender_wr.clone();

    tokio::spawn(reconnect_task(cfg, sender_rd, sender_wr2, receiver_wr));

    Ok((sender_wr, receiver_rd))
}
