//! A minimal SOCKS5 proxy (RFC 1928, RFC 1929): no authentication or
//! username/password, `CONNECT` only, relaying to wherever it is asked.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

/// A `CONNECT` the proxy relayed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connect {
    /// `host:port` as asked (an IP, or a name for `socks5h`).
    pub target: String,
    /// The username, when the client authenticated.
    pub user: Option<String>,
}

type Log = Arc<Mutex<Vec<Connect>>>;

/// On its own thread and loopback port; shuts down when dropped.
pub struct SocksProxy {
    addr: SocketAddr,
    log: Log,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl SocksProxy {
    /// `auth`: the only username and password accepted; `None` accepts
    /// clients without authentication.
    pub fn start(auth: Option<(&str, &str)>) -> io::Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let log = Log::default();
        let auth = auth.map(|(u, p)| (u.to_owned(), p.to_owned()));
        let (shutdown, stop) = oneshot::channel();
        let thread_log = log.clone();
        let thread = std::thread::spawn(move || {
            runtime.block_on(async move {
                let listener = TcpListener::from_std(listener).expect("adopt listener");
                let accept = async {
                    loop {
                        let Ok((stream, _)) = listener.accept().await else {
                            return;
                        };
                        let (log, auth) = (thread_log.clone(), auth.clone());
                        tokio::spawn(async move {
                            let _ = connection(stream, log, auth).await;
                        });
                    }
                };
                tokio::select! {
                    _ = accept => {}
                    _ = stop => {}
                }
            })
        });
        Ok(Self {
            addr,
            log,
            shutdown: Some(shutdown),
            thread: Some(thread),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn connects(&self) -> Vec<Connect> {
        self.log.lock().unwrap().clone()
    }
}

impl Drop for SocksProxy {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn connection(
    mut client: TcpStream,
    log: Log,
    auth: Option<(String, String)>,
) -> io::Result<()> {
    let bad = || io::Error::from(io::ErrorKind::InvalidData);
    let mut head = [0; 2];
    client.read_exact(&mut head).await?;
    let mut methods = vec![0; head[1] as usize];
    client.read_exact(&mut methods).await?;
    let want = if auth.is_some() { 2 } else { 0 };
    if head[0] != 5 || !methods.contains(&want) {
        client.write_all(&[5, 0xff]).await?;
        return Err(bad());
    }
    client.write_all(&[5, want]).await?;
    let mut user = None;
    if let Some((u, p)) = &auth {
        let mut ver = [0; 1];
        client.read_exact(&mut ver).await?;
        let name = read_string(&mut client).await?;
        let pass = read_string(&mut client).await?;
        let ok = ver[0] == 1 && name == *u && pass == *p;
        client.write_all(&[1, if ok { 0 } else { 1 }]).await?;
        if !ok {
            return Err(bad());
        }
        user = Some(name);
    }
    let mut req = [0; 4];
    client.read_exact(&mut req).await?;
    if req[0] != 5 || req[1] != 1 {
        // Only CONNECT.
        client.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        return Err(bad());
    }
    let host = match req[3] {
        1 => {
            let mut a = [0; 4];
            client.read_exact(&mut a).await?;
            Ipv4Addr::from(a).to_string()
        }
        3 => read_string(&mut client).await?,
        4 => {
            let mut a = [0; 16];
            client.read_exact(&mut a).await?;
            format!("[{}]", Ipv6Addr::from(a))
        }
        _ => return Err(bad()),
    };
    let port = client.read_u16().await?;
    let target = format!("{host}:{port}");
    log.lock().unwrap().push(Connect {
        target: target.clone(),
        user,
    });
    let Ok(mut upstream) = TcpStream::connect(&target).await else {
        client.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        return Ok(());
    };
    client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

async fn read_string(s: &mut TcpStream) -> io::Result<String> {
    let len = s.read_u8().await?;
    let mut buf = vec![0; len as usize];
    s.read_exact(&mut buf).await?;
    String::from_utf8(buf).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))
}
