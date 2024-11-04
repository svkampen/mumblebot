use librespot::playback::audio_backend::{Sink, SinkError};
use librespot::playback::convert::Converter;
use librespot::playback::decoder::{AudioPacket, AudioPacketError};
use log::{debug, info};
use rubato::{FftFixedIn, Resampler};
use strided::Stride;
use tokio::runtime::Handle;

use tokio::sync::mpsc;

const CHANNELS: usize = 2;

pub struct ResamplingSink {
    rt_handle: Handle,
    output: mpsc::Sender<Vec<i16>>,
    resampler: FftFixedIn<f64>,
    in_buffer: Vec<f64>,
    out_buffer: Vec<Vec<f64>>,
}

impl ResamplingSink {
    pub fn new(handle: Handle, output: mpsc::Sender<Vec<i16>>) -> ResamplingSink {
        let resampler = FftFixedIn::<f64>::new(44100, 48000, 1024, 2, CHANNELS).unwrap();
        let out_buffer = resampler.output_buffer_allocate(true);

        debug!("Initialized ResamplingSink!");

        ResamplingSink {
            rt_handle: handle,
            output,
            resampler,
            in_buffer: vec![],
            out_buffer,
        }
    }
}

/* Get samples separated by channel instead of interleaved. */
fn get_striated_samples(data: &[f64]) -> Vec<Vec<f64>> {
    let stride = Stride::new(data);
    let (l, r) = stride.substrides2();

    let striated_samples: Vec<Vec<f64>> =
        vec![l.iter().cloned().collect(), r.iter().cloned().collect()];

    striated_samples
}

/* Turn a slice of sample striae to a vector of interleaved samples. */
fn to_interleaved_samples(data: &[Vec<f64>], out_frames: usize) -> Vec<i16> {
    let mut interleaved_samples: Vec<i16> = vec![];

    let f64_to_i16 = |x: f64| ((x * (i16::MAX - 1) as f64) as i16);

    for n in 0..(CHANNELS * out_frames) {
        let sample = data[n % CHANNELS][n / CHANNELS];
        interleaved_samples.push(f64_to_i16(sample));
    }

    interleaved_samples
}

trait ToSinkErr {
    fn to_sink_err(self) -> SinkError;
}

impl ToSinkErr for AudioPacketError {
    fn to_sink_err(self) -> SinkError {
        SinkError::OnWrite(self.to_string())
    }
}

impl ToSinkErr for std::io::Error {
    fn to_sink_err(self) -> SinkError {
        SinkError::OnWrite(self.to_string())
    }
}

impl<T> ToSinkErr for tokio::sync::mpsc::error::SendError<T> {
    fn to_sink_err(self) -> SinkError {
        SinkError::OnWrite(self.to_string())
    }
}

impl Sink for ResamplingSink {
    fn start(&mut self) -> Result<(), SinkError> {
        Ok(())
    }

    fn stop(&mut self) -> Result<(), SinkError> {
        Ok(())
    }

    fn write(&mut self, packet: AudioPacket, _converter: &mut Converter) -> Result<(), SinkError> {
        self.in_buffer
            .extend(packet.samples().map_err(ToSinkErr::to_sink_err)?);

        let mut required_samples = self.resampler.input_frames_next() * CHANNELS;
        let mut processed_data = Vec::new();

        while self.in_buffer.len() >= required_samples {
            let chunk: Vec<f64> = self.in_buffer.drain(..required_samples).collect();
            let striated_chunk = get_striated_samples(&chunk);

            let (in_frames, out_frames) = self
                .resampler
                .process_into_buffer(&striated_chunk, &mut self.out_buffer, None)
                .unwrap();

            processed_data.extend(to_interleaved_samples(&mut self.out_buffer, out_frames));

            assert_eq!(in_frames * CHANNELS, required_samples);

            required_samples = self.resampler.input_frames_next() * CHANNELS;
        }

        if processed_data.len() == 0 {
            return Ok(());
        }

        let output = self.output.clone();
        self.rt_handle
            .spawn(async move { output.send(processed_data).await });

        Ok(())
    }
}
