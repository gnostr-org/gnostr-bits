use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    io::{BufReader, BufWriter, Read},
    net::SocketAddr,
    path::PathBuf,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use anyhow::{bail, Context};
use bencode::{bencode_serialize_to_writer, BencodeDeserializer};
use buffers::{ByteBufT, ByteString};
use clone_to_owned::CloneToOwned;
use dht::{
    Dht, DhtBuilder, DhtConfig, Id20, PersistentDht, PersistentDhtConfig, RequestPeersStream,
};
use futures::{stream::FuturesUnordered, StreamExt, TryFutureExt};
use librqbit_core::{
    directories::get_configuration_directory,
    magnet::Magnet,
    peer_id::generate_peer_id,
    spawn_utils::spawn_with_cancel,
    torrent_metainfo::{torrent_from_bytes, TorrentMetaV1Info, TorrentMetaV1Owned},
};
use parking_lot::RwLock;
use peer_binary_protocol::{Handshake, PIECE_MESSAGE_DEFAULT_LEN};
use reqwest::Url;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_with::serde_as;
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, error_span, info, trace, warn, Instrument};

use crate::{
    dht_utils::{read_metainfo_from_peer_receiver, ReadMetainfoResult},
    peer_connection::{with_timeout, PeerConnectionOptions},
    spawn_utils::BlockingSpawner,
    torrent_state::{
        ManagedTorrentBuilder, ManagedTorrentHandle, ManagedTorrentState, TorrentStateLive,
    },
};

pub const SUPPORTED_SCHEMES: [&str; 3] = ["http:", "https:", "magnet:"];

pub type TorrentId = usize;

#[derive(Default)]
pub struct SessionDatabase {
    next_id: TorrentId,
    torrents: HashMap<TorrentId, ManagedTorrentHandle>,
}

impl SessionDatabase {
    fn add_torrent(
        &mut self,
        torrent: ManagedTorrentHandle,
        preferred_id: Option<TorrentId>,
    ) -> TorrentId {
        match preferred_id {
            Some(id) if self.torrents.contains_key(&id) => {
                warn!("id {id} already present in DB, ignoring \"preferred_id\" parameter");
            }
            Some(id) => {
                self.torrents.insert(id, torrent);
                self.next_id = id.max(self.next_id).wrapping_add(1);
                return id;
            }
            _ => {}
        }
        let idx = self.next_id;
        self.torrents.insert(idx, torrent);
        self.next_id += 1;
        idx
    }

    fn serialize(&self) -> SerializedSessionDatabase {
        SerializedSessionDatabase {
            torrents: self
                .torrents
                .iter()
                .map(|(id, torrent)| {
                    (
                        *id,
                        SerializedTorrent {
                            trackers: torrent
                                .info()
                                .trackers
                                .iter()
                                .map(|u| u.to_string())
                                .collect(),
                            info_hash: torrent.info_hash().as_string(),
                            info: torrent.info().info.clone(),
                            only_files: torrent.only_files.clone(),
                            is_paused: torrent
                                .with_state(|s| matches!(s, ManagedTorrentState::Paused(_))),
                            output_folder: torrent.info().out_dir.clone(),
                        },
                    )
                })
                .collect(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct SerializedTorrent {
    info_hash: String,
    #[serde(
        serialize_with = "serialize_torrent",
        deserialize_with = "deserialize_torrent"
    )]
    info: TorrentMetaV1Info<ByteString>,
    trackers: HashSet<String>,
    output_folder: PathBuf,
    only_files: Option<Vec<usize>>,
    is_paused: bool,
}

fn serialize_torrent<S>(t: &TorrentMetaV1Info<ByteString>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    use base64::{engine::general_purpose, Engine as _};
    use serde::ser::Error;
    let mut writer = Vec::new();
    bencode_serialize_to_writer(t, &mut writer).map_err(S::Error::custom)?;
    let s = general_purpose::STANDARD_NO_PAD.encode(&writer);
    s.serialize(serializer)
}

fn deserialize_torrent<'de, D>(deserializer: D) -> Result<TorrentMetaV1Info<ByteString>, D::Error>
where
    D: Deserializer<'de>,
{
    use base64::{engine::general_purpose, Engine as _};
    use serde::de::Error;
    let s = String::deserialize(deserializer)?;
    let b = general_purpose::STANDARD_NO_PAD
        .decode(s)
        .map_err(D::Error::custom)?;
    TorrentMetaV1Info::<ByteString>::deserialize(&mut BencodeDeserializer::new_from_buf(&b))
        .map_err(D::Error::custom)
}

#[derive(Serialize, Deserialize)]
struct SerializedSessionDatabase {
    torrents: HashMap<usize, SerializedTorrent>,
}

pub struct Session {
    peer_id: Id20,
    dht: Option<Dht>,
    persistence_filename: PathBuf,
    peer_opts: PeerConnectionOptions,
    spawner: BlockingSpawner,
    db: RwLock<SessionDatabase>,
    output_folder: PathBuf,

    tcp_listen_port: Option<u16>,

    cancellation_token: CancellationToken,
}

async fn torrent_from_url(url: &str) -> anyhow::Result<TorrentMetaV1Owned> {
    let response = reqwest::get(url)
        .await
        .context("error downloading torrent metadata")?;
    if !response.status().is_success() {
        anyhow::bail!("GET {} returned {}", url, response.status())
    }
    let b = response
        .bytes()
        .await
        .with_context(|| format!("error reading response body from {url}"))?;
    torrent_from_bytes(&b).context("error decoding torrent")
}

fn compute_only_files<ByteBuf: AsRef<[u8]>>(
    torrent: &TorrentMetaV1Info<ByteBuf>,
    filename_re: &str,
) -> anyhow::Result<Vec<usize>> {
    let filename_re = regex::Regex::new(filename_re).context("filename regex is incorrect")?;
    let mut only_files = Vec::new();
    for (idx, (filename, _)) in torrent.iter_filenames_and_lengths()?.enumerate() {
        let full_path = filename
            .to_pathbuf()
            .with_context(|| format!("filename of file {idx} is not valid utf8"))?;
        if filename_re.is_match(full_path.to_str().unwrap()) {
            only_files.push(idx);
        }
    }
    if only_files.is_empty() {
        anyhow::bail!("none of the filenames match the given regex")
    }
    Ok(only_files)
}

/// Options for adding new torrents to the session.
#[serde_as]
#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AddTorrentOptions {
    /// Start in paused state.
    pub paused: bool,
    /// A regex to only download files matching it.
    pub only_files_regex: Option<String>,
    /// An explicit list of file IDs to download.
    /// To see the file indices, run with "list_only".
    pub only_files: Option<Vec<usize>>,
    /// Allow writing on top of existing files, including when resuming a torrent.
    /// You probably want to set it, however for safety it's not default.
    pub overwrite: bool,
    /// Only list the files in the torrent without starting it.
    pub list_only: bool,
    /// The output folder for the torrent. If not set, the session's default one will be used.
    pub output_folder: Option<String>,
    /// Sub-folder within session's default output folder. Will error if "output_folder" if also set.
    /// By default, multi-torrent files are downloaded to a sub-folder.
    pub sub_folder: Option<String>,
    /// Peer connection options, timeouts etc. If not set, session's defaults will be used.
    pub peer_opts: Option<PeerConnectionOptions>,

    /// Force a refresh interval for polling trackers.
    #[serde_as(as = "Option<serde_with::DurationSeconds>")]
    pub force_tracker_interval: Option<Duration>,

    pub disable_trackers: bool,

    /// Initial peers to start of with.
    pub initial_peers: Option<Vec<SocketAddr>>,

    /// This is used to restore the session from serialized state.
    #[serde(skip)]
    pub preferred_id: Option<usize>,
}

pub struct ListOnlyResponse {
    pub info_hash: Id20,
    pub info: TorrentMetaV1Info<ByteString>,
    pub only_files: Option<Vec<usize>>,
    pub output_folder: PathBuf,
    pub seen_peers: Vec<SocketAddr>,
}

#[allow(clippy::large_enum_variant)]
pub enum AddTorrentResponse {
    AlreadyManaged(TorrentId, ManagedTorrentHandle),
    ListOnly(ListOnlyResponse),
    Added(TorrentId, ManagedTorrentHandle),
}

impl AddTorrentResponse {
    pub fn into_handle(self) -> Option<ManagedTorrentHandle> {
        match self {
            Self::AlreadyManaged(_, handle) => Some(handle),
            Self::ListOnly(_) => None,
            Self::Added(_, handle) => Some(handle),
        }
    }
}

pub fn read_local_file_including_stdin(filename: &str) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    if filename == "-" {
        std::io::stdin()
            .read_to_end(&mut buf)
            .context("error reading stdin")?;
    } else {
        std::fs::File::open(filename)
            .context("error opening")?
            .read_to_end(&mut buf)
            .context("error reading")?;
    }
    Ok(buf)
}

pub enum AddTorrent<'a> {
    Url(Cow<'a, str>),
    TorrentFileBytes(Cow<'a, [u8]>),
    TorrentInfo(Box<TorrentMetaV1Owned>),
}

impl<'a> AddTorrent<'a> {
    // Don't call this from HTTP API.
    pub fn from_cli_argument(path: &'a str) -> anyhow::Result<Self> {
        if SUPPORTED_SCHEMES.iter().any(|s| path.starts_with(s)) {
            return Ok(Self::Url(Cow::Borrowed(path)));
        }
        Self::from_local_filename(path)
    }

    pub fn from_url(url: impl Into<Cow<'a, str>>) -> Self {
        Self::Url(url.into())
    }

    pub fn from_bytes(bytes: impl Into<Cow<'a, [u8]>>) -> Self {
        Self::TorrentFileBytes(bytes.into())
    }

    // Don't call this from HTTP API.
    pub fn from_local_filename(filename: &str) -> anyhow::Result<Self> {
        let file = read_local_file_including_stdin(filename)
            .with_context(|| format!("error reading local file {filename:?}"))?;
        Ok(Self::TorrentFileBytes(Cow::Owned(file)))
    }

    pub fn into_bytes(self) -> Vec<u8> {
        match self {
            Self::Url(s) => s.into_owned().into_bytes(),
            Self::TorrentFileBytes(b) => b.into_owned(),
            Self::TorrentInfo(_) => unimplemented!(),
        }
    }
}

#[derive(Default)]
pub struct SessionOptions {
    /// Turn on to disable DHT.
    pub disable_dht: bool,
    /// Turn on to disable DHT persistence. By default it will re-use stored DHT
    /// configuration, including the port it listens on.
    pub disable_dht_persistence: bool,
    /// Pass in to configure DHT persistence filename. This can be used to run multiple
    /// librqbit instances at a time.
    pub dht_config: Option<PersistentDhtConfig>,

    /// Turn on to dump session contents into a file periodically, so that on next start
    /// all remembered torrents will continue where they left off.
    pub persistence: bool,
    /// The filename for persistence. By default uses an OS-specific folder.
    pub persistence_filename: Option<PathBuf>,

    /// The peer ID to use. If not specified, a random one will be generated.
    pub peer_id: Option<Id20>,
    /// Configure default peer connection options. Can be overriden per torrent.
    pub peer_opts: Option<PeerConnectionOptions>,

    pub listen_port_range: Option<std::ops::Range<u16>>,
    pub enable_upnp_port_forwarding: bool,
}

async fn create_tcp_listener(
    port_range: std::ops::Range<u16>,
) -> anyhow::Result<(TcpListener, u16)> {
    for port in port_range.clone() {
        match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(l) => return Ok((l, port)),
            Err(e) => {
                debug!("error listening on port {port}: {e:#}")
            }
        }
    }
    bail!("no free TCP ports in range {port_range:?}");
}

pub(crate) struct CheckedIncomingConnection {
    pub addr: SocketAddr,
    pub stream: tokio::net::TcpStream,
    pub read_buf: Vec<u8>,
    pub handshake: Handshake<ByteString>,
    pub read_so_far: usize,
}

impl Session {
    /// Create a new session. The passed in folder will be used as a default unless overriden per torrent.
    pub async fn new(output_folder: PathBuf) -> anyhow::Result<Arc<Self>> {
        Self::new_with_opts(output_folder, SessionOptions::default()).await
    }

    pub fn default_persistence_filename() -> anyhow::Result<PathBuf> {
        let dir = get_configuration_directory("session")?;
        Ok(dir.data_dir().join("session.json"))
    }

    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancellation_token
    }

    /// Create a new session with options.
    pub async fn new_with_opts(
        output_folder: PathBuf,
        mut opts: SessionOptions,
    ) -> anyhow::Result<Arc<Self>> {
        let peer_id = opts.peer_id.unwrap_or_else(generate_peer_id);
        let token = CancellationToken::new();

        let (tcp_listener, tcp_listen_port) = if let Some(port_range) = opts.listen_port_range {
            let (l, p) = create_tcp_listener(port_range)
                .await
                .context("error listening on TCP")?;
            info!("Listening on 0.0.0.0:{p} for incoming peer connections");
            (Some(l), Some(p))
        } else {
            (None, None)
        };

        let dht = if opts.disable_dht {
            None
        } else {
            let dht = if opts.disable_dht_persistence {
                DhtBuilder::with_config(DhtConfig {
                    cancellation_token: Some(token.child_token()),
                    ..Default::default()
                })
                .await
                .context("error initializing DHT")?
            } else {
                let pdht_config = opts.dht_config.take().unwrap_or_default();
                PersistentDht::create(Some(pdht_config), Some(token.clone()))
                    .await
                    .context("error initializing persistent DHT")?
            };

            Some(dht)
        };
        let peer_opts = opts.peer_opts.unwrap_or_default();
        let persistence_filename = match opts.persistence_filename {
            Some(filename) => filename,
            None => Self::default_persistence_filename()?,
        };
        let spawner = BlockingSpawner::default();

        let session = Arc::new(Self {
            persistence_filename,
            peer_id,
            dht,
            peer_opts,
            spawner,
            output_folder,
            db: RwLock::new(Default::default()),
            cancellation_token: token,
            tcp_listen_port,
        });

        if let Some(tcp_listener) = tcp_listener {
            session.spawn(
                error_span!("tcp_listen", port = tcp_listen_port),
                session.clone().task_tcp_listener(tcp_listener),
            );
        }

        if let Some(listen_port) = tcp_listen_port {
            if opts.enable_upnp_port_forwarding {
                session.spawn(
                    error_span!("upnp_forward", port = listen_port),
                    session.clone().task_upnp_port_forwarder(listen_port),
                );
            }
        }

        if opts.persistence {
            info!(
                "will use {:?} for session persistence",
                session.persistence_filename
            );
            if let Some(parent) = session.persistence_filename.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("couldn't create directory {:?} for session storage", parent)
                })?;
            }
            let persistence_task = session.clone().task_persistence();
            session.spawn(error_span!("session_persistence"), persistence_task);
        }

        Ok(session)
    }

    async fn task_persistence(self: Arc<Self>) -> anyhow::Result<()> {
        // Populate initial from the state filename
        if let Err(e) = self.populate_from_stored().await {
            error!("could not populate session from stored file: {:?}", e);
        }

        let session = Arc::downgrade(&self);
        drop(self);

        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let session = match session.upgrade() {
                Some(s) => s,
                None => break,
            };
            if let Err(e) = session.dump_to_disk() {
                error!("error dumping session to disk: {:?}", e);
            }
        }

        Ok(())
    }

    async fn check_incoming_connection(
        &self,
        addr: SocketAddr,
        mut stream: TcpStream,
    ) -> anyhow::Result<(Arc<TorrentStateLive>, CheckedIncomingConnection)> {
        // TODO: move buffer handling to peer_connection

        let rwtimeout = self
            .peer_opts
            .read_write_timeout
            .unwrap_or_else(|| Duration::from_secs(10));

        let mut read_buf = vec![0u8; PIECE_MESSAGE_DEFAULT_LEN * 2];
        let mut read_so_far = with_timeout(rwtimeout, stream.read(&mut read_buf))
            .await
            .context("error reading handshake")?;
        if read_so_far == 0 {
            anyhow::bail!("bad handshake");
        }
        let (h, size) = Handshake::deserialize(&read_buf[..read_so_far])
            .map_err(|e| anyhow::anyhow!("error deserializing handshake: {:?}", e))?;

        trace!("received handshake from {addr}: {:?}", h);

        if h.peer_id == self.peer_id.0 {
            bail!("seems like we are connecting to ourselves, ignoring");
        }

        for (id, torrent) in self.db.read().torrents.iter() {
            if torrent.info_hash().0 != h.info_hash {
                continue;
            }

            let live = match torrent.live() {
                Some(live) => live,
                None => {
                    bail!("torrent {id} is not live, ignoring connection");
                }
            };

            let handshake = h.clone_to_owned();

            if read_so_far > size {
                read_buf.copy_within(size..read_so_far, 0);
            }
            read_so_far -= size;

            return Ok((
                live,
                CheckedIncomingConnection {
                    addr,
                    stream,
                    handshake,
                    read_buf,
                    read_so_far,
                },
            ));
        }

        bail!("didn't find a matching torrent for {:?}", Id20(h.info_hash))
    }

    async fn task_tcp_listener(self: Arc<Self>, l: TcpListener) -> anyhow::Result<()> {
        let mut futs = FuturesUnordered::new();

        loop {
            tokio::select! {
                r = l.accept() => {
                    match r {
                        Ok((stream, addr)) => {
                            trace!("accepted connection from {addr}");
                            futs.push(
                                self.check_incoming_connection(addr, stream)
                                    .map_err(|e| {
                                        debug!("error checking incoming connection: {e:#}");
                                        e
                                    })
                                    .instrument(error_span!("incoming", addr=%addr))
                            );
                        }
                        Err(e) => {
                            error!("error accepting: {e:#}");
                            continue;
                        }
                    }
                },
                Some(Ok((live, checked))) = futs.next(), if !futs.is_empty() => {
                    if let Err(e) = live.add_incoming_peer(checked) {
                        warn!("error handing over incoming connection: {e:#}");
                    }
                },
            }
        }
    }

    async fn task_upnp_port_forwarder(self: Arc<Self>, port: u16) -> anyhow::Result<()> {
        let pf = librqbit_upnp::UpnpPortForwarder::new(vec![port], None)?;
        pf.run_forever().await
    }

    pub fn get_dht(&self) -> Option<&Dht> {
        self.dht.as_ref()
    }

    fn merge_peer_opts(&self, other: Option<PeerConnectionOptions>) -> PeerConnectionOptions {
        let other = match other {
            Some(o) => o,
            None => self.peer_opts,
        };
        PeerConnectionOptions {
            connect_timeout: other.connect_timeout.or(self.peer_opts.connect_timeout),
            read_write_timeout: other
                .read_write_timeout
                .or(self.peer_opts.read_write_timeout),
            keep_alive_interval: other
                .keep_alive_interval
                .or(self.peer_opts.keep_alive_interval),
        }
    }

    /// Spawn a task in the context of the session.
    pub fn spawn(
        &self,
        span: tracing::Span,
        fut: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    ) {
        spawn_with_cancel(span, self.cancellation_token.clone(), fut);
    }

    /// Stop the session and all managed tasks.
    pub async fn stop(&self) {
        let torrents = self
            .db
            .read()
            .torrents
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for torrent in torrents {
            if let Err(e) = torrent.pause() {
                debug!("error pausing torrent: {e:#}");
            }
        }
        self.cancellation_token.cancel();
        // this sucks, but hopefully will be enough
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    async fn populate_from_stored(self: &Arc<Self>) -> anyhow::Result<()> {
        let mut rdr = match std::fs::File::open(&self.persistence_filename) {
            Ok(f) => BufReader::new(f),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(e).context(format!(
                    "error opening session file {:?}",
                    self.persistence_filename
                ))
            }
        };
        let db: SerializedSessionDatabase =
            serde_json::from_reader(&mut rdr).context("error deserializing session database")?;
        let mut futures = Vec::new();
        for (id, storrent) in db.torrents.into_iter() {
            let trackers: Vec<ByteString> = storrent
                .trackers
                .into_iter()
                .map(|t| ByteString(t.into_bytes()))
                .collect();
            let info = TorrentMetaV1Owned {
                announce: trackers
                    .get(0)
                    .cloned()
                    .unwrap_or_else(|| ByteString(b"http://retracker.local/announce".to_vec())),
                announce_list: vec![trackers],
                info: storrent.info,
                comment: None,
                created_by: None,
                encoding: None,
                publisher: None,
                publisher_url: None,
                creation_date: None,
                info_hash: Id20::from_str(&storrent.info_hash)?,
            };
            futures.push({
                let session = self.clone();
                async move {
                    session
                        .add_torrent(
                            AddTorrent::TorrentInfo(Box::new(info)),
                            Some(AddTorrentOptions {
                                paused: storrent.is_paused,
                                output_folder: Some(
                                    storrent
                                        .output_folder
                                        .to_str()
                                        .context("broken path")?
                                        .to_owned(),
                                ),
                                only_files: storrent.only_files,
                                overwrite: true,
                                preferred_id: Some(id),
                                ..Default::default()
                            }),
                        )
                        .await
                        .map_err(|e| {
                            error!("error adding torrent from stored session: {:?}", e);
                            e
                        })
                }
            });
        }
        futures::future::join_all(futures).await;
        Ok(())
    }

    fn dump_to_disk(&self) -> anyhow::Result<()> {
        let tmp_filename = format!("{}.tmp", self.persistence_filename.to_str().unwrap());
        let mut tmp = BufWriter::new(
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&tmp_filename)
                .with_context(|| format!("error opening {:?}", tmp_filename))?,
        );
        let serialized = self.db.read().serialize();
        serde_json::to_writer(&mut tmp, &serialized).context("error serializing")?;
        drop(tmp);

        std::fs::rename(&tmp_filename, &self.persistence_filename)
            .context("error renaming persistence file")?;
        trace!(filename=?self.persistence_filename, "wrote persistence");
        Ok(())
    }

    /// Run a callback given the currently managed torrents.
    pub fn with_torrents<R>(
        &self,
        callback: impl Fn(&mut dyn Iterator<Item = (TorrentId, &ManagedTorrentHandle)>) -> R,
    ) -> R {
        callback(&mut self.db.read().torrents.iter().map(|(id, t)| (*id, t)))
    }

    /// Add a torrent to the session.
    pub async fn add_torrent(
        &self,
        add: AddTorrent<'_>,
        opts: Option<AddTorrentOptions>,
    ) -> anyhow::Result<AddTorrentResponse> {
        // Magnet links are different in that we first need to discover the metadata.
        let span = error_span!("add_torrent");
        let _ = span.enter();

        let opts = opts.unwrap_or_default();

        let announce_port = if opts.list_only {
            None
        } else {
            self.tcp_listen_port
        };

        let (info_hash, info, dht_rx, trackers, initial_peers) = match add {
            AddTorrent::Url(magnet) if magnet.starts_with("magnet:") => {
                let Magnet {
                    info_hash,
                    trackers,
                } = Magnet::parse(&magnet).context("provided path is not a valid magnet URL")?;

                let dht_rx = self
                    .dht
                    .as_ref()
                    .context("magnet links without DHT are not supported")?
                    .get_peers(info_hash, announce_port)?;

                let trackers = trackers
                    .into_iter()
                    .filter_map(|url| match reqwest::Url::parse(&url) {
                        Ok(url) => Some(url),
                        Err(e) => {
                            warn!("error parsing tracker {} as url: {}", url, e);
                            None
                        }
                    })
                    .collect();

                debug!(?info_hash, "querying DHT");
                let (info, dht_rx, initial_peers) = match read_metainfo_from_peer_receiver(
                    self.peer_id,
                    info_hash,
                    opts.initial_peers.clone().unwrap_or_default(),
                    dht_rx,
                    Some(self.merge_peer_opts(opts.peer_opts)),
                )
                .await
                {
                    ReadMetainfoResult::Found { info, rx, seen } => (info, rx, seen),
                    ReadMetainfoResult::ChannelClosed { .. } => {
                        anyhow::bail!("DHT died, no way to discover torrent metainfo")
                    }
                };
                debug!(?info, "received result from DHT");
                (
                    info_hash,
                    info,
                    if opts.paused || opts.list_only {
                        None
                    } else {
                        Some(dht_rx)
                    },
                    trackers,
                    initial_peers,
                )
            }
            other => {
                let torrent = match other {
                    AddTorrent::Url(url)
                        if url.starts_with("http://") || url.starts_with("https://") =>
                    {
                        torrent_from_url(&url).await?
                    }
                    AddTorrent::Url(url) => {
                        bail!(
                            "unsupported URL {:?}. Supporting magnet:, http:, and https",
                            url
                        )
                    }
                    AddTorrent::TorrentFileBytes(bytes) => {
                        torrent_from_bytes(&bytes).context("error decoding torrent")?
                    }
                    AddTorrent::TorrentInfo(t) => *t,
                };

                let dht_rx = match self.dht.as_ref() {
                    Some(dht) if !opts.paused && !opts.list_only => {
                        debug!(info_hash=?torrent.info_hash, "reading peers from DHT");
                        Some(dht.get_peers(torrent.info_hash, announce_port)?)
                    }
                    _ => None,
                };
                let trackers = torrent
                    .iter_announce()
                    .filter_map(|tracker| {
                        let url = match std::str::from_utf8(tracker.as_ref()) {
                            Ok(url) => url,
                            Err(_) => {
                                warn!("cannot parse tracker url as utf-8, ignoring");
                                return None;
                            }
                        };
                        match Url::parse(url) {
                            Ok(url) => Some(url),
                            Err(e) => {
                                warn!("cannot parse tracker URL {}: {}", url, e);
                                None
                            }
                        }
                    })
                    .collect::<Vec<_>>();
                (
                    torrent.info_hash,
                    torrent.info,
                    dht_rx,
                    trackers,
                    opts.initial_peers
                        .clone()
                        .unwrap_or_default()
                        .into_iter()
                        .collect(),
                )
            }
        };

        self.main_torrent_info(
            info_hash,
            info,
            dht_rx,
            initial_peers.into_iter().collect(),
            trackers,
            opts,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn main_torrent_info(
        &self,
        info_hash: Id20,
        info: TorrentMetaV1Info<ByteString>,
        dht_peer_rx: Option<RequestPeersStream>,
        initial_peers: Vec<SocketAddr>,
        trackers: Vec<reqwest::Url>,
        opts: AddTorrentOptions,
    ) -> anyhow::Result<AddTorrentResponse> {
        debug!("Torrent info: {:#?}", &info);

        let get_only_files =
            |only_files: Option<Vec<usize>>, only_files_regex: Option<String>, list_only: bool| {
                match (only_files, only_files_regex) {
                    (Some(_), Some(_)) => {
                        bail!("only_files and only_files_regex are mutually exclusive");
                    }
                    (Some(only_files), None) => {
                        let total_files = info.iter_file_lengths()?.count();
                        for id in only_files.iter().copied() {
                            if id >= total_files {
                                anyhow::bail!("file id {} is out of range", id);
                            }
                        }
                        Ok(Some(only_files))
                    }
                    (None, Some(filename_re)) => {
                        let only_files = compute_only_files(&info, &filename_re)?;
                        for (idx, (filename, _)) in info.iter_filenames_and_lengths()?.enumerate() {
                            if !only_files.contains(&idx) {
                                continue;
                            }
                            if !list_only {
                                info!(?filename, "will download");
                            }
                        }
                        Ok(Some(only_files))
                    }
                    (None, None) => Ok(None),
                }
            };

        let only_files = get_only_files(opts.only_files, opts.only_files_regex, opts.list_only)?;

        let get_default_subfolder = || {
            let files = info
                .iter_filenames_and_lengths()?
                .map(|(f, l)| Ok((f.to_pathbuf()?, l)))
                .collect::<anyhow::Result<Vec<(PathBuf, u64)>>>()?;
            if files.len() < 2 {
                return Ok(None);
            }
            if let Some(name) = &info.name {
                let s = std::str::from_utf8(name.as_slice())
                    .context("invalid UTF-8 in torrent name")?;
                return Ok(Some(PathBuf::from(s)));
            };
            // Let the subfolder name be the longest filename
            let longest = files
                .iter()
                .max_by_key(|(_, l)| l)
                .unwrap()
                .0
                .file_stem()
                .context("can't determine longest filename")?;
            Ok::<_, anyhow::Error>(Some(PathBuf::from(longest)))
        };

        let output_folder = match (opts.output_folder, opts.sub_folder) {
            (None, None) => self
                .output_folder
                .join(get_default_subfolder()?.unwrap_or_default()),
            (Some(o), None) => PathBuf::from(o),
            (Some(_), Some(_)) => bail!("you can't provide both output_folder and sub_folder"),
            (None, Some(s)) => self.output_folder.join(s),
        };

        if opts.list_only {
            return Ok(AddTorrentResponse::ListOnly(ListOnlyResponse {
                info_hash,
                info,
                only_files,
                output_folder,
                seen_peers: initial_peers,
            }));
        }

        let mut builder = ManagedTorrentBuilder::new(info, info_hash, output_folder.clone());
        builder
            .overwrite(opts.overwrite)
            .spawner(self.spawner)
            .cancellation_token(self.cancellation_token.child_token())
            .peer_id(self.peer_id);

        if opts.disable_trackers {
            builder.trackers(trackers);
        }

        if let Some(only_files) = only_files {
            builder.only_files(only_files);
        }
        if let Some(interval) = opts.force_tracker_interval {
            builder.force_tracker_interval(interval);
        }

        let peer_opts = self.merge_peer_opts(opts.peer_opts);

        if let Some(t) = peer_opts.connect_timeout {
            builder.peer_connect_timeout(t);
        }

        if let Some(t) = peer_opts.read_write_timeout {
            builder.peer_read_write_timeout(t);
        }

        let (managed_torrent, id) = {
            let mut g = self.db.write();
            if let Some((id, handle)) = g.torrents.iter().find(|(_, t)| t.info_hash() == info_hash)
            {
                return Ok(AddTorrentResponse::AlreadyManaged(*id, handle.clone()));
            }
            let next_id = g.torrents.len();
            let managed_torrent =
                builder.build(error_span!(parent: None, "torrent", id = next_id))?;
            let id = g.add_torrent(managed_torrent.clone(), opts.preferred_id);
            (managed_torrent, id)
        };

        {
            let span = managed_torrent.info.span.clone();
            let _ = span.enter();
            managed_torrent
                .start(initial_peers, dht_peer_rx, opts.paused)
                .context("error starting torrent")?;
        }

        Ok(AddTorrentResponse::Added(id, managed_torrent))
    }

    pub fn get(&self, id: TorrentId) -> Option<ManagedTorrentHandle> {
        self.db.read().torrents.get(&id).cloned()
    }

    pub fn delete(&self, id: TorrentId, delete_files: bool) -> anyhow::Result<()> {
        let removed = self
            .db
            .write()
            .torrents
            .remove(&id)
            .with_context(|| format!("torrent with id {} did not exist", id))?;

        let paused = removed
            .with_state_mut(|s| {
                let paused = match s.take() {
                    ManagedTorrentState::Paused(p) => p,
                    ManagedTorrentState::Live(l) => l.pause()?,
                    _ => return Ok(None),
                };
                Ok::<_, anyhow::Error>(Some(paused))
            })
            .context("error pausing torrent");

        match (paused, delete_files) {
            (Err(e), true) => Err(e).context("torrent deleted, but could not delete files"),
            (Err(e), false) => {
                warn!(error=?e, "could not delete torrent files");
                Ok(())
            }
            (Ok(Some(paused)), true) => {
                drop(paused.files);
                for file in paused.filenames {
                    if let Err(e) = std::fs::remove_file(&file) {
                        warn!(?file, error=?e, "could not delete file");
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub fn unpause(&self, handle: &ManagedTorrentHandle) -> anyhow::Result<()> {
        let peer_rx = self
            .dht
            .as_ref()
            .map(|dht| dht.get_peers(handle.info_hash(), self.tcp_listen_port))
            .transpose()?;
        handle.start(Default::default(), peer_rx, false)?;
        Ok(())
    }
}
