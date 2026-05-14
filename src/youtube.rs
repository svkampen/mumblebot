use std::fmt::Display;
use std::process::Stdio;

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

pub async fn get_title(url: impl AsRef<str> + Display) -> anyhow::Result<String> {
    let pipe = format!("yt-dlp --no-download --dump-json '{}' | jq -r .title", url);

    let child = Command::new("/bin/sh").arg("-c").arg(pipe).output().await?;

    let title = String::from_utf8(child.stdout)?;

    Ok(title)
}

pub async fn stream_url(
    url: String,
    sender: Sender<Vec<i16>>,
    cancel_tok: CancellationToken,
) -> anyhow::Result<()> {
    let pipe = format!(
        "yt-dlp --buffer-size 1024K -x '{}' -o- | ffmpeg -i - -f s16le -ar 48000 -ac 2 -",
        url
    );

    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(pipe)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let mut output = child.stdout.take().unwrap();

    'outer: loop {
        let mut buf: Vec<i16> = Vec::with_capacity(128 * 1024);

        {
            let mut buf_ref: &mut [u8] = unsafe {
                std::slice::from_raw_parts_mut(
                    buf.as_mut_ptr() as *mut u8,
                    buf.capacity() * size_of::<i16>(),
                )
            };

            tokio::select! {
                _ = cancel_tok.cancelled() => {
                    break 'outer;
                },
                res = output.read_buf(&mut buf_ref) => {
                    match res {
                        Ok(nbytes) => unsafe { buf.set_len(nbytes / size_of::<i16>()) },
                        Err(_) => break 'outer,
                    }
                }
            }
        }

        sender.send(buf).await?;
    }

    Ok(())
}
