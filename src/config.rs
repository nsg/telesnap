use std::{env, fs, net::SocketAddr, os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};

#[derive(Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub api_token: String,
    pub state_path: PathBuf,
    pub download_dir: PathBuf,
    pub max_download_bytes: u64,
    pub max_lifetime: Duration,
    pub command_timeout: Duration,
    pub pending_timeout: Duration,
    pub download_timeout: Duration,
    pub allow_private_urls: bool,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let api_token = env::var("TELESNAP_API_TOKEN")
            .map_err(|_| "TELESNAP_API_TOKEN must be set".to_owned())?;
        if api_token.len() < 32 || !api_token.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(
                "TELESNAP_API_TOKEN must contain at least 32 visible ASCII bytes".to_owned(),
            );
        }

        let command_timeout_seconds = parse_positive_env("TELESNAP_COMMAND_TIMEOUT_SECONDS", 300)?;
        let default_pending_timeout = command_timeout_seconds
            .checked_add(300)
            .ok_or_else(|| "TELESNAP_COMMAND_TIMEOUT_SECONDS is too large".to_owned())?;
        let pending_timeout_seconds =
            parse_positive_env("TELESNAP_PENDING_TIMEOUT_SECONDS", default_pending_timeout)?;
        if pending_timeout_seconds < command_timeout_seconds {
            return Err(
                "TELESNAP_PENDING_TIMEOUT_SECONDS must be at least TELESNAP_COMMAND_TIMEOUT_SECONDS"
                    .to_owned(),
            );
        }

        Ok(Self {
            bind: parse_env("TELESNAP_BIND", "127.0.0.1:8080")?,
            api_token,
            state_path: env::var_os("TELESNAP_STATE_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| "/var/lib/telesnap/expirations.json".into()),
            download_dir: env::var_os("TELESNAP_DOWNLOAD_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| "/var/lib/telesnap/downloads".into()),
            max_download_bytes: parse_positive_env(
                "TELESNAP_MAX_DOWNLOAD_BYTES",
                512 * 1024 * 1024,
            )?,
            max_lifetime: Duration::from_secs(parse_positive_env(
                "TELESNAP_MAX_LIFETIME_SECONDS",
                86_400,
            )?),
            command_timeout: Duration::from_secs(command_timeout_seconds),
            pending_timeout: Duration::from_secs(pending_timeout_seconds),
            download_timeout: Duration::from_secs(parse_positive_env(
                "TELESNAP_DOWNLOAD_TIMEOUT_SECONDS",
                300,
            )?),
            allow_private_urls: parse_env("TELESNAP_ALLOW_PRIVATE_URLS", "false")?,
        })
    }

    pub fn prepare_directories(&self) -> Result<(), std::io::Error> {
        if let Some(parent) = self.state_path.parent() {
            let created = !parent.exists();
            fs::create_dir_all(parent)?;
            if created {
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            }
        }
        let created = !self.download_dir.exists();
        fs::create_dir_all(&self.download_dir)?;
        if created {
            fs::set_permissions(&self.download_dir, fs::Permissions::from_mode(0o700))?;
        }
        self.remove_stale_downloads()?;
        Ok(())
    }

    fn remove_stale_downloads(&self) -> Result<(), std::io::Error> {
        for entry in fs::read_dir(&self.download_dir)? {
            let entry = entry?;
            let path = entry.path();
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if path.extension().and_then(|extension| extension.to_str()) == Some("snap")
                && uuid::Uuid::parse_str(stem).is_ok()
            {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }
}

pub fn ensure_root() -> Result<(), String> {
    let status = fs::read_to_string("/proc/self/status")
        .map_err(|error| format!("cannot determine effective uid: {error}"))?;
    let effective_uid = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|uids| uids.split_whitespace().nth(1))
        .and_then(|uid| uid.parse::<u32>().ok())
        .ok_or_else(|| "cannot parse effective uid from /proc/self/status".to_owned())?;

    if effective_uid != 0 {
        return Err("telesnap must run as root".to_owned());
    }
    Ok(())
}

fn parse_env<T>(name: &str, default: &str) -> Result<T, String>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    env::var(name)
        .unwrap_or_else(|_| default.to_owned())
        .parse()
        .map_err(|error| format!("invalid {name}: {error}"))
}

fn parse_positive_env(name: &str, default: u64) -> Result<u64, String> {
    let value = match env::var(name) {
        Ok(value) => value
            .parse::<u64>()
            .map_err(|error| format!("invalid {name}: {error}"))?,
        Err(env::VarError::NotPresent) => default,
        Err(error) => return Err(format!("invalid {name}: {error}")),
    };
    if value == 0 {
        return Err(format!("{name} must be greater than zero"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::ensure_root;

    #[test]
    fn root_check_reads_the_effective_uid_on_linux() {
        if let Err(error) = ensure_root() {
            assert_eq!(error, "telesnap must run as root");
        }
    }
}
