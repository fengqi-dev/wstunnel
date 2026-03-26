use crate::protocols::stdio;
use crate::tunnel::{LocalProtocol, RemoteAddr};
use anyhow::{Context, anyhow};
use std::pin::Pin;
use std::task::Poll;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::oneshot;
use tokio_stream::Stream;
use url::Host;

pub struct TunnelStdioTunnelListener<R, W>
where
    R: AsyncRead + Send + 'static,
    W: AsyncWrite + Send + 'static,
{
    listener: Option<(R, W)>,
    dest: (Host, u16),
    proxy_protocol: bool,
}

pub async fn new_tunnelid_listener(
    dest: (Host, u16),
    proxy_protocol: bool,
) -> anyhow::Result<(
    TunnelStdioTunnelListener<impl AsyncRead + Send, impl AsyncWrite + Send>,
    oneshot::Sender<()>,
)> {
    let (listener, handle) = stdio::run_server()
        .await
        .with_context(|| anyhow!("Cannot start STDIO server"))?;

    Ok((
        TunnelStdioTunnelListener {
            listener: Some(listener),
            proxy_protocol,
            dest,
        },
        handle,
    ))
}

impl<R, W> Stream for TunnelStdioTunnelListener<R, W>
where
    R: AsyncRead + Send + 'static,
    W: AsyncWrite + Send + 'static,
{
    type Item = anyhow::Result<((R, W), RemoteAddr)>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<Option<Self::Item>> {
        let this = unsafe { self.get_unchecked_mut() };
        let ret = match this.listener.take() {
            None => None,
            Some(stream) => {
                let (host, port) = this.dest.clone();
                Some(Ok((
                    stream,
                    RemoteAddr {
                        protocol: LocalProtocol::TunnelStdio {
                            proxy_protocol: this.proxy_protocol,
                        },
                        host,
                        port,
                    },
                )))
            }
        };

        Poll::Ready(ret)
    }
}
