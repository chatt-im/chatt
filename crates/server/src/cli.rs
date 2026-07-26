use std::path::PathBuf;

use crate::config::{CONFIG_OPTION_SPECS, ConfigOverride};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ServeTarget {
    Config(PathBuf),
    Directory(PathBuf),
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Command {
    Serve {
        target: ServeTarget,
        overrides: Vec<ConfigOverride>,
    },
    InitConfig(PathBuf),
    Invite(String),
    MlsStorageStatus,
    MlsCompact,
    Help,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct ParsedCli {
    pub logfile: Option<String>,
    pub command: Command,
}

pub(super) fn parse(args: &[String]) -> Result<ParsedCli, String> {
    let (logfile, args) = extract_logfile(args)?;
    let command = match args.as_slice() {
        [command] if command == "--help" || command == "-h" => Command::Help,
        [command, user] if command == "invite" && !user.trim().is_empty() => {
            Command::Invite(user.clone())
        }
        [first, second] if first == "mls" && second == "storage-status" => {
            Command::MlsStorageStatus
        }
        [first, second] if first == "mls" && second == "compact" => Command::MlsCompact,
        [command, path] if command == "init-config" && !path.trim().is_empty() => {
            Command::InitConfig(PathBuf::from(path))
        }
        [command, rest @ ..] if command == "serve" => parse_serve(rest)?,
        _ => return Err("invalid command line".to_string()),
    };
    Ok(ParsedCli { logfile, command })
}

fn extract_logfile(args: &[String]) -> Result<(Option<String>, Vec<String>), String> {
    let mut logfile = None;
    let mut positional = Vec::new();
    let mut index = 1;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--logfile" {
            let value = args
                .get(index + 1)
                .ok_or_else(|| "--logfile requires a path".to_string())?;
            if value.is_empty() {
                return Err("--logfile requires a non-empty path".to_string());
            }
            logfile = Some(value.clone());
            index += 2;
        } else if let Some(value) = arg.strip_prefix("--logfile=") {
            if value.is_empty() {
                return Err("--logfile requires a non-empty path".to_string());
            }
            logfile = Some(value.to_string());
            index += 1;
        } else {
            positional.push(arg.clone());
            index += 1;
        }
    }
    Ok((logfile, positional))
}

fn parse_serve(args: &[String]) -> Result<Command, String> {
    let mut directory = None;
    let mut config_path = None;
    let mut overrides = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--help" || arg == "-h" {
            return Ok(Command::Help);
        }
        if arg == "--dir" {
            if directory.is_some() {
                return Err("--dir may only be specified once".to_string());
            }
            let value = required_following_value(args, index, "--dir")?;
            directory = Some(PathBuf::from(value));
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--dir=") {
            if directory.is_some() {
                return Err("--dir may only be specified once".to_string());
            }
            if value.is_empty() {
                return Err("--dir requires a non-empty path".to_string());
            }
            directory = Some(PathBuf::from(value));
            index += 1;
            continue;
        }
        if let Some(option) = arg.strip_prefix("--") {
            let (name, value, consumed) = if let Some((name, value)) = option.split_once('=') {
                (name, value.to_string(), 1)
            } else {
                let value = required_following_value(args, index, &format!("--{option}"))?;
                (option, value.to_string(), 2)
            };
            overrides.push(ConfigOverride::new(name, value)?);
            index += consumed;
            continue;
        }
        if config_path.is_some() {
            return Err("serve accepts only one configuration path".to_string());
        }
        config_path = Some(PathBuf::from(arg));
        index += 1;
    }

    let target = match (directory, config_path) {
        (Some(directory), None) => ServeTarget::Directory(directory),
        (None, Some(path)) => ServeTarget::Config(path),
        (Some(_), Some(_)) => {
            return Err("serve accepts either --dir or CONFIG_PATH, not both".to_string());
        }
        (None, None) => return Err("serve requires --dir DIR or CONFIG_PATH".to_string()),
    };
    Ok(Command::Serve { target, overrides })
}

fn required_following_value<'a>(
    args: &'a [String],
    index: usize,
    option: &str,
) -> Result<&'a str, String> {
    let value = args
        .get(index + 1)
        .ok_or_else(|| format!("{option} requires a value"))?;
    if value.is_empty() || value.starts_with("--") {
        return Err(format!("{option} requires a value"));
    }
    Ok(value)
}

pub(super) fn usage() -> String {
    let mut usage = String::from(
        "usage:\n\
         \x20 chatt-server serve --dir DIR [--SECTION.FIELD=VALUE ...]\n\
         \x20 chatt-server serve CONFIG_PATH [--SECTION.FIELD=VALUE ...]\n\
         \x20 chatt-server init-config CONFIG_PATH\n\
         \x20 chatt-server invite USER\n\
         \x20 chatt-server mls storage-status\n\
         \x20 chatt-server mls compact\n\n\
         serve configuration overrides:\n",
    );
    for spec in CONFIG_OPTION_SPECS {
        usage.push_str(&format!(
            "  --{}={}  {}\n",
            spec.name, spec.value_name, spec.description
        ));
    }
    usage.push_str(
        "\nglobal options:\n\
         \x20 --logfile=PATH  write kvlog output to a file\n\
         \x20 --help          show this help\n",
    );
    usage
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(args: &[&str]) -> Vec<String> {
        std::iter::once("chatt-server")
            .chain(args.iter().copied())
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn parses_directory_serve_with_dotted_overrides() {
        let parsed = parse(&args(&[
            "serve",
            "--dir",
            "./instance",
            "--network.tcp-addr=127.0.0.1:42000",
            "--network.p2p-enabled",
            "false",
        ]))
        .unwrap();

        let Command::Serve { target, overrides } = parsed.command else {
            panic!("expected serve");
        };
        assert_eq!(target, ServeTarget::Directory(PathBuf::from("./instance")));
        assert_eq!(
            overrides
                .iter()
                .map(ConfigOverride::name)
                .collect::<Vec<_>>(),
            ["network.tcp-addr", "network.p2p-enabled"]
        );
    }

    #[test]
    fn parses_explicit_config_with_override_and_logfile() {
        let parsed = parse(&args(&[
            "--logfile=/tmp/server.kvlog",
            "serve",
            "server.toml",
            "--security.public=true",
        ]))
        .unwrap();

        assert_eq!(parsed.logfile.as_deref(), Some("/tmp/server.kvlog"));
        let Command::Serve { target, overrides } = parsed.command else {
            panic!("expected serve");
        };
        assert_eq!(target, ServeTarget::Config(PathBuf::from("server.toml")));
        assert_eq!(overrides[0].name(), "security.public");
    }

    #[test]
    fn rejects_array_and_unknown_options() {
        let error = parse(&args(&["serve", "--dir=x", "--rooms.0.name=lobby"])).unwrap_err();
        assert!(error.contains("unknown server configuration option"));
        assert!(error.contains("rooms.0.name"));
    }

    #[test]
    fn rejects_identity_seed_override() {
        let error = parse(&args(&[
            "serve",
            "--dir=x",
            "--security.server-identity-seed=secret",
        ]))
        .unwrap_err();

        assert!(error.contains("unknown server configuration option"));
        assert!(error.contains("security.server-identity-seed"));
    }

    #[test]
    fn rejects_conflicting_serve_targets() {
        let error = parse(&args(&["serve", "server.toml", "--dir", "instance"])).unwrap_err();
        assert!(error.contains("either --dir or CONFIG_PATH"));
    }

    #[test]
    fn help_lists_every_config_option() {
        let usage = usage();
        for spec in CONFIG_OPTION_SPECS {
            assert!(usage.contains(&format!("--{}=", spec.name)));
        }
    }
}
