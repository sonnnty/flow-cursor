//! QUIC endpoint construction (TLS 1.3, mutual authentication by pinned
//! fingerprints).

use crate::identity::Identity;
use crate::tls::{ClientAuthorizer, PinnedClientVerifier, PinnedServerVerifier, ServerPin};
use anyhow::{Context, Result};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::rustls;
use quinn::{ClientConfig, Endpoint, ServerConfig, TransportConfig, VarInt};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// Server name used in the TLS handshake (ignored by our verifiers).
pub const SNI: &str = "flowcursor";

#[derive(Clone, Copy, Debug)]
pub struct TransportOpts {
    pub idle_timeout: Duration,
    pub keep_alive: Duration,
}

impl TransportOpts {
    /// Agent ↔ hub: survives short Wi-Fi hiccups.
    pub const HUB: TransportOpts =
        TransportOpts { idle_timeout: Duration::from_secs(10), keep_alive: Duration::from_millis(1500) };
    /// Agent ↔ agent: dead paths are noticed by the link manager much
    /// earlier; this only reclaims the connection.
    pub const P2P: TransportOpts =
        TransportOpts { idle_timeout: Duration::from_secs(4), keep_alive: Duration::from_millis(500) };
}

pub fn transport(opts: TransportOpts) -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    t.max_idle_timeout(Some(VarInt::from_u32(opts.idle_timeout.as_millis() as u32).into()));
    t.keep_alive_interval(Some(opts.keep_alive));
    t.datagram_receive_buffer_size(Some(1 << 20));
    t.datagram_send_buffer_size(1 << 20);
    // Low-latency input: small initial RTT estimate for a LAN.
    t.initial_rtt(Duration::from_millis(10));
    Arc::new(t)
}

pub fn server_config(
    id: &Identity,
    alpn: &[u8],
    auth: Arc<dyn ClientAuthorizer>,
    opts: TransportOpts,
) -> Result<ServerConfig> {
    let mut tls = rustls::ServerConfig::builder_with_provider(crate::tls::provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(PinnedClientVerifier::new(auth))
        .with_single_cert(vec![id.cert.clone()], id.key())
        .context("server certificate")?;
    tls.alpn_protocols = vec![alpn.to_vec()];
    tls.max_early_data_size = 0;
    let mut sc = ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    sc.transport_config(transport(opts));
    Ok(sc)
}

pub fn client_config(id: &Identity, alpn: &[u8], pin: ServerPin, opts: TransportOpts) -> Result<ClientConfig> {
    let mut tls = rustls::ClientConfig::builder_with_provider(crate::tls::provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(PinnedServerVerifier::new(pin))
        .with_client_auth_cert(vec![id.cert.clone()], id.key())
        .context("client certificate")?;
    tls.alpn_protocols = vec![alpn.to_vec()];
    tls.enable_early_data = false;
    let mut cc = ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    cc.transport_config(transport(opts));
    Ok(cc)
}

/// Binds a UDP socket and creates an endpoint (server part optional).
pub fn bind(addr: SocketAddr, server: Option<ServerConfig>) -> Result<Endpoint> {
    let ep = match server {
        Some(sc) => Endpoint::server(sc, addr),
        None => Endpoint::client(addr),
    }
    .with_context(|| format!("binding UDP {addr}"))?;
    Ok(ep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::{peer_fingerprint, AnyClient};
    use flow_proto::Fingerprint;

    #[derive(Debug)]
    struct Only(Fingerprint);
    impl ClientAuthorizer for Only {
        fn allowed(&self, fp: &Fingerprint) -> bool {
            *fp == self.0
        }
    }

    async fn serve(id: &Identity, auth: Arc<dyn ClientAuthorizer>) -> Endpoint {
        let sc = server_config(id, b"t", auth, TransportOpts::P2P).unwrap();
        let ep = bind("127.0.0.1:0".parse().unwrap(), Some(sc)).unwrap();
        let ep2 = ep.clone();
        tokio::spawn(async move {
            while let Some(inc) = ep2.accept().await {
                tokio::spawn(async move {
                    if let Ok(c) = inc.await {
                        let _ = c.closed().await;
                    }
                });
            }
        });
        ep
    }

    async fn dial(server: &Endpoint, client: &Identity, pin: ServerPin) -> Result<quinn::Connection> {
        let ep = bind("127.0.0.1:0".parse().unwrap(), None)?;
        let cc = client_config(client, b"t", pin, TransportOpts::P2P)?;
        Ok(ep.connect_with(cc, server.local_addr()?, SNI)?.await?)
    }

    #[tokio::test]
    async fn mutual_pinning() {
        let srv = Identity::generate("srv").unwrap();
        let cli = Identity::generate("cli").unwrap();
        let mallory = Identity::generate("mallory").unwrap();
        let ep = serve(&srv, Arc::new(Only(cli.fingerprint))).await;

        // Right pins on both sides.
        let c = dial(&ep, &cli, ServerPin::Exact(srv.fingerprint)).await.unwrap();
        assert_eq!(peer_fingerprint(&c), Some(srv.fingerprint));
        // Prefix pin (join token style).
        dial(&ep, &cli, ServerPin::Prefix(srv.fingerprint.0[..16].to_vec())).await.unwrap();
        // Client expects a different server.
        assert!(dial(&ep, &cli, ServerPin::Exact(mallory.fingerprint)).await.is_err());
        // Server does not know this client.
        let r = dial(&ep, &mallory, ServerPin::Exact(srv.fingerprint)).await;
        // With TLS 1.3 the client may only learn about the rejection after
        // its side of the handshake completed: the server then closes it.
        if let Ok(c) = r {
            let closed = tokio::time::timeout(Duration::from_secs(3), c.closed()).await;
            assert!(closed.is_ok(), "server must close a connection with an unknown client cert");
        }

        let any = serve(&srv, Arc::new(AnyClient)).await;
        let c = dial(&any, &mallory, ServerPin::Exact(srv.fingerprint)).await.unwrap();
        assert_eq!(peer_fingerprint(&c), Some(srv.fingerprint));
    }
}
