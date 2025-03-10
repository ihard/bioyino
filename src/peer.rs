use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use capnp;
use capnp::message::{Builder, ReaderOptions};
use capnp_futures::ReadStream;
use failure_derive::Fail;
use futures::future::{err, join_all, Either, Future, IntoFuture};
use futures::sync::mpsc::Sender;
use futures::sync::oneshot;
use futures::{Sink, Stream};
use slog::{debug, error as log_error, o, warn, Logger};
use tokio::executor::current_thread::spawn;
use tokio::net::TcpStream;
use tokio::timer::Interval;

use bioyino_metric::protocol_capnp::{message as cmsg, message::Builder as CBuilder};
use bioyino_metric::{Metric, MetricError};

use crate::task::Task;
use crate::util::{bound_stream, reusing_listener, try_resolve, BackoffRetryBuilder};
use crate::{Cache, Float, PEER_ERRORS};

const CAPNP_READER_OPTIONS: ReaderOptions = ReaderOptions { traversal_limit_in_words: 8 * 1024 * 1024 * 1024, nesting_limit: 16 };

#[derive(Fail, Debug)]
pub enum PeerError {
    #[fail(display = "I/O error: {}", _0)]
    Io(#[cause] ::std::io::Error),

    #[fail(display = "Error when creating timer: {}", _0)]
    Timer(#[cause] ::tokio::timer::Error),

    #[fail(display = "error sending task to worker thread")]
    TaskSend,

    #[fail(display = "server received incorrect message")]
    BadMessage,

    #[fail(display = "bad command")]
    BadCommand,

    #[fail(display = "response not sent")]
    Response,

    #[fail(display = "decoding capnp failed: {}", _0)]
    Capnp(capnp::Error),

    #[fail(display = "decoding capnp schema failed: {}", _0)]
    CapnpSchema(capnp::NotInSchema),

    #[fail(display = "decoding metric failed: {}", _0)]
    Metric(MetricError),
}

#[derive(Clone, Debug)]
pub struct NativeProtocolServer {
    log: Logger,
    listen: SocketAddr,
    chans: Vec<Sender<Task>>,
}

impl NativeProtocolServer {
    pub fn new(log: Logger, listen: SocketAddr, chans: Vec<Sender<Task>>) -> Self {
        Self { log: log.new(o!("source"=>"canproto-peer-server", "ip"=>format!("{}", listen.clone()))), listen, chans }
    }
}

impl IntoFuture for NativeProtocolServer {
    type Item = ();
    type Error = PeerError;
    type Future = Box<Future<Item = Self::Item, Error = Self::Error>>;

    fn into_future(self) -> Self::Future {
        let Self { log, listen, chans } = self;
        let serv_log = log.clone();

        let listener = match reusing_listener(&listen) {
            Ok(l) => l,
            Err(e) => {
                return Box::new(err(PeerError::Io(e)));
            }
        };

        let future = listener
            .incoming()
            .map_err(|e| PeerError::Io(e))
            .for_each(move |conn| {
                let peer_addr = conn.peer_addr().map(|addr| addr.to_string()).unwrap_or("[UNCONNECTED]".into());
                let transport = ReadStream::new(conn, CAPNP_READER_OPTIONS);

                let log = log.new(o!("remote"=>peer_addr));
                let elog = log.clone();

                let chans = chans.clone();
                let mut chans = chans.into_iter().cycle();

                let receiver = transport
                    .then(move |reader| {
                        // decode incoming capnp data into message
                        // FIXME unwraps
                        let reader = reader.map_err(PeerError::Capnp)?;
                        let reader = reader.get_root::<cmsg::Reader>().map_err(PeerError::Capnp)?;
                        let next_chan = chans.next().unwrap();
                        parse_and_send(reader, next_chan, log.clone()).map_err(|e| {
                            warn!(log, "bad incoming message"; "error" => e.to_string());
                            PeerError::Metric(e)
                        })
                    })
                .map_err(move |e| {
                    warn!(elog, "snapshot server client error"; "error"=>format!("{:?}", e));
                })
                .for_each(|_| {
                    // Consume all messages from the stream
                    Ok(())
                });
                spawn(receiver);
                Ok(())
            })
        .map_err(move |e| {
            log_error!(serv_log, "snapshot server gone with error"; "error"=>format!("{:?}", e));
            e
        });
        Box::new(future)
    }
}

fn parse_and_send(reader: cmsg::Reader, next_chan: Sender<Task>, log: Logger) -> Result<(), MetricError> {
    match reader.which().map_err(MetricError::CapnpSchema)? {
        cmsg::Single(reader) => {
            let reader = reader.map_err(MetricError::Capnp)?;
            let (name, metric) = Metric::<Float>::from_capnp(reader)?;
            let future = next_chan
                .send(Task::AddMetric(name, metric))
                .map(|_| ()) // drop next sender
                .map_err(|_| PeerError::TaskSend);
            let elog = log.clone();
            spawn(future.map_err(move |e| {
                warn!(elog, "error joining snapshot: {:?}", e);
            }));
            Ok(())
        }
        cmsg::Multi(reader) => {
            let reader = reader.map_err(MetricError::Capnp)?;
            let mut metrics = Vec::new();
            reader.iter().map(|reader| Metric::<Float>::from_capnp(reader).map(|(name, metric)| metrics.push((name, metric)))).last();
            let future = next_chan
                .send(Task::AddMetrics(metrics))
                .map(|_| ()) // drop next sender
                .map_err(|_| PeerError::TaskSend);
            let elog = log.clone();
            spawn(future.map_err(move |e| {
                warn!(elog, "error joining snapshot: {:?}", e);
            }));
            Ok(())
        }
        cmsg::Snapshot(reader) => {
            let reader = reader.map_err(MetricError::Capnp)?;
            let mut metrics = Vec::new();
            reader.iter().map(|reader| Metric::<Float>::from_capnp(reader).map(|(name, metric)| metrics.push((name, metric)))).last();
            let future = next_chan
                .send(Task::AddSnapshot(metrics))
                .map(|_| ()) // drop next sender
                .map_err(|_| PeerError::TaskSend);
            let elog = log.clone();
            spawn(future.map_err(move |e| {
                warn!(elog, "error joining snapshot: {:?}", e);
            }));
            Ok(())
        }
    }
}

pub struct NativeProtocolSnapshot {
    nodes: Vec<SocketAddr>,
    client_bind: Option<SocketAddr>,
    interval: Duration,
    chans: Vec<Sender<Task>>,
    log: Logger,
}

impl NativeProtocolSnapshot {
    pub fn new(log: &Logger, nodes: Vec<String>, client_bind: Option<SocketAddr>, interval: Duration, chans: &Vec<Sender<Task>>) -> Self {
        let nodes = nodes.into_iter().map(|node| try_resolve(&node)).collect::<Vec<_>>();
        Self { log: log.new(o!("source"=>"peer-client")), nodes, client_bind, interval, chans: chans.clone() }
    }
}

impl IntoFuture for NativeProtocolSnapshot {
    type Item = ();
    type Error = PeerError;
    type Future = Box<Future<Item = Self::Item, Error = Self::Error>>;

    fn into_future(self) -> Self::Future {
        let Self { log, nodes, client_bind, interval, chans } = self;

        let timer = Interval::new(Instant::now() + interval, interval);
        let future = timer.map_err(|e| PeerError::Timer(e)).for_each(move |_| {
            let chans = chans.clone();
            let nodes = nodes.clone();

            let metrics = chans
                .into_iter()
                .map(|chan| {
                    let (tx, rx) = oneshot::channel();
                    spawn(chan.send(Task::TakeSnapshot(tx)).then(|_| Ok(())));
                    rx
                })
            .collect::<Vec<_>>();

            let get_metrics = join_all(metrics)
                .map_err(|_| {
                    PEER_ERRORS.fetch_add(1, Ordering::Relaxed);
                    PeerError::TaskSend
                })
            .and_then(move |mut metrics| {
                metrics.retain(|m| m.len() > 0);
                Ok(Arc::new(metrics))
            });

            // All nodes have to receive the same metrics
            // so we don't parallel connections and metrics fetching
            let log = log.clone();
            get_metrics.and_then(move |metrics| {
                nodes
                    .into_iter()
                    .map(move |address| {
                        let metrics = metrics.clone();
                        let log = log.clone();
                        let peer_client_ret = BackoffRetryBuilder { delay: 500, delay_mul: 2f32, delay_max: 5000, retries: 3 };
                        let options = SnapshotClientOptions { address: address, bind: client_bind };
                        let client = SnapshotSender::new(metrics, options, log.clone());
                        spawn(peer_client_ret.spawn(client).map_err(move |e| {
                            warn!(log, "snapshot client removed after giving up trying"; "error"=>format!("{:?}", e));
                        }));
                    })
                .last();
                Ok(())
            })
        });
        Box::new(future)
    }
}

#[derive(Clone)]
pub struct SnapshotClientOptions {
    address: SocketAddr,
    bind: Option<SocketAddr>,
}

#[derive(Clone)]
pub struct SnapshotSender {
    metrics: Arc<Vec<Cache>>,
    options: SnapshotClientOptions,
    log: Logger,
}

impl SnapshotSender {
    pub fn new(metrics: Arc<Vec<Cache>>, options: SnapshotClientOptions, log: Logger) -> Self {
        Self { metrics, options, log }
    }
}

impl IntoFuture for SnapshotSender {
    type Item = ();
    type Error = PeerError;
    type Future = Box<Future<Item = Self::Item, Error = Self::Error>>;

    fn into_future(self) -> Self::Future {
        let Self { metrics, log, options } = self;
        let elog = log.clone();
        let stream_future = match options.bind {
            Some(bind_addr) => match bound_stream(&bind_addr) {
                Ok(std_stream) => Either::A(TcpStream::connect_std(std_stream, &options.address, &tokio::reactor::Handle::default())),
                Err(e) => Either::B(err(e)),
            },
            None => Either::A(TcpStream::connect(&options.address)),
        };

        let sender = stream_future
            .map_err(|e| PeerError::Io(e))
            .and_then(move |conn| {
                let codec = ::capnp_futures::serialize::Transport::new(conn, CAPNP_READER_OPTIONS);

                let mut snapshot_message = Builder::new_default();
                {
                    let builder = snapshot_message.init_root::<CBuilder>();
                    let flat_len = metrics.iter().flat_map(|hmap| hmap.iter()).count();
                    let mut multi_metric = builder.init_snapshot(flat_len as u32);
                    metrics
                        .iter()
                        .flat_map(|hmap| hmap.into_iter())
                        .enumerate()
                        .map(|(idx, (name, metric))| {
                            let mut c_metric = multi_metric.reborrow().get(idx as u32);
                            let name = unsafe { ::std::str::from_utf8_unchecked(&name) };
                            c_metric.set_name(name);
                            metric.fill_capnp(&mut c_metric);
                        })
                    .last();
                }
                codec.send(snapshot_message).map(|_| ()).map_err(move |e| {
                    debug!(log, "codec error"; "error"=>e.to_string());
                    PeerError::Capnp(e)
                })
            })
        .map_err(move |e| {
            PEER_ERRORS.fetch_add(1, Ordering::Relaxed);
            debug!(elog, "error sending snapshot: {}", e);
            e
        });
        Box::new(sender)
    }
}

#[cfg(test)]
mod test {

    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use bytes::Bytes;
    use capnp::message::Builder;
    use futures::sync::mpsc::{self, Receiver};
    use slog::Logger;
    use tokio::runtime::current_thread::Runtime;
    use tokio::timer::Delay;

    use metric::{Metric, MetricType};

    use crate::config::System;
    use crate::task::TaskRunner;
    use crate::util::prepare_log;

    use super::*;

    fn prepare_runtime_with_server(log: Logger) -> (Runtime, Receiver<Task>, SocketAddr) {
        let mut chans = Vec::new();
        let (tx, rx) = mpsc::channel(5);
        chans.push(tx);

        let address: ::std::net::SocketAddr = "127.0.0.1:8136".parse().unwrap();
        let mut runtime = Runtime::new().expect("creating runtime for main thread");

        let c_peer_listen = address.clone();
        let c_serv_log = log.clone();
        let peer_server = NativeProtocolServer::new(log.clone(), c_peer_listen, chans).into_future().map_err(move |e| {
            warn!(c_serv_log, "shot server gone with error: {:?}", e);
            panic!("shot server");
        });
        runtime.spawn(peer_server);

        (runtime, rx, address)
    }

    #[test]
    fn test_peer_protocol_capnp() {
        let test_timeout = Instant::now() + Duration::from_secs(3);

        let log = prepare_log("test_peer_capnp");

        let mut config = System::default();
        config.metrics.log_parse_errors = true;
        let runner = TaskRunner::new(log.clone(), Arc::new(config), 16);

        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let ts = ts.as_secs() as u64;

        let outmetric = Metric::new(42f64, MetricType::Gauge(None), ts.into(), None).unwrap();

        let metric = outmetric.clone();
        let (mut runtime, rx, address) = prepare_runtime_with_server(log.clone());

        let future = rx
            .fold(runner, move |mut runner, task: Task| {
                runner.run(task);
                Ok(runner)
            })
        .and_then(move |runner| {
            let single_name: Bytes = "complex.test.bioyino_single".into();
            let multi_name: Bytes = "complex.test.bioyino_multi".into();
            let shot_name: Bytes = "complex.test.bioyino_snapshot".into();
            assert_eq!(runner.get_long_entry(&shot_name), Some(&outmetric));
            assert_eq!(runner.get_short_entry(&single_name), Some(&outmetric));
            assert_eq!(runner.get_short_entry(&multi_name), Some(&outmetric));

            Ok(())
        })
        .map_err(|_| panic!("error in the future"));
        runtime.spawn(future);

        let sender = TcpStream::connect(&address)
            .map_err(|e| {
                panic!("connection err: {:?}", e);
            })
        .and_then(move |conn| {
            let codec = ::capnp_futures::serialize::Transport::new(conn, CAPNP_READER_OPTIONS);

            let mut single_message = Builder::new_default();
            {
                let builder = single_message.init_root::<CBuilder>();
                let mut c_metric = builder.init_single();
                c_metric.set_name("complex.test.bioyino_single");
                metric.fill_capnp(&mut c_metric);
            }

            let mut multi_message = Builder::new_default();
            {
                let builder = multi_message.init_root::<CBuilder>();
                let multi_metric = builder.init_multi(1);
                let mut new_metric = multi_metric.get(0);
                new_metric.set_name("complex.test.bioyino_multi");
                metric.fill_capnp(&mut new_metric);
            }

            let mut snapshot_message = Builder::new_default();
            {
                let builder = snapshot_message.init_root::<CBuilder>();
                let multi_metric = builder.init_snapshot(1);
                let mut new_metric = multi_metric.get(0);
                new_metric.set_name("complex.test.bioyino_snapshot");
                metric.fill_capnp(&mut new_metric);
            }
            codec.send(single_message).and_then(|codec| codec.send(multi_message).and_then(|codec| codec.send(snapshot_message))).map(|_| ()).map_err(|e| println!("codec error: {:?}", e))
        })
        .map_err(move |e| {
            debug!(log, "error sending snapshot: {:?}", e);
            panic!("failed sending snapshot");
        });

        let d = Delay::new(Instant::now() + Duration::from_secs(1));
        let delayed = d.map_err(|_| ()).and_then(|_| sender);
        runtime.spawn(delayed);

        let test_delay = Delay::new(test_timeout);
        runtime.block_on(test_delay).expect("runtime");
    }
}
