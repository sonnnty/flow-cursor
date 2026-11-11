//! Helpers for tests and the latency bench: a UDP forwarder that can be
//! switched to drop everything, to simulate a blocked direct path.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::net::UdpSocket;

pub struct UdpProxy {
    pub addr: SocketAddr,
    blocked: Arc<AtomicBool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl UdpProxy {
    /// Forwards datagrams between one client and `target`.
    pub async fn start(target: SocketAddr) -> std::io::Result<Self> {
        let front = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let back = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        back.connect(target).await?;
        let addr = front.local_addr()?;
        let blocked = Arc::new(AtomicBool::new(false));
        let client: Arc<tokio::sync::Mutex<Option<SocketAddr>>> = Arc::default();
        let (f, b, bl, cl) = (front.clone(), back.clone(), blocked.clone(), client.clone());
        let t1 = tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            while let Ok((n, from)) = f.recv_from(&mut buf).await {
                *cl.lock().await = Some(from);
                if !bl.load(Ordering::SeqCst) {
                    let _ = b.send(&buf[..n]).await;
                }
            }
        });
        let (f, b, bl, cl) = (front, back, blocked.clone(), client);
        let t2 = tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            while let Ok(n) = b.recv(&mut buf).await {
                let to = *cl.lock().await;
                if let (Some(to), false) = (to, bl.load(Ordering::SeqCst)) {
                    let _ = f.send_to(&buf[..n], to).await;
                }
            }
        });
        Ok(UdpProxy { addr, blocked, tasks: vec![t1, t2] })
    }

    pub fn set_blocked(&self, b: bool) {
        self.blocked.store(b, Ordering::SeqCst);
    }
}

impl Drop for UdpProxy {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}
