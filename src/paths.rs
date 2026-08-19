use std::path::{Path, PathBuf};

pub(crate) fn client_config_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CHATT_CONFIG").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }
    default_config_dir().map(|dir| dir.join("chatt").join("client.toml"))
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
pub(crate) fn canonical_config_path(path: &Path) -> PathBuf {
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
pub(crate) fn client_instance_name(config_path: &Path) -> String {
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
    use super::{canonical_client_config_path, client_instance_name};

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
