use std::{
    ops::Deref,
    path::{Path, PathBuf},
};

/// A uniquely named temporary directory that is removed on drop, including
/// while a test unwinds.
pub(crate) struct TempDir(tempfile::TempDir);

impl TempDir {
    pub(crate) fn new(label: &str) -> Self {
        let prefix = format!("chatt-{label}-");
        Self(
            tempfile::Builder::new()
                .prefix(&prefix)
                .tempdir()
                .expect("create temporary test directory"),
        )
    }

    /// Retains this directory while exposing one path inside it as the value.
    pub(crate) fn with_path(self, relative: impl AsRef<Path>) -> TempPath {
        let path = self.join(relative);
        TempPath { _dir: self, path }
    }
}

impl Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.0.path()
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        self
    }
}

/// A path whose owning temporary directory remains alive for the same scope.
pub(crate) struct TempPath {
    _dir: TempDir,
    path: PathBuf,
}

impl Deref for TempPath {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl AsRef<Path> for TempPath {
    fn as_ref(&self) -> &Path {
        self
    }
}
