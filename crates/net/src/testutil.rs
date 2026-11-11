//! Helpers for tests and the latency bench: a UDP forwarder that can be
//! switched to drop everything, to simulate a blocked direct path.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

pub struct UdpProxy {
    pub addr: SocketAddr,
    blocked: Arc<AtomicBool>,
    tasks: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl UdpProxy {
    /// Forwards datagrams between any number of clients and `target`
    /// (one upstream socket per client, like a NAT).
    pub async fn start(target: SocketAddr) -> std::io::Result<Self> {
        let front = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let addr = front.local_addr()?;
        let blocked = Arc::new(AtomicBool::new(false));
        let tasks: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>> = Arc::default();
        let backs: Arc<Mutex<HashMap<SocketAddr, Arc<UdpSocket>>>> = Arc::default();
        let (f, bl, ts) = (front.clone(), blocked.clone(), tasks.clone());
        let main = tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            while let Ok((n, client)) = f.recv_from(&mut buf).await {
                let back = {
                    let mut map = backs.lock().await;
                    match map.get(&client) {
                        Some(b) => b.clone(),
                        None => {
                            let Ok(b) = UdpSocket::bind("127.0.0.1:0").await else { continue };
                            if b.connect(target).await.is_err() {
                                continue;
                            }
                            let b = Arc::new(b);
                            map.insert(client, b.clone());
                            let (b2, f2, bl2) = (b.clone(), f.clone(), bl.clone());
                            let t = tokio::spawn(async move {
                                let mut buf = vec![0u8; 65536];
                                while let Ok(n) = b2.recv(&mut buf).await {
                                    if !bl2.load(Ordering::SeqCst) {
                                        let _ = f2.send_to(&buf[..n], client).await;
                                    }
                                }
                            });
                            ts.lock().expect("poisoned").push(t);
                            b
                        }
                    }
                };
                if !bl.load(Ordering::SeqCst) {
                    let _ = back.send(&buf[..n]).await;
                }
            }
        });
        tasks.lock().expect("poisoned").push(main);
        Ok(UdpProxy { addr, blocked, tasks })
    }

    pub fn set_blocked(&self, b: bool) {
        self.blocked.store(b, Ordering::SeqCst);
    }
}

impl Drop for UdpProxy {
    fn drop(&mut self) {
        for t in self.tasks.lock().expect("poisoned").iter() {
            t.abort();
        }
    }
}

/// Finds a currently free UDP port on 127.0.0.1.
pub fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0").and_then(|s| s.local_addr()).map(|a| a.port()).unwrap_or(0)
}
