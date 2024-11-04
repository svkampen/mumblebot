use std::{sync::Arc, time::Duration};

use log::{debug, info, trace};
use num_traits::FromPrimitive;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};

use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

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
    let mut our_version = mumble_proto::Version::default();
    our_version.release = Some("MumbleBot".into());
    our_version.os = Some("Linux".into());
    // we report version 1.4.0
    our_version.version_v2 = Some(0x0001000400000000);

    out.send(MumbleMsg::Version(our_version)).await?;

    Ok(())
}

async fn send_auth(out: &mut MumbleMsgSink, cfg: &Config) -> anyhow::Result<()> {
    let mut our_auth = mumble_proto::Authenticate::default();
    our_auth.client_type = Some(1); // BOT
    our_auth.opus = Some(true);
    our_auth.username = Some(cfg.username.clone());

    out.send(MumbleMsg::Authenticate(our_auth)).await?;

    Ok(())
}

pub async fn send_text_message(out: &MumbleMsgSink, text: impl Into<String>) -> anyhow::Result<()> {
    let mut msg = mumble_proto::TextMessage::default();
    msg.channel_id = vec![0];
    msg.message = text.into();

    out.send(MumbleMsg::TextMessage(msg)).await?;

    Ok(())
}

async fn read_task(mut stream: ReadStream, channel: mpsc::Sender<MumbleMsg>) -> anyhow::Result<()> {
    loop {
        let msg = read_message(&mut stream).await?;
        debug!("Received message from server: {:?}", msg);

        channel.send(msg).await?;
    }
}

/**
 * Task that sends encoded MumbleMsgs over the wire.
 */
async fn mumblemsg_sender_task(
    mut channel: tokio::sync::mpsc::Receiver<MumbleMsg>,
    mut stream: WriteStream,
) -> anyhow::Result<()> {
    let mut packet_sequence_nr: u64 = 0;

    let mut ping_interval = tokio::time::interval(Duration::from_secs(15));

    loop {
        let msg = tokio::select! {
            _ = ping_interval.tick() => {
                MumbleMsg::Ping(mumble_proto::Ping::default())
            }
            res = channel.recv() => {
                ping_interval.reset();
                res.unwrap()
            }
        };

        if let MumbleMsg::UDPTunnel(audio_data) = msg {
            trace!(target: "mumblebot::net::voice",
                "Sending voice data! Seq NR: {:?} Length: {:?}",
                packet_sequence_nr,
                audio_data.len()
            );

            let seq_nr_encoded = types::varint_encode(packet_sequence_nr);
            let len_encoded = types::varint_encode(audio_data.len() as u64);

            let total_packet_len = 1 + seq_nr_encoded.len() + len_encoded.len() + audio_data.len();

            stream.write_u16(MumbleType::UDPTunnel as u16).await?;
            stream.write_u32(total_packet_len as u32).await?;

            stream.write_u8(128).await?; // type + target

            stream.write_all(&seq_nr_encoded).await?;
            stream.write_all(&len_encoded).await?;

            stream.write_all(&audio_data).await?;

            packet_sequence_nr = packet_sequence_nr + 1;
        } else {
            let tag = msg.tag();
            let data = msg.as_data();
            debug!("Sending message: {:?} {:?}", tag, data);

            stream.write_u16(tag as u16).await?;

            stream.write_u32(data.len() as u32).await?;
            stream.write_all(&data).await?;
        }
    }
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

pub async fn init(cfg: &Config) -> anyhow::Result<(MumbleMsgSink, MumbleMsgSource)> {
    info!("Connecting...");

    let (rd, wr) = connect(&cfg.host, cfg.port).await?;
    let (mut msg_out_wr, msg_out_rd) = tokio::sync::mpsc::channel::<types::MumbleMsg>(10);

    tokio::spawn(mumblemsg_sender_task(msg_out_rd, wr));

    send_version(&mut msg_out_wr).await?;
    send_auth(&mut msg_out_wr, &cfg).await?;

    let (msg_sink, msg_source) = mpsc::channel(16);

    tokio::spawn(read_task(rd, msg_sink));

    Ok((msg_out_wr, msg_source))
}
