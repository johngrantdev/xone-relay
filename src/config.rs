use std::path::PathBuf;

pub const DEFAULT_PORT: u16 = 47998;
pub const DEFAULT_DEVICE_NAME: &str = "Microsoft Xbox Controller";
const CONFIG: &str = "/etc/xone-relay.conf";

pub struct Config {
    pub receiver_address: Option<String>,
    pub port: u16,
    path: PathBuf,
}

impl Config {
    pub fn load() -> Result<Self, String> {
        let path = std::env::var_os("XONE_RELAY_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(CONFIG));
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        Self::parse(&text, path)
    }

    pub fn require_receiver_address(&self) -> Result<&str, String> {
        self.receiver_address
            .as_deref()
            .ok_or_else(|| format!("no RECEIVER_ADDRESS in {}", self.path.display()))
    }

    fn parse(text: &str, path: PathBuf) -> Result<Self, String> {
        let mut config = Self {
            receiver_address: None,
            port: DEFAULT_PORT,
            path,
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().trim_matches('"');
            match key.trim() {
                "RECEIVER_ADDRESS" => config.receiver_address = Some(value.to_string()),
                "PORT" => {
                    config.port = value.parse().map_err(|_| {
                        format!("invalid PORT in {}: {value}", config.path.display())
                    })?;
                }
                _ => {}
            }
        }
        if config.port == 0 {
            return Err(format!("invalid PORT in {}: 0", config.path.display()));
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Result<Config, String> {
        Config::parse(text, PathBuf::from("/tmp/xone-relay.conf"))
    }

    #[test]
    fn parses_the_shared_keys() {
        let config = parsed("RECEIVER_ADDRESS=192.0.2.10\nPORT=48000\n").expect("valid config");
        assert_eq!(config.receiver_address.as_deref(), Some("192.0.2.10"));
        assert_eq!(config.port, 48000);
    }

    #[test]
    fn defaults_when_keys_are_absent() {
        let config = parsed("").expect("empty config");
        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.receiver_address, None);
    }

    #[test]
    fn rejects_a_zero_port() {
        assert!(parsed("PORT=0\n").is_err());
    }
}
