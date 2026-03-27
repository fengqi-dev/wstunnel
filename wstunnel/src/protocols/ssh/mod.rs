pub mod scp;
pub mod shell;

use crate::config::Client;
use crate::create_client;
use crate::executor::TokioExecutorRef;
use crate::tunnel::{LocalProtocol as TunnelLocalProtocol, RemoteAddr};
use anyhow::{Context, bail};
use russh::{client, keys};
use std::path::Path;
use std::sync::Arc;
use url::Host;
use uuid::Uuid;

pub(crate) struct AcceptAll;
impl client::Handler for AcceptAll {
    type Error = anyhow::Error;
    fn check_server_key(
        &mut self,
        _server_public_key: &keys::ssh_key::PublicKey,
    ) -> impl std::future::Future<Output = Result<bool, Self::Error>> + Send {
        async { Ok(true) }
    }
}

pub(crate) struct SshSession {
    pub session: client::Handle<AcceptAll>,
    pub tunnel_task: tokio::task::JoinHandle<()>,
}

pub(crate) async fn connect_ssh(
    mut client_cfg: Client,
    tunnel: Uuid,
    user: String,
    key: &Path,
    key_passphrase: Option<&str>,
    executor: impl TokioExecutorRef,
) -> anyhow::Result<SshSession> {
    client_cfg.local_to_remote.clear();
    client_cfg.remote_to_local.clear();

    let ws_client = create_client(client_cfg, executor).await?;

    let remote = RemoteAddr {
        protocol: TunnelLocalProtocol::TunnelStdio { proxy_protocol: false },
        host: Host::Domain(tunnel.to_string()),
        port: 0,
    };

    let (ssh_side, tunnel_side) = tokio::io::duplex(64 * 1024);
    let (tunnel_rx, tunnel_tx) = tokio::io::split(tunnel_side);

    let request_id = Uuid::now_v7();
    let client2 = ws_client.clone();
    let tunnel_task = tokio::spawn(async move {
        let _ = client2
            .connect_to_server(request_id, &remote, (tunnel_rx, tunnel_tx))
            .await;
    });

    let cfg = Arc::new(client::Config::default());
    let mut session = client::connect_stream(cfg, ssh_side, AcceptAll).await?;

    let secret = keys::load_secret_key(key, key_passphrase)
        .with_context(|| format!("cannot load ssh private key {}", key.display()))?;
    let secret = Arc::new(secret);
    let hash_alg = if secret.algorithm().is_rsa() {
        session.best_supported_rsa_hash().await?.flatten()
    } else {
        None
    };
    let pk = keys::PrivateKeyWithHashAlg::new(secret, hash_alg);
    let auth = session.authenticate_publickey(user, pk).await?;
    if !auth.success() {
        bail!("ssh authentication failed");
    }

    Ok(SshSession { session, tunnel_task })
}
