use anyhow::{Context, anyhow};
use http_body_util::BodyExt;
use hyper::header::AUTHORIZATION;
use hyper::{Request, Uri};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde::Deserialize;
use std::net::IpAddr;
use url::Host;
use url::Url;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
struct TunnelEndpointResponse {
    host: String,
    port: u16,
}

pub async fn resolve_tunnel_over_http(
    base: &Url,
    id: Uuid,
    authorization: Option<&str>,
) -> anyhow::Result<(Host, u16)> {
    if base.scheme() != "http" {
        return Err(anyhow!("tunnel resolver only supports http:// for now, got {}", base.scheme()));
    }

    let mut url = base.clone();
    let id_str = id.to_string();
    url.path_segments_mut()
        .map_err(|_| anyhow!("cannot append path to resolver base url {base}"))?
        .push(&id_str);
    let uri: Uri = url
        .as_str()
        .parse()
        .with_context(|| format!("invalid resolver uri {url}"))?;

    let client: Client<HttpConnector, http_body_util::Empty<bytes::Bytes>> =
        Client::builder(TokioExecutor::new()).build_http();
    let req = if let Some(auth) = authorization.filter(|s| !s.is_empty()) {
        Request::builder().method("GET").uri(uri).header(AUTHORIZATION, auth)
    } else {
        Request::builder().method("GET").uri(uri)
    }
    .body(http_body_util::Empty::<bytes::Bytes>::new())
    .expect("failed to build resolver request");

    let resp = client
        .request(req)
        .await
        .with_context(|| format!("resolver request failed for tunnel {id}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.into_body().collect().await?.to_bytes();
        return Err(anyhow!(
            "resolver rejected tunnel {id}: status {} body {}",
            status,
            String::from_utf8_lossy(&body)
        ));
    }

    let body = resp.into_body().collect().await?.to_bytes();
    let payload = String::from_utf8(body.to_vec()).unwrap_or_default();
    let parsed: TunnelEndpointResponse = serde_yaml::from_str(&payload)
        .with_context(|| format!("cannot parse resolver response for tunnel {id} as YAML"))?;

    let host = parse_host(&parsed.host)
        .with_context(|| format!("invalid host '{}' from resolver for tunnel {id}", parsed.host))?;
    Ok((host, parsed.port))
}

fn parse_host(host: &str) -> anyhow::Result<Host> {
    let host = host.trim();
    if host.is_empty() {
        return Err(anyhow!("host is empty"));
    }
    match host.parse::<IpAddr>() {
        Ok(ip) => Ok(match ip {
            std::net::IpAddr::V4(v4) => Host::Ipv4(v4),
            std::net::IpAddr::V6(v6) => Host::Ipv6(v6),
        }),
        Err(_) => Ok(Host::Domain(host.to_string())),
    }
}
