use log::LevelFilter;
use log::{debug, info, warn};

use std::net::SocketAddr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[cfg(any(target_os = "linux"))]
extern crate libc;

use futures::stream::{self, SelectAll, StreamExt};
use structopt::StructOpt;
use tokio::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpSocket;
use tokio::signal;
use tokio::sync::broadcast;
use tokio::time::{self, sleep, Instant};
use tokio_stream::wrappers::TcpListenerStream;

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

#[cfg(any(target_os = "linux"))]
fn set_ulimit() -> () {
    use std::io;

    unsafe {
        let mut rlim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) == -1 {
            warn!("cannot get maxfiles limit: {}", io::Error::last_os_error());

            return;
        }

        if rlim.rlim_cur < rlim.rlim_max {
            info!(
                "set max open files from {} to {}",
                rlim.rlim_cur, rlim.rlim_max
            );

            rlim.rlim_cur = rlim.rlim_max;
            if libc::setrlimit(libc::RLIMIT_NOFILE, &mut rlim) == -1 {
                warn!("cannot set maxfiles limit: {}", io::Error::last_os_error());

                return;
            }
        } else {
            info!(
                "open files are already set to max allowed {}",
                rlim.rlim_max
            );
        }
    }
}

#[cfg(not(any(target_os = "linux")))]
fn set_ulimit() -> () {
    info!("set open files limit is not supported for anything but linux");
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

    info!(
        "http-tarpit has started; pid: {}, version: {}, delay: {}ms",
        std::process::id(),
        env!("CARGO_PKG_VERSION"),
        delay.as_millis()
    );

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

    // Set limits before drop priv
    set_ulimit();

    #[cfg(all(unix, feature = "drop_privs"))]
    let privdrop_enabled = [
        &opts.privdrop.chroot,
        &opts.privdrop.user,
        &opts.privdrop.group,
    ]
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

    let total_conns: Arc<AtomicI32> = Arc::new(AtomicI32::new(0));
    let num_clients: Arc<AtomicI32> = Arc::new(AtomicI32::new(0));

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
                        let mut shutdown_receiver = shut_send.subscribe();

                        tokio::spawn(async move {
                            let mut buf = vec![0; 1024];
                            let (mut rd, mut wr) = io::split(sock);
                            let interval = time::sleep(delay);
                            tokio::pin!(interval);
                            loop {
                                tokio::select!{
                                    r = rd.read(&mut buf) => {
                                        match r {
                                            Ok(0) => {
                                            // EOF
                                                debug!("disconnect before tarpit, remote: {}, current connections: {}",
                                                peer,
                                                borrowed_num_clients.load(Ordering::Acquire));
                                                break;
                                            },
                                            Ok(n) => {
                                                let next_wakeup = Instant::now() + delay;
                                                debug!("read {} bytes, start tarpitting; end at {:?}",
                                                    n, next_wakeup);
                                                interval.as_mut().reset(next_wakeup);
                                            }
                                            Err(e) => {
                                                warn!("read failed, error: {:?}", e);
                                                break;
                                            }
                                        }
                                    }
                                    _ = &mut interval => {
                                        match wr.write_all(b"HTTP/1.0 200 Delay OK\r\n").await {
                                            Ok(_) => {
                                                info!("disconnect after tarpit, remote: {}, current connections: {}",
                                                    peer,
                                                    borrowed_num_clients.load(Ordering::Acquire));
                                                break;
                                            }
                                            Err(e) => {
                                                info!("write error during tarpig: {:?}", e);
                                                break;
                                            }
                                        }
                                    }
                                    _ = shutdown_receiver.recv() => {
                                        info!("got termination signal, breaking tarpit for {}",
                                                peer);
                                        return;
                                    }
                                }
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
            },
            _ = shut_recv.recv() => {
                // TODO:
                // Allow threads to be terminated. This should be done by
                // using bidirectional channels at some point...
                let wait = Duration::from_millis(100);
                sleep(wait).await;
                break;
            }
        }
    }
    info!(
        "terminating, {} connections served",
        total_conns.load(Ordering::Acquire)
    );
}
