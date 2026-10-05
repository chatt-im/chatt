use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Environment fallback for `--portable-root`.
pub(crate) const PORTABLE_ROOT_ENV: &str = "CHATT_PORTABLE_ROOT";

static PORTABLE_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Roots all client config, data, and runtime state under `dir` for the rest
/// of the process.
///
/// Explicit overrides (`--config`, `CHATT_CONFIG`, `CHATT_CONTROL_SOCKET`,
/// `CHATT_RUN_DIR`) still win; the ambient `XDG_*` and `HOME` defaults do not,
/// since a sandbox sets those too. The directory is created and made absolute
/// so later canonicalization and socket paths do not depend on the working
/// directory.
pub(crate) fn set_portable_root(dir: &Path) -> Result<(), String> {
    if dir.as_os_str().is_empty() {
        return Err("portable root must not be empty".to_string());
    }
    let dir = std::path::absolute(dir)
        .map_err(|error| format!("invalid portable root {}: {error}", dir.display()))?;
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("failed to create {}: {error}", dir.display()))?;
    PORTABLE_ROOT
        .set(dir)
        .map_err(|_| "portable root is already set".to_string())
}

fn portable_root() -> Option<&'static Path> {
    PORTABLE_ROOT.get().map(PathBuf::as_path)
}

/// Directory holding the control socket, its leadership lock, and other
/// per-session runtime files when running under a portable root.
pub(crate) fn portable_run_dir() -> Option<PathBuf> {
    portable_root().map(portable_run_dir_in)
}

/// Scratch directory for in-flight browser uploads.
pub(crate) fn web_upload_dir() -> PathBuf {
    match portable_run_dir() {
        Some(dir) => dir.join("web-uploads"),
        None => std::env::temp_dir().join("chatt-web-uploads"),
    }
}

pub(crate) fn client_config_path() -> Option<PathBuf> {
    resolve_client_config_path(
        std::env::var_os("CHATT_CONFIG").map(PathBuf::from),
        portable_root(),
    )
}

fn resolve_client_config_path(explicit: Option<PathBuf>, root: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit.filter(|path| !path.as_os_str().is_empty()) {
        return Some(path);
    }
    if let Some(root) = root {
        return Some(root.join("client.toml"));
    }
    default_config_dir().map(|dir| dir.join("chatt").join("client.toml"))
}

fn portable_run_dir_in(root: &Path) -> PathBuf {
    root.join("run")
}

fn portable_data_dir_in(root: &Path) -> PathBuf {
    root.join("data")
}

/// The one path every alias of a configuration resolves to.
///
/// Exclusion is only as good as the identity it keys on, and `absolute` is
/// lexical: it leaves symlinks unresolved and, where the filesystem ignores
/// case, leaves two spellings distinct. Canonicalizing the directory covers
/// both. The final component is joined rather than canonicalized because the
/// configuration need not exist yet at first run, and because `save_runtime`
/// replaces it by rename, so its own inode is not stable anyway. A directory
/// that does not exist yet falls back to the lexical form.
#[cfg(any(windows, feature = "embedded-client", test))]
pub(crate) fn canonical_config_path(path: &std::path::Path) -> PathBuf {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let Some(parent) = path.parent() else {
        return path;
    };
    let Some(name) = path.file_name() else {
        return path;
    };
    match std::fs::canonicalize(parent) {
        Ok(parent) => parent.join(name),
        Err(_) => path,
    }
}

/// [`canonical_config_path`] over the selected configuration, or the default.
#[cfg(any(windows, test))]
pub(crate) fn canonical_client_config_path(config_path: Option<&str>) -> PathBuf {
    let path = match config_path {
        Some(path) => PathBuf::from(path),
        None => client_config_path().unwrap_or_default(),
    };
    canonical_config_path(&path)
}

/// The name a frontend claims for single-instance exclusion.
///
/// Scoped by canonical config path so independent configurations may run side
/// by side while aliases of the same path contend for one claim. SHA-256 is
/// stable across separate processes and Rust toolchain versions.
#[cfg(any(windows, feature = "embedded-client", test))]
pub(crate) fn client_instance_name(config_path: &std::path::Path) -> String {
    use aws_lc_rs::digest;

    let digest = digest::digest(
        &digest::SHA256,
        &chatt_platform::os_str_to_bytes(config_path.as_os_str()),
    );
    let mut name = String::from("chatt-client-");
    for byte in &digest.as_ref()[..8] {
        name.push_str(&format!("{byte:02x}"));
    }
    name
}

pub(crate) fn client_data_dir() -> Option<PathBuf> {
    if let Some(root) = portable_root() {
        return Some(portable_data_dir_in(root));
    }
    if let Some(path) = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path).join("chatt"));
    }

    #[cfg(target_os = "windows")]
    {
        std::env::var_os("LOCALAPPDATA")
            .or_else(|| std::env::var_os("APPDATA"))
            .map(|base| PathBuf::from(base).join("chatt"))
    }

    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Library/Application Support/chatt"))
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share/chatt"))
    }
}

pub(crate) fn default_download_dir() -> Option<PathBuf> {
    client_data_dir().map(|dir| dir.join("files"))
}

fn default_config_dir() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }

    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA").map(PathBuf::from)
    }

    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        canonical_client_config_path, client_instance_name, portable_data_dir_in,
        portable_run_dir_in, resolve_client_config_path,
    };

    #[test]
    fn explicit_config_path_wins_over_portable_root() {
        let root = Path::new("/portable");
        let explicit = PathBuf::from("/elsewhere/client.toml");

        assert_eq!(
            resolve_client_config_path(Some(explicit.clone()), Some(root)),
            Some(explicit)
        );
    }

    #[test]
    fn portable_root_holds_config_data_and_runtime_state() {
        let root = Path::new("/portable");

        assert_eq!(
            resolve_client_config_path(None, Some(root)),
            Some(root.join("client.toml"))
        );
        assert_eq!(
            resolve_client_config_path(Some(PathBuf::new()), Some(root)),
            Some(root.join("client.toml"))
        );
        assert!(portable_data_dir_in(root).starts_with(root));
        assert!(portable_run_dir_in(root).starts_with(root));
        assert_ne!(portable_data_dir_in(root), portable_run_dir_in(root));
    }

    fn name_for(path: &str) -> String {
        client_instance_name(&canonical_client_config_path(Some(path)))
    }

    #[test]
    fn instance_name_is_stable_and_scoped_to_the_absolute_config_path() {
        let first = name_for("/tmp/chatt-instance-a/client.toml");
        let alias = name_for("/tmp/chatt-instance-a/./client.toml");
        let second = name_for("/tmp/chatt-instance-b/client.toml");

        assert_eq!(first, alias);
        assert_ne!(first, second);
        assert_eq!(first.len(), "chatt-client-".len() + 16);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_resolves_to_the_same_instance_name() {
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();

        assert_eq!(
            name_for(real.join("client.toml").to_str().unwrap()),
            name_for(alias.join("client.toml").to_str().unwrap())
        );
    }
}
