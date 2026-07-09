use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::http::path;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RoutePath(String);

impl RoutePath {
    pub fn parse(value: impl AsRef<str>) -> Option<Self> {
        path::normalize_route_path(value.as_ref())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn from_normalized(value: String) -> Self {
        Self(value)
    }
}

/// Resolves a request path to an embedded asset as
/// `(content_type, content_encoding, body)`. Used by [`Router::embedded_assets`]
/// to serve a frontend compiled into the binary. Returns `None` for a path the
/// embed does not cover.
pub type EmbeddedResolver = fn(&str) -> Option<(&'static str, &'static str, &'static [u8])>;

/// The request passed to a [generated route](Router::mount_generated) handler.
pub struct GeneratedRequest<'a> {
    /// The full request path.
    pub path: &'a str,
    /// The path with the mount prefix stripped, e.g. `report.txt` for a
    /// `/highlight/report.txt` request on a `/highlight` mount.
    pub relative: &'a str,
    /// True for a `HEAD` request, whose body must be omitted.
    pub is_head: bool,
}

/// A computed response from a generated-route handler.
///
/// Each variant is a distinct outcome, so there is no in-band status sentinel: a
/// [`pass`](GeneratedResponse::pass) can only be produced by that constructor,
/// never by a handler that happens to compute a particular status code.
pub enum GeneratedResponse {
    /// An in-memory body served with the given status and content type. The
    /// router applies the request's `Range` and `HEAD` to the shared `Arc`
    /// without copying the body.
    Bytes {
        status: u16,
        content_type: String,
        body: Arc<Vec<u8>>,
    },
    /// Serve this file from disk, with full `Range`/`If-Modified-Since` support.
    /// Used to serve a specific file the handler located itself (e.g. a download
    /// held in a per-room directory the router does not mount).
    File(PathBuf),
    /// An error response with an empty body.
    Error(u16),
    /// Defer to a [directory mount](Router::mount_file_dir) on the handler's own
    /// prefix, or a `404` when none matches. Lets a generated handler fall
    /// through to disk when it has nothing to serve itself.
    Pass,
}

impl GeneratedResponse {
    /// A `200 OK` response with the given body and content type.
    pub fn ok(content_type: impl Into<String>, body: impl Into<Vec<u8>>) -> Self {
        Self::Bytes {
            status: 200,
            content_type: content_type.into(),
            body: Arc::new(body.into()),
        }
    }

    /// A `200 OK` response backed by an already shared vector.
    pub fn ok_shared(content_type: impl Into<String>, body: Arc<Vec<u8>>) -> Self {
        Self::Bytes {
            status: 200,
            content_type: content_type.into(),
            body,
        }
    }

    /// An error response with an empty body.
    pub fn error(status: u16) -> Self {
        Self::Error(status)
    }

    /// Serve `path` from disk with `Range`/`If-Modified-Since` support.
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self::File(path.into())
    }

    /// Defer to a directory mount on the handler's prefix, or a `404`.
    pub fn pass() -> Self {
        Self::Pass
    }
}

/// A handler that computes a response for any path under its mount prefix. It
/// runs on the server's I/O thread pool, so it may block on disk or CPU without
/// stalling the event loop.
pub type GeneratedHandler = Arc<dyn Fn(&GeneratedRequest) -> GeneratedResponse + Send + Sync>;

#[derive(Clone)]
pub(crate) struct GeneratedMount {
    pub(crate) prefix: RoutePath,
    pub(crate) handler: GeneratedHandler,
}

#[derive(Clone, Default)]
pub struct Router {
    static_assets: Vec<StaticAsset>,
    mounts: Vec<DirMount>,
    generated: Vec<GeneratedMount>,
    websocket_routes: Vec<RoutePath>,
    embedded: Option<EmbeddedResolver>,
}

impl Router {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn route_bytes(
        mut self,
        path: impl AsRef<str>,
        body: impl Into<Vec<u8>>,
        content_type: impl Into<String>,
    ) -> Self {
        self.add_route_bytes(path, body, content_type);
        self
    }

    pub fn add_route_bytes(
        &mut self,
        path: impl AsRef<str>,
        body: impl Into<Vec<u8>>,
        content_type: impl Into<String>,
    ) -> &mut Self {
        let path = RoutePath::parse(path.as_ref()).unwrap_or_else(|| {
            panic!("invalid static route path: {:?}", path.as_ref());
        });
        if let Some(asset) = self
            .static_assets
            .iter_mut()
            .find(|asset| asset.path == path)
        {
            asset.body = Arc::<[u8]>::from(body.into());
            asset.content_type = content_type.into();
            return self;
        }
        self.static_assets.push(StaticAsset {
            path,
            body: Arc::<[u8]>::from(body.into()),
            content_type: content_type.into(),
        });
        self
    }

    pub fn static_route(
        self,
        path: impl AsRef<str>,
        body: impl Into<Vec<u8>>,
        content_type: impl Into<String>,
    ) -> Self {
        self.route_bytes(path, body, content_type)
    }

    pub fn mount_static_dir(mut self, prefix: impl AsRef<str>, dir: impl Into<PathBuf>) -> Self {
        self.add_static_dir(prefix, dir);
        self
    }

    pub fn add_static_dir(
        &mut self,
        prefix: impl AsRef<str>,
        dir: impl Into<PathBuf>,
    ) -> &mut Self {
        self.add_mount(prefix, dir.into(), MountKind::StaticDir)
    }

    pub fn mount_file_dir(mut self, prefix: impl AsRef<str>, dir: impl Into<PathBuf>) -> Self {
        self.add_file_dir(prefix, dir);
        self
    }

    pub fn add_file_dir(&mut self, prefix: impl AsRef<str>, dir: impl Into<PathBuf>) -> &mut Self {
        self.add_mount(prefix, dir.into(), MountKind::FileDir)
    }

    /// Serves assets compiled into the binary. The resolver maps a request path
    /// to `(content_type, content_encoding, body)`, with `content_encoding`
    /// empty for an identity body. It is consulted after registered static
    /// routes and before filesystem mounts.
    pub fn embedded_assets(mut self, resolver: EmbeddedResolver) -> Self {
        self.embedded = Some(resolver);
        self
    }

    pub(crate) fn embedded_asset(
        &self,
        path: &str,
    ) -> Option<(&'static str, &'static str, &'static [u8])> {
        self.embedded.and_then(|resolver| resolver(path))
    }

    /// Mounts a handler that computes responses for every path under `prefix`.
    ///
    /// The handler runs on the I/O thread pool, so it may read files or do
    /// heavy work without blocking the event loop. It is consulted after static
    /// routes and embedded assets, and before directory mounts, so a specific
    /// generated prefix wins over a catch-all `/` static mount.
    pub fn mount_generated(mut self, prefix: impl AsRef<str>, handler: GeneratedHandler) -> Self {
        self.add_generated(prefix, handler);
        self
    }

    pub fn add_generated(
        &mut self,
        prefix: impl AsRef<str>,
        handler: GeneratedHandler,
    ) -> &mut Self {
        let mut prefix = RoutePath::parse(prefix.as_ref()).unwrap_or_else(|| {
            panic!("invalid generated route prefix: {:?}", prefix.as_ref());
        });
        if prefix.0.len() > 1 {
            while prefix.0.ends_with('/') {
                prefix.0.pop();
            }
        }
        self.generated.retain(|mount| mount.prefix != prefix);
        self.generated.push(GeneratedMount { prefix, handler });
        self
    }

    /// Resolves a request path to a generated handler and the path relative to
    /// its mount prefix, choosing the longest matching prefix.
    pub(crate) fn resolve_generated<'m, 'p>(
        &'m self,
        path: &'p RoutePath,
    ) -> Option<(&'m GeneratedHandler, &'p str, &'m RoutePath)> {
        self.generated
            .iter()
            .filter_map(|mount| Some((mount, relative_to_prefix(&mount.prefix, path)?)))
            .max_by_key(|(mount, _)| mount.prefix.as_str().len())
            .map(|(mount, relative)| (&mount.handler, relative, &mount.prefix))
    }

    pub fn websocket(mut self, path: impl AsRef<str>) -> Self {
        self.add_websocket(path);
        self
    }

    pub fn add_websocket(&mut self, path: impl AsRef<str>) -> &mut Self {
        let path = RoutePath::parse(path.as_ref()).unwrap_or_else(|| {
            panic!("invalid websocket route path: {:?}", path.as_ref());
        });
        if !self.websocket_routes.iter().any(|route| route == &path) {
            self.websocket_routes.push(path);
        }
        self
    }

    pub(crate) fn static_asset(&self, path: &RoutePath) -> Option<&StaticAsset> {
        self.static_assets.iter().find(|asset| &asset.path == path)
    }

    pub(crate) fn has_websocket(&self, path: &RoutePath) -> bool {
        self.websocket_routes.iter().any(|route| route == path)
    }

    pub(crate) fn resolve_mount<'m, 'p>(
        &'m self,
        path: &'p RoutePath,
    ) -> Option<ResolvedMount<'m, 'p>> {
        self.mounts
            .iter()
            .filter_map(|mount| {
                let relative_path = mount.relative_path(path)?;
                Some(ResolvedMount {
                    mount,
                    relative_path,
                })
            })
            .max_by_key(|resolved| resolved.mount.prefix.as_str().len())
    }

    fn add_mount(&mut self, prefix: impl AsRef<str>, root: PathBuf, kind: MountKind) -> &mut Self {
        let mut prefix = RoutePath::parse(prefix.as_ref()).unwrap_or_else(|| {
            panic!("invalid directory mount prefix: {:?}", prefix.as_ref());
        });
        if prefix.0.len() > 1 {
            while prefix.0.ends_with('/') {
                prefix.0.pop();
            }
        }
        self.mounts
            .retain(|mount| mount.prefix != prefix || mount.kind != kind);
        self.mounts.push(DirMount { prefix, root, kind });
        self
    }
}

#[derive(Clone)]
pub(crate) struct StaticAsset {
    pub(crate) path: RoutePath,
    pub(crate) body: Arc<[u8]>,
    pub(crate) content_type: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MountKind {
    StaticDir,
    FileDir,
}

#[derive(Clone)]
pub(crate) struct DirMount {
    pub(crate) prefix: RoutePath,
    pub(crate) root: PathBuf,
    pub(crate) kind: MountKind,
}

impl DirMount {
    fn relative_path<'a>(&self, path: &'a RoutePath) -> Option<&'a str> {
        relative_to_prefix(&self.prefix, path)
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
}

/// Strips a mount `prefix` from a request `path`, returning the remainder or
/// [`None`] when the path is not under the prefix.
fn relative_to_prefix<'a>(prefix: &RoutePath, path: &'a RoutePath) -> Option<&'a str> {
    let prefix = prefix.as_str();
    let path = path.as_str();
    if prefix == "/" {
        return Some(path.trim_start_matches('/'));
    }
    if path == prefix {
        return Some("");
    }
    path.strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('/'))
}

pub(crate) struct ResolvedMount<'m, 'p> {
    pub(crate) mount: &'m DirMount,
    pub(crate) relative_path: &'p str,
}
