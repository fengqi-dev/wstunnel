use crate::config::Client;
use crate::executor::{TokioExecutor, TokioExecutorRef};
use anyhow::{Context, anyhow, bail};
use russh::{ChannelMsg, client};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use super::{AcceptAll, connect_ssh};

#[derive(Clone, Debug)]
pub struct ScpClientConfig {
    pub client: Client,
    pub tunnel: Uuid,
    pub user: String,
    pub key: PathBuf,
    pub key_passphrase: Option<String>,
}

#[derive(Debug)]
pub enum ScpTransfer {
    Upload { local: PathBuf, remote: String },
    Download { remote: String, local: PathBuf },
}

pub async fn run_scp_client(
    args: ScpClientConfig,
    transfer: ScpTransfer,
    recursive: bool,
    executor: impl TokioExecutor,
) -> anyhow::Result<()> {
    run_scp_client_impl(args, transfer, recursive, executor.ref_clone()).await
}

async fn run_scp_client_impl(
    args: ScpClientConfig,
    transfer: ScpTransfer,
    recursive: bool,
    executor: impl TokioExecutorRef,
) -> anyhow::Result<()> {
    let ssh = connect_ssh(
        args.client,
        args.tunnel,
        args.user,
        &args.key,
        args.key_passphrase.as_deref(),
        executor,
    )
    .await?;
    let session = ssh.session;
    let tunnel_task = ssh.tunnel_task;

    let result = match transfer {
        ScpTransfer::Upload { local, remote } => scp_upload(&session, &local, &remote, recursive).await,
        ScpTransfer::Download { remote, local } => scp_download(&session, &remote, &local, recursive).await,
    };

    let _ = session
        .disconnect(russh::Disconnect::ByApplication, "scp done", "")
        .await;
    tunnel_task.abort();
    result
}

type SshChannel = russh::Channel<client::Msg>;

async fn recv_response(channel: &mut SshChannel) -> anyhow::Result<()> {
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Data { data }) => {
                let bytes = data.as_ref();
                if bytes.is_empty() {
                    continue;
                }
                match bytes[0] {
                    0 => return Ok(()),
                    1 => {
                        let msg = String::from_utf8_lossy(&bytes[1..]);
                        eprintln!("scp warning: {}", msg.trim());
                        return Ok(());
                    }
                    2 => {
                        let msg = String::from_utf8_lossy(&bytes[1..]);
                        bail!("scp error: {}", msg.trim());
                    }
                    _ => {
                        bail!("unexpected scp response byte: {}", bytes[0]);
                    }
                }
            }
            Some(ChannelMsg::Eof | ChannelMsg::Close) => {
                bail!("channel closed unexpectedly");
            }
            Some(_) => continue,
            None => bail!("channel closed"),
        }
    }
}

async fn recv_data(channel: &mut SshChannel, buf: &mut Vec<u8>) -> anyhow::Result<bool> {
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Data { data }) => {
                buf.extend_from_slice(data.as_ref());
                return Ok(true);
            }
            Some(ChannelMsg::Eof | ChannelMsg::Close) => return Ok(false),
            Some(_) => continue,
            None => return Ok(false),
        }
    }
}

// --- Upload ---

async fn scp_upload(
    session: &client::Handle<AcceptAll>,
    local: &Path,
    remote: &str,
    recursive: bool,
) -> anyhow::Result<()> {
    let cmd = if recursive {
        format!("scp -r -t {remote}")
    } else {
        format!("scp -t {remote}")
    };

    let mut channel = session.channel_open_session().await?;
    channel.exec(true, cmd.as_str()).await?;
    recv_response(&mut channel).await?;

    if local.is_dir() {
        if !recursive {
            bail!("{} is a directory (use --recursive)", local.display());
        }
        upload_dir(&mut channel, local).await?;
    } else {
        upload_file(&mut channel, local).await?;
    }

    channel.eof().await?;
    Ok(())
}

async fn upload_file(channel: &mut SshChannel, path: &Path) -> anyhow::Result<()> {
    let meta = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("cannot stat {}", path.display()))?;
    let size = meta.len();
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("no file name: {}", path.display()))?
        .to_string_lossy();

    let header = format!("C0644 {size} {name}\n");
    channel.data(header.as_bytes()).await?;
    recv_response(channel).await?;

    let data = tokio::fs::read(path)
        .await
        .with_context(|| format!("cannot read {}", path.display()))?;
    channel.data(&data[..]).await?;
    channel.data(&[0u8][..]).await?;
    recv_response(channel).await?;

    eprintln!("{} ({size} bytes)", path.display());
    Ok(())
}

async fn upload_dir(channel: &mut SshChannel, dir: &Path) -> anyhow::Result<()> {
    let name = dir
        .file_name()
        .ok_or_else(|| anyhow!("no directory name: {}", dir.display()))?
        .to_string_lossy();

    let header = format!("D0755 0 {name}\n");
    channel.data(header.as_bytes()).await?;
    recv_response(channel).await?;

    let mut entries = tokio::fs::read_dir(dir)
        .await
        .with_context(|| format!("cannot read dir {}", dir.display()))?;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.is_dir() {
            Box::pin(upload_dir(channel, &path)).await?;
        } else {
            upload_file(channel, &path).await?;
        }
    }

    channel.data(b"E\n" as &[u8]).await?;
    recv_response(channel).await?;
    Ok(())
}

// --- Download ---

async fn scp_download(
    session: &client::Handle<AcceptAll>,
    remote: &str,
    local: &Path,
    recursive: bool,
) -> anyhow::Result<()> {
    let cmd = if recursive {
        format!("scp -r -f {remote}")
    } else {
        format!("scp -f {remote}")
    };

    let mut channel = session.channel_open_session().await?;
    channel.exec(true, cmd.as_str()).await?;
    channel.data(&[0u8][..]).await?;

    let mut dir_stack: Vec<PathBuf> = vec![local.to_path_buf()];
    let mut buf = Vec::with_capacity(8192);

    loop {
        if !recv_data(&mut channel, &mut buf).await? {
            break;
        }

        if buf.is_empty() {
            continue;
        }

        match buf[0] {
            b'C' => {
                let (size, name) = parse_c_header(&buf)?;
                buf.clear();

                channel.data(&[0u8][..]).await?;

                let dest = dir_stack.last().unwrap().join(&name);
                download_file_data(&mut channel, &dest, size, &mut buf).await?;
                eprintln!("{} ({size} bytes)", dest.display());

                channel.data(&[0u8][..]).await?;
                buf.clear();
            }
            b'D' => {
                let (_size, name) = parse_c_header(&buf)?;
                buf.clear();

                let dest = dir_stack.last().unwrap().join(&name);
                tokio::fs::create_dir_all(&dest)
                    .await
                    .with_context(|| format!("cannot create dir {}", dest.display()))?;
                dir_stack.push(dest);

                channel.data(&[0u8][..]).await?;
            }
            b'E' => {
                buf.clear();
                if dir_stack.len() > 1 {
                    dir_stack.pop();
                }
                channel.data(&[0u8][..]).await?;
            }
            0x01 => {
                let msg = String::from_utf8_lossy(&buf[1..]);
                eprintln!("scp warning: {}", msg.trim());
                buf.clear();
                channel.data(&[0u8][..]).await?;
            }
            0x02 => {
                let msg = String::from_utf8_lossy(&buf[1..]);
                bail!("scp error: {}", msg.trim());
            }
            _ => {
                bail!("unexpected scp header byte: 0x{:02x}", buf[0]);
            }
        }
    }

    channel.eof().await?;
    Ok(())
}

fn parse_c_header(buf: &[u8]) -> anyhow::Result<(u64, String)> {
    let line = std::str::from_utf8(buf)
        .context("invalid utf-8 in scp header")?
        .trim_end_matches('\n');
    let parts: Vec<&str> = line[1..].splitn(3, ' ').collect();
    if parts.len() < 3 {
        bail!("malformed scp header: {line}");
    }
    let size: u64 = parts[1].parse().context("invalid size in scp header")?;
    let name = parts[2].to_string();
    Ok((size, name))
}

async fn download_file_data(
    channel: &mut SshChannel,
    dest: &Path,
    size: u64,
    buf: &mut Vec<u8>,
) -> anyhow::Result<()> {
    let mut remaining = size;
    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("cannot create {}", dest.display()))?;

    while remaining > 0 {
        buf.clear();
        if !recv_data(channel, buf).await? {
            bail!("channel closed during file transfer");
        }
        let take = (remaining as usize).min(buf.len());
        file.write_all(&buf[..take]).await?;
        remaining -= take as u64;

        if take < buf.len() {
            let leftover = buf[take..].to_vec();
            buf.clear();
            buf.extend_from_slice(&leftover);
        } else {
            buf.clear();
        }
    }

    file.flush().await?;

    if buf.is_empty() {
        recv_data(channel, buf).await?;
    }
    if buf.first() == Some(&0) {
        buf.remove(0);
    }

    Ok(())
}
