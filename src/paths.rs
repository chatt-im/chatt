use std::path::PathBuf;

pub(crate) fn client_config_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CHATT_CONFIG").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }
    default_config_dir().map(|dir| dir.join("chatt").join("client.toml"))
}

/// The name a frontend claims for single-instance exclusion.
///
/// Scoped by absolute config path so independent configurations may run side
/// by side while aliases of the same path still contend for one claim. SHA-256
/// is stable across separate processes and Rust toolchain versions.
pub(crate) fn client_instance_name(config_path: Option<&str>) -> String {
    use aws_lc_rs::digest;

    let path = match config_path {
        Some(path) => PathBuf::from(path),
        None => client_config_path().unwrap_or_default(),
    };
    let path = std::path::absolute(&path).unwrap_or(path);
    let digest = digest::digest(
        &digest::SHA256,
        &chatt_platform::os_str_to_bytes(path.as_os_str()),
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
    use super::client_instance_name;

    #[test]
    fn instance_name_is_stable_and_scoped_to_the_absolute_config_path() {
        let first = client_instance_name(Some("/tmp/chatt-instance-a/client.toml"));
        let alias = client_instance_name(Some("/tmp/chatt-instance-a/./client.toml"));
        let second = client_instance_name(Some("/tmp/chatt-instance-b/client.toml"));

        assert_eq!(first, alias);
        assert_ne!(first, second);
        assert_eq!(first.len(), "chatt-client-".len() + 16);
    }
}
