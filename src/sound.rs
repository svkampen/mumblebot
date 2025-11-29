use std::{sync::Arc, time::Duration};

use anyhow::Ok;
use log::{debug, info};
use opus::{Application, Channels, Encoder};
use tokio::{
    sync::{Mutex, mpsc},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::types::{self, MumbleMsg};

const SAMPLE_RATE: u32 = 48_000;

pub fn init_encoder() -> Encoder {
    Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::Audio).expect("encoder construction")
}

struct AudioSenderData {
    source: Option<mpsc::Receiver<Vec<i16>>>,
    sink: types::MumbleMsgSink,
    finish_channel: mpsc::Sender<()>,
    buf: Vec<i16>,
    cancel_tok: Option<CancellationToken>,
    volume: f64,
    task: Option<JoinHandle<anyhow::Result<()>>>,
}

pub struct AudioSender {
    data: Arc<Mutex<AudioSenderData>>,
}

impl AudioSender {
    pub fn new(sink: types::MumbleMsgSink, finish_channel: mpsc::Sender<()>) -> Self {
        AudioSender {
            data: Arc::new(Mutex::new(AudioSenderData {
                source: None,
                sink,
                finish_channel,
                buf: Vec::new(),
                cancel_tok: None,
                volume: 0.25,
                task: None,
            })),
        }
    }

    pub async fn start(&self, source: mpsc::Receiver<Vec<i16>>) -> anyhow::Result<()> {
        let ct = CancellationToken::new();
        let ct2 = ct.clone();

        let mut lg = self.data.lock().await;
        lg.source = Some(source);
        lg.buf.clear();
        lg.cancel_tok = Some(ct);
        lg.task = Some(tokio::spawn(Self::send_task(self.data.clone(), ct2)));

        Ok(())
    }

    pub async fn resume(&self) {
        let ct = CancellationToken::new();
        let ct2 = ct.clone();

        {
            let mut lg = self.data.lock().await;
            // keep the buffer and source the same
            lg.cancel_tok = Some(ct);
            lg.task = Some(tokio::spawn(Self::send_task(self.data.clone(), ct2)));
        }
    }

    pub async fn stop(&self) -> anyhow::Result<()> {
        let task = {
            let mut lg = self.data.lock().await;
            if let Some(ct) = lg.cancel_tok.as_ref() {
                ct.cancel();
            }
            lg.task.take()
        };

        if let Some(task) = task {
            task.await?
        } else {
            debug!("Unexpected stop while no stream task is running?");
            Ok(())
        }
    }

    pub async fn set_volume(&self, volume: f64) {
        self.data.lock().await.volume = volume;
    }

    async fn send_task(
        data: Arc<Mutex<AudioSenderData>>,
        ct: CancellationToken,
    ) -> anyhow::Result<()> {
        const FRAME_MS: u64 = 10;
        const SAMPLES_PER_CHANNEL: usize = (SAMPLE_RATE as usize) / 1_000 * (FRAME_MS as usize);
        const SAMPLES_PER_FRAME: usize = SAMPLES_PER_CHANNEL * 2;

        debug!("Send task starting...");

        // pre-buffer the first thirty seconds
        {
            let mut data = data.lock().await;
            while data.buf.len() < 48_000 * 30 * 2 {
                let chunk = data
                    .source
                    .as_mut()
                    .expect("Source should be set when send_task runs")
                    .recv()
                    .await;

                if let Some(chunk) = chunk {
                    data.buf.extend(chunk);
                } else {
                    break;
                }
            }
        }

        debug!("Pre-buffering done.");

        let mut encoder = init_encoder();
        encoder.set_packet_loss_perc(15)?;

        // maximum frame size for Mumble
        let mut frame_buf = vec![0u8; 1020];

        let mut interval = tokio::time::interval(Duration::from_millis(FRAME_MS));

        let finish_channel = data.lock().await.finish_channel.clone();

        'outer: loop {
            let mut data = data.lock().await;

            while data.buf.len() < SAMPLES_PER_FRAME {
                if let Some(chunk) = data
                    .source
                    .as_mut()
                    .expect("Source should be set when send_task runs")
                    .recv()
                    .await
                {
                    data.buf.extend(chunk);
                } else {
                    break 'outer;
                }
            }

            let mut buf: Vec<i16> = data.buf.drain(..SAMPLES_PER_FRAME).collect();
            for val in buf.iter_mut() {
                *val = (*val as f64 * data.volume) as i16;
            }

            let encoded_len = encoder.encode(&buf, &mut frame_buf)?;

            tokio::select! {
                _ = interval.tick() => {}
                _ = ct.cancelled() => {
                    return Ok(());
                }
            }

            data.sink
                .send(MumbleMsg::UDPTunnel(Vec::from(&frame_buf[..encoded_len])))
                .await?;
        }

        finish_channel.send(()).await?;
        info!("Finished song!");
        Ok(())
    }
}
