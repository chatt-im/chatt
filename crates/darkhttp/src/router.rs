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

#[derive(Clone, Default)]
pub struct Router {
    static_assets: Vec<StaticAsset>,
    mounts: Vec<DirMount>,
    websocket_routes: Vec<RoutePath>,
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
        let prefix = self.prefix.as_str();
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

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
}

pub(crate) struct ResolvedMount<'m, 'p> {
    pub(crate) mount: &'m DirMount,
    pub(crate) relative_path: &'p str,
}
