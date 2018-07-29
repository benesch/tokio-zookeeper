extern crate byteorder;
#[macro_use]
extern crate failure;
#[macro_use]
extern crate futures;
extern crate tokio;
#[macro_use]
extern crate lazy_static;

use futures::sync::oneshot;
use std::borrow::Cow;
use std::net::SocketAddr;
use tokio::prelude::*;

pub mod error;
mod proto;
mod types;

use proto::{WatchType, ZkError};
pub use types::{Acl, CreateMode, KeeperState, Stat, WatchedEvent, WatchedEventType};

#[derive(Clone)]
pub struct ZooKeeper {
    #[allow(dead_code)]
    connection: proto::Enqueuer,
}

impl ZooKeeper {
    pub fn connect(
        addr: &SocketAddr,
    ) -> impl Future<Item = (Self, impl Stream<Item = WatchedEvent, Error = ()>), Error = failure::Error>
    {
        let (tx, rx) = futures::sync::mpsc::unbounded();
        let addr = addr.clone();
        tokio::net::TcpStream::connect(&addr)
            .map_err(failure::Error::from)
            .and_then(move |stream| Self::handshake(addr, stream, tx))
            .map(move |zk| (zk, rx))
    }

    fn handshake(
        addr: SocketAddr,
        stream: tokio::net::TcpStream,
        default_watcher: futures::sync::mpsc::UnboundedSender<WatchedEvent>,
    ) -> impl Future<Item = Self, Error = failure::Error> {
        let request = proto::Request::Connect {
            protocol_version: 0,
            last_zxid_seen: 0,
            timeout: 0,
            session_id: 0,
            passwd: vec![],
            read_only: false,
        };
        eprintln!("about to handshake");

        let enqueuer = proto::Packetizer::new(addr, stream, default_watcher);
        enqueuer.enqueue(request).map(move |response| {
            eprintln!("{:?}", response);
            ZooKeeper {
                connection: enqueuer,
            }
        })
    }

    pub fn create<D, A>(
        self,
        path: &str,
        data: D,
        acl: A,
        mode: CreateMode,
    ) -> impl Future<Item = (Self, Result<String, error::Create>), Error = failure::Error>
    where
        D: Into<Cow<'static, [u8]>>,
        A: Into<Cow<'static, [Acl]>>,
    {
        self.connection
            .enqueue(proto::Request::Create {
                path: path.to_string(),
                data: data.into(),
                acl: acl.into(),
                mode,
            })
            .and_then(move |r| match r {
                Ok(proto::Response::String(s)) => Ok(Ok(s)),
                Ok(r) => bail!("got non-string response to create: {:?}", r),
                Err(ZkError::NoNode) => Ok(Err(error::Create::NoNode)),
                Err(ZkError::NodeExists) => Ok(Err(error::Create::NodeExists)),
                Err(ZkError::InvalidACL) => Ok(Err(error::Create::InvalidAcl)),
                Err(ZkError::NoChildrenForEphemerals) => {
                    Ok(Err(error::Create::NoChildrenForEphemerals))
                }
                Err(e) => Err(format_err!("create call failed: {:?}", e)),
            })
            .map(move |r| (self, r))
    }

    pub fn delete(
        self,
        path: &str,
        version: Option<i32>,
    ) -> impl Future<Item = (Self, Result<(), error::Delete>), Error = failure::Error> {
        let version = version.unwrap_or(-1);
        self.connection
            .enqueue(proto::Request::Delete {
                path: path.to_string(),
                version: version,
            })
            .and_then(move |r| match r {
                Ok(proto::Response::Empty) => Ok(Ok(())),
                Ok(r) => bail!("got non-empty response to delete: {:?}", r),
                Err(ZkError::NoNode) => Ok(Err(error::Delete::NoNode)),
                Err(ZkError::NotEmpty) => Ok(Err(error::Delete::NotEmpty)),
                Err(ZkError::BadVersion) => {
                    Ok(Err(error::Delete::BadVersion { expected: version }))
                }
                Err(e) => Err(format_err!("delete call failed: {:?}", e)),
            })
            .map(move |r| (self, r))
    }
}

impl ZooKeeper {
    pub fn watch(self) -> WatchGlobally {
        WatchGlobally(self)
    }

    pub fn with_watcher(self) -> WithWatcher {
        WithWatcher(self)
    }

    fn exists_w(
        self,
        path: &str,
        watch: Watch,
    ) -> impl Future<Item = (Self, Option<Stat>), Error = failure::Error> {
        self.connection
            .enqueue(proto::Request::Exists {
                path: path.to_string(),
                watch,
            })
            .and_then(|r| match r {
                Ok(proto::Response::Exists { stat }) => Ok(Some(stat)),
                Ok(r) => bail!("got a non-create response to a create request: {:?}", r),
                Err(ZkError::NoNode) => Ok(None),
                Err(e) => bail!("exists call failed: {:?}", e),
            })
            .map(move |r| (self, r))
    }

    pub fn exists(
        self,
        path: &str,
    ) -> impl Future<Item = (Self, Option<Stat>), Error = failure::Error> {
        self.exists_w(path, Watch::None)
    }

    fn get_children_w(
        self,
        path: &str,
        watch: Watch,
    ) -> impl Future<Item = (Self, Option<Vec<String>>), Error = failure::Error> {
        self.connection
            .enqueue(proto::Request::GetChildren {
                path: path.to_string(),
                watch,
            })
            .and_then(|r| match r {
                Ok(proto::Response::Strings(children)) => Ok(Some(children)),
                Ok(r) => bail!("got non-strings response to get-children: {:?}", r),
                Err(ZkError::NoNode) => Ok(None),
                Err(e) => Err(format_err!("get-children call failed: {:?}", e)),
            })
            .map(move |r| (self, r))
    }

    pub fn get_children(
        self,
        path: &str,
    ) -> impl Future<Item = (Self, Option<Vec<String>>), Error = failure::Error> {
        self.get_children_w(path, Watch::None)
    }

    fn get_data_w(
        self,
        path: &str,
        watch: Watch,
    ) -> impl Future<Item = (Self, Option<(Vec<u8>, Stat)>), Error = failure::Error> {
        self.connection
            .enqueue(proto::Request::GetData {
                path: path.to_string(),
                watch,
            })
            .and_then(|r| match r {
                Ok(proto::Response::GetData { bytes, stat }) => Ok(Some((bytes, stat))),
                Ok(r) => bail!("got non-data response to get-data: {:?}", r),
                Err(ZkError::NoNode) => Ok(None),
                Err(e) => Err(format_err!("get-data call failed: {:?}", e)),
            })
            .map(move |r| (self, r))
    }

    pub fn get_data(
        self,
        path: &str,
    ) -> impl Future<Item = (Self, Option<(Vec<u8>, Stat)>), Error = failure::Error> {
        self.get_data_w(path, Watch::None)
    }
}

pub struct WatchGlobally(ZooKeeper);

impl WatchGlobally {
    pub fn exists(
        self,
        path: &str,
    ) -> impl Future<Item = (ZooKeeper, Option<Stat>), Error = failure::Error> {
        self.0.exists_w(path, Watch::Global)
    }

    pub fn get_children(
        self,
        path: &str,
    ) -> impl Future<Item = (ZooKeeper, Option<Vec<String>>), Error = failure::Error> {
        self.0.get_children_w(path, Watch::Global)
    }

    pub fn get_data(
        self,
        path: &str,
    ) -> impl Future<Item = (ZooKeeper, Option<(Vec<u8>, Stat)>), Error = failure::Error> {
        self.0.get_data_w(path, Watch::Global)
    }
}

pub struct WithWatcher(ZooKeeper);

impl WithWatcher {
    pub fn exists(
        self,
        path: &str,
    ) -> impl Future<
        Item = (ZooKeeper, oneshot::Receiver<WatchedEvent>, Option<Stat>),
        Error = failure::Error,
    > {
        let (tx, rx) = oneshot::channel();
        self.0
            .exists_w(path, Watch::Custom(tx))
            .map(|r| (r.0, rx, r.1))
    }

    pub fn get_children(
        self,
        path: &str,
    ) -> impl Future<
        Item = (
            ZooKeeper,
            Option<(oneshot::Receiver<WatchedEvent>, Vec<String>)>,
        ),
        Error = failure::Error,
    > {
        let (tx, rx) = oneshot::channel();
        self.0
            .get_children_w(path, Watch::Custom(tx))
            .map(|r| (r.0, r.1.map(move |c| (rx, c))))
    }

    pub fn get_data(
        self,
        path: &str,
    ) -> impl Future<
        Item = (
            ZooKeeper,
            Option<(oneshot::Receiver<WatchedEvent>, Vec<u8>, Stat)>,
        ),
        Error = failure::Error,
    > {
        let (tx, rx) = oneshot::channel();
        self.0
            .get_data_w(path, Watch::Custom(tx))
            .map(|r| (r.0, r.1.map(move |(b, s)| (rx, b, s))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let mut rt = tokio::runtime::Runtime::new().unwrap();
        let (zk, w): (ZooKeeper, _) =
            rt.block_on(
                ZooKeeper::connect(&"127.0.0.1:2181".parse().unwrap()).and_then(|(zk, w)| {
                    zk.with_watcher()
                        .exists("/foo")
                        .inspect(|(_, _, stat)| assert_eq!(stat, &None))
                        .and_then(|(zk, exists_w, _)| {
                            zk.watch()
                                .exists("/foo")
                                .map(move |(zk, x)| (zk, x, exists_w))
                        })
                        .inspect(|(_, stat, _)| assert_eq!(stat, &None))
                        .and_then(|(zk, _, exists_w)| {
                            zk.create(
                                "/foo",
                                &b"Hello world"[..],
                                Acl::open_unsafe(),
                                CreateMode::Persistent,
                            ).map(move |(zk, x)| (zk, x, exists_w))
                        })
                        .inspect(|(_, ref path, _)| {
                            assert_eq!(path.as_ref().map(String::as_str), Ok("/foo"))
                        })
                        .and_then(move |(zk, _, exists_w)| {
                            exists_w
                                .map(move |w| (zk, w))
                                .map_err(|e| format_err!("exists_w failed: {:?}", e))
                        })
                        .inspect(|(_, event)| {
                            assert_eq!(
                                event,
                                &WatchedEvent {
                                    event_type: WatchedEventType::NodeCreated,
                                    keeper_state: KeeperState::SyncConnected,
                                    path: String::from("/foo"),
                                }
                            );
                        })
                        .and_then(|(zk, _)| zk.watch().exists("/foo"))
                        .inspect(|(_, stat)| {
                            assert_eq!(stat.unwrap().data_length as usize, b"Hello world".len())
                        })
                        .and_then(|(zk, _)| zk.get_data("/foo"))
                        .inspect(|(_, res)| {
                            let data = b"Hello world";
                            let res = res.as_ref().unwrap();
                            assert_eq!(res.0, data);
                            assert_eq!(res.1.data_length as usize, data.len());
                        })
                        .and_then(|(zk, _)| {
                            zk.create(
                                "/foo/bar",
                                &b"Hello bar"[..],
                                Acl::open_unsafe(),
                                CreateMode::Persistent,
                            )
                        })
                        .inspect(|(_, ref path)| {
                            assert_eq!(path.as_ref().map(String::as_str), Ok("/foo/bar"))
                        })
                        .and_then(|(zk, _)| zk.get_children("/foo"))
                        .inspect(|(_, children)| {
                            assert_eq!(children, &Some(vec!["bar".to_string()]));
                        })
                        .and_then(|(zk, _)| zk.get_data("/foo/bar"))
                        .inspect(|(_, res)| {
                            let data = b"Hello bar";
                            let res = res.as_ref().unwrap();
                            assert_eq!(res.0, data);
                            assert_eq!(res.1.data_length as usize, data.len());
                        })
                        .and_then(|(zk, _)| zk.delete("/foo", None))
                        .inspect(|(_, res)| assert_eq!(res, &Err(error::Delete::NotEmpty)))
                        .and_then(|(zk, _)| zk.delete("/foo/bar", None))
                        .inspect(|(_, res)| assert_eq!(res, &Ok(())))
                        .and_then(|(zk, _)| zk.delete("/foo", None))
                        .inspect(|(_, res)| assert_eq!(res, &Ok(())))
                        .and_then(|(zk, _)| zk.watch().exists("/foo"))
                        .inspect(|(_, stat)| assert_eq!(stat, &None))
                        .and_then(move |(zk, _)| {
                            w.into_future()
                                .map(move |x| (zk, x))
                                .map_err(|e| format_err!("stream error: {:?}", e.0))
                        })
                        .inspect(|(_, (event, _))| {
                            assert_eq!(
                                event,
                                &Some(WatchedEvent {
                                    event_type: WatchedEventType::NodeCreated,
                                    keeper_state: KeeperState::SyncConnected,
                                    path: String::from("/foo"),
                                })
                            );
                        })
                        .and_then(|(zk, (_, w))| {
                            w.into_future()
                                .map(move |x| (zk, x))
                                .map_err(|e| format_err!("stream error: {:?}", e.0))
                        })
                        .inspect(|(_, (event, _))| {
                            assert_eq!(
                                event,
                                &Some(WatchedEvent {
                                    event_type: WatchedEventType::NodeDeleted,
                                    keeper_state: KeeperState::SyncConnected,
                                    path: String::from("/foo"),
                                })
                            );
                        })
                        .map(|(zk, (_, w))| (zk, w))
                }),
            ).unwrap();

        eprintln!("got through all futures");
        drop(zk); // make Packetizer idle
        rt.shutdown_on_idle().wait().unwrap();
        assert_eq!(w.wait().count(), 0);
    }
}
