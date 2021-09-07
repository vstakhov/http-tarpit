use log::{warn, info};
use log::LevelFilter;

use std::net::SocketAddr;
use std::time::Duration;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

use structopt::StructOpt;
use futures::stream::{self, SelectAll, StreamExt};
use tokio::net::{TcpSocket};
use tokio::time::{sleep, interval};
use tokio_stream::wrappers::{TcpListenerStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::io;
use tokio::signal;
use tokio::sync::broadcast;

#[cfg(all(unix, feature = "drop_privs"))]
use privdrop::PrivDrop;

#[derive(Debug, StructOpt)]
#[structopt(name = "http-tarpit", about = "HTTP tarpit endpoint")]
struct Config {
    /// Listen address(es) to bind to
    #[structopt(short = "l", long = "listen", default_value = "0.0.0.0:4040")]
    listen_addrs: Vec<SocketAddr>,
    /// Seconds between reponse
    #[structopt(short = "d", long = "delay", default_value = "60.0")]
    delay: f32,
    /// Verbose level (repeat for more verbosity)
    #[structopt(short = "v", long = "verbose", parse(from_occurrences))]
    verbose: u8,
    #[cfg(all(unix, feature = "drop_privs"))]
    #[structopt(flatten)]
    #[cfg(all(unix, feature = "drop_privs"))]
    privdrop: PrivDropConfig,
}

#[cfg(all(unix, feature = "drop_privs"))]
#[derive(Debug, StructOpt)]
struct PrivDropConfig {
    /// Run as this user and their primary group
    #[structopt(short = "u", long = "user")]
    user: Option<String>,
    /// Run as this group
    #[structopt(short = "g", long = "group")]
    group: Option<String>,
    /// Chroot to this directory
    #[structopt(long = "chroot")]
    chroot: Option<String>,
}

async fn listen_socket(addr: SocketAddr) -> std::io::Result<TcpListenerStream> {
    let sock = match addr {
        SocketAddr::V4(_) => TcpSocket::new_v4()?,
        SocketAddr::V6(_) => TcpSocket::new_v6()?,
    };
    info!("listen on {}", addr.to_string());
    sock.set_reuseaddr(true)?;
    sock.bind(addr)?;
    sock.listen(128).map(TcpListenerStream::new)
}



#[tokio::main(flavor = "current_thread")]
async fn main() {
    let opts = Config::from_args();
    let delay = Duration::from_secs_f32(f32::from(opts.delay));
    let (shut_send, mut shut_recv) = broadcast::channel(4);

    let log_level = match opts.verbose {
        0 => LevelFilter::Off,
        1 => LevelFilter::Info,
        2 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    };
    env_logger::Builder::from_default_env()
        .filter(None, log_level)
        .format_timestamp(Some(env_logger::fmt::TimestampPrecision::Millis))
        .init();

    info!("http-tarpit has started; pid: {}, version: {}, delay: {}ms",
        std::process::id(),
        env!("CARGO_PKG_VERSION"),
        delay.as_millis());

    let mut listeners = stream::iter(opts.listen_addrs.iter())
        .then(|addr| async move {
            match listen_socket(*addr).await {
                Ok(listener) => {
                    info!("listen on addr: {}", addr);
                    listener
                }
                Err(err) => {
                    panic!("cannot listen on addr: {}, error: {}", addr, err);
                }
            }
        })
        .collect::<SelectAll<_>>()
        .await;

    #[cfg(all(unix, feature = "drop_privs"))]
        let privdrop_enabled = [&opts.privdrop.chroot, &opts.privdrop.user, &opts.privdrop.group]
        .iter()
        .any(|o| o.is_some());
    if privdrop_enabled {
        let mut pd = PrivDrop::default();
        if let Some(path) = opts.privdrop.chroot {
            info!("chroot: {}", path);
            pd = pd.chroot(path);
        }

        if let Some(user) = opts.privdrop.user {
            info!("setuid user: {}", user);
            pd = pd.user(user);
        }

        if let Some(group) = opts.privdrop.group {
            info!("setgid group: {}", group);
            pd = pd.group(group);
        }

        pd.apply()
            .unwrap_or_else(|e| panic!("Failed to drop privileges: {}", e));

        info!("dropped privs");
    }

    let total_conns : Arc<AtomicI32> = Arc::new(AtomicI32::new(0));
    let num_clients : Arc<AtomicI32> = Arc::new(AtomicI32::new(0));

    loop {
        tokio::select! {
            Some(client) = listeners.next() => {
                match client {
                    Ok(sock) => {
                        let peer = match sock.peer_addr() {
                            Ok(peer) => peer,
                            Err(e) => {
                                warn!("getpeername failed, error: {:?}", e);
                                continue;
                            }
                        };
                        num_clients.fetch_add(1, Ordering::Release);
                        total_conns.fetch_add(1, Ordering::Release);

                        info!("connect, remote: {}, current connections: {}", peer,
                            num_clients.load(Ordering::Acquire));

                        let borrowed_num_clients = num_clients.clone();
                        tokio::spawn(async move {
                            let mut buf = vec![0; 1024];
                            let (mut rd, mut wr) = io::split(sock);
                            loop {
                                match rd.read(&mut buf).await {
                                    Ok(0) => {
                                        // EOF
                                        info!("disconnect before tarpit, remote: {}, current connections: {}",
                                            peer,
                                            borrowed_num_clients.load(Ordering::Acquire));
                                        break;
                                    },
                                    Ok(n) => {
                                        info!("read {} bytes, start tarpitting", n);
                                        let mut interval = interval(delay);
                                        interval.tick().await;
                                        interval.tick().await;
                                        match wr.write_all(b"HTTP/1.0 200 Delay OK\r\n").await {
                                            Ok(_) => {
                                                info!("disconnect after tarpit, remote: {}, current connections: {}",
                                                    peer,
                                                    borrowed_num_clients.load(Ordering::Acquire));
                                            }
                                            Err(e) => {
                                                info!("write error during tarpig: {:?}", e);
                                            }
                                        }
                                        break;
                                    }
                                    Err(e) => {
                                        warn!("read failed, error: {:?}", e);
                                        break;
                                    }
                                };
                            }
                            borrowed_num_clients.fetch_sub(1, Ordering::Release);
                        });
                    }
                    Err(err) => match err.kind() {
                        std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::ConnectionReset => (),
                        _ => {
                            let wait = Duration::from_millis(100);
                            warn!("accept, err: {}, wait: {:?}", err, wait);
                            sleep(wait).await;
                        }
                    },
                }
            }
            _ = signal::ctrl_c() => {
                info!("SIGINT received, send termination to {} threads",
                    num_clients.load(Ordering::Acquire));
                shut_send.send(1).unwrap();
                break;
            },
        }
    }
    info!("terminating, {} connections served", total_conns.load(Ordering::Acquire));
}
