use crate::config::Client;
use crate::executor::{TokioExecutor, TokioExecutorRef};
use std::path::PathBuf;
use uuid::Uuid;

use super::connect_ssh;

#[derive(Clone, Debug)]
pub struct SshClientConfig {
    pub client: Client,
    pub tunnel: Uuid,
    pub user: String,
    pub key: PathBuf,
    pub key_passphrase: Option<String>,
    pub term: String,
}

pub async fn run_ssh_client(args: SshClientConfig, executor: impl TokioExecutor) -> anyhow::Result<()> {
    run_ssh_client_impl(args, executor.ref_clone()).await
}

async fn run_ssh_client_impl(args: SshClientConfig, executor: impl TokioExecutorRef) -> anyhow::Result<()> {
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

    use crossterm::terminal;

    let channel = session.channel_open_session().await?;
    terminal::enable_raw_mode()?;
    struct RawModeGuard;
    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
    let _raw_guard = RawModeGuard;

    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    channel
        .request_pty(true, &args.term, cols as u32, rows as u32, 0, 0, &[])
        .await?;
    channel.request_shell(true).await?;

    let stream = channel.into_stream();
    let (mut r, mut w) = tokio::io::split(stream);
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    let mut to_remote = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = [0u8; 1024];
        loop {
            let n = stdin.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            if buf[..n].contains(&0x04) {
                break;
            }
            w.write_all(&buf[..n]).await?;
        }
        let _ = w.shutdown().await;
        Ok::<u64, std::io::Error>(0)
    });
    let mut from_remote = tokio::spawn(async move { tokio::io::copy(&mut r, &mut stdout).await });

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            let _ = session.disconnect(russh::Disconnect::ByApplication, "terminated", "").await;
        }
        _ = &mut to_remote => {
            let _ = session.disconnect(russh::Disconnect::ByApplication, "stdin closed", "").await;
        }
        _ = &mut from_remote => {}
    }

    to_remote.abort();
    from_remote.abort();
    tunnel_task.abort();
    Ok(())
}
