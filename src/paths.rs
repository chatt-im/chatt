use std::path::PathBuf;

pub(crate) fn client_config_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CHATT_CONFIG").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }
    default_config_dir().map(|dir| dir.join("chatt").join("client.toml"))
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
