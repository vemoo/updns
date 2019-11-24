use futures::future::{BoxFuture, FutureExt};
use regex::Regex;
use std::{
    borrow::Cow,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    result,
    slice::Iter,
};
use tokio::{
    fs::{create_dir_all, File, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt, Result},
};

lazy_static! {
    static ref COMMENT_REGEX: Regex = Regex::new("#.*$").unwrap();
}

#[derive(Debug)]
pub struct Invalid {
    pub line: usize,
    pub source: String,
    pub kind: InvalidType,
}

#[derive(Debug)]
pub enum InvalidType {
    Regex,
    SocketAddr,
    IpAddr,
    Timeout,
    Other,
}

impl InvalidType {
    pub fn text(&self) -> &str {
        match self {
            InvalidType::SocketAddr => "Cannot parse socket address",
            InvalidType::IpAddr => "Cannot parse ip address",
            InvalidType::Regex => "Cannot parse regular expression",
            InvalidType::Timeout => "Cannot parse timeout",
            InvalidType::Other => "Invalid line",
        }
    }
}

#[derive(Debug)]
pub struct Hosts {
    record: Vec<(Host, IpAddr)>,
}

impl Hosts {
    pub fn new() -> Hosts {
        Hosts { record: Vec::new() }
    }

    fn push(&mut self, record: (Host, IpAddr)) {
        self.record.push(record);
    }

    fn extend(&mut self, hosts: Hosts) {
        for item in hosts.record {
            self.record.push(item);
        }
    }

    pub fn iter(&mut self) -> Iter<(Host, IpAddr)> {
        self.record.iter()
    }

    pub fn get(&self, domain: &str) -> Option<&IpAddr> {
        for (reg, ip) in &self.record {
            if reg.is_match(domain) {
                return Some(ip);
            }
        }
        None
    }
}

// domain match
#[derive(Debug)]
pub struct Host(MatchMode);

#[derive(Debug)]
enum MatchMode {
    Text(String),
    Regex(Regex),
}

impl Host {
    fn new(domain: &str) -> result::Result<Host, regex::Error> {
        // example.com
        if Self::is_text(domain) {
            return Ok(Host(MatchMode::Text(domain.to_string())));
        }

        // *.example.com
        if Self::is_wildcard(domain) {
            let s = format!("^{}$", domain.replace(".", r"\.").replace("*", r"[^.]+"));
            return Ok(Host(MatchMode::Regex(Regex::new(&s)?)));
        }

        // use regex
        Ok(Host(MatchMode::Regex(Regex::new(domain)?)))
    }

    fn is_text(domain: &str) -> bool {
        const ALLOW: &str = "abcdefghijklmnopqrstuvwxyz0123456789-.";
        domain.chars().all(|item| ALLOW.chars().any(|c| item == c))
    }

    fn is_wildcard(domain: &str) -> bool {
        const ALLOW: &str = "abcdefghijklmnopqrstuvwxyz0123456789-.*";
        domain.chars().all(|item| ALLOW.chars().any(|c| item == c))
    }

    pub fn is_match(&self, domain: &str) -> bool {
        match &self.0 {
            MatchMode::Text(text) => text == domain,
            MatchMode::Regex(reg) => reg.is_match(domain),
        }
    }

    pub fn as_str(&self) -> &str {
        match &self.0 {
            MatchMode::Text(text) => text,
            MatchMode::Regex(reg) => reg.as_str(),
        }
    }
}

#[derive(Debug)]
pub struct ParseConfig {
    pub bind: Vec<SocketAddr>,
    pub proxy: Vec<SocketAddr>,
    pub hosts: Hosts,
    pub timeout: Option<u64>,
    pub invalid: Vec<Invalid>,
}

impl ParseConfig {
    fn new() -> ParseConfig {
        ParseConfig {
            hosts: Hosts::new(),
            bind: Vec::new(),
            proxy: Vec::new(),
            invalid: Vec::new(),
            timeout: None,
        }
    }

    fn extend(&mut self, other: Self) {
        self.bind.extend(other.bind);
        self.proxy.extend(other.proxy);
        self.hosts.extend(other.hosts);
        self.invalid.extend(other.invalid);
        if other.timeout.is_some() {
            self.timeout = other.timeout;
        }
    }
}

#[derive(Debug)]
pub struct Config {
    path: PathBuf,
    file: File,
}

impl Config {
    pub async fn new<P: AsRef<Path>>(path: P) -> Result<Config> {
        let path = path.as_ref();

        if let Some(dir) = path.parent() {
            create_dir_all(dir).await?;
        }

        Ok(Config {
            file: OpenOptions::new()
                .read(true)
                .append(true)
                .create(true)
                .open(path)
                .await?,
            path: path.to_path_buf(),
        })
    }

    async fn read_to_string(&mut self) -> Result<String> {
        let mut content = String::new();
        self.file.read_to_string(&mut content).await?;
        Ok(content)
    }

    pub async fn add(&mut self, domain: &str, ip: &str) -> Result<usize> {
        if self.read_to_string().await?.ends_with('\n') {
            self.file
                .write(format!("{}  {}", domain, ip).as_bytes())
                .await
        } else {
            self.file
                .write(format!("\n{}  {}", domain, ip).as_bytes())
                .await
        }
    }

    fn split(text: &str) -> Option<(&str, &str)> {
        let mut text = text.split_ascii_whitespace();

        if let (Some(left), Some(right)) = (text.next(), text.next()) {
            if text.next().is_none() {
                return Some((left, right));
            }
        }

        None
    }

    fn parse_host(key: &str, value: &str) -> result::Result<(Host, IpAddr), InvalidType> {
        // match host
        // example.com 0.0.0.0
        // 0.0.0.0 example.com

        // ip domain
        if let Ok(ip) = key.parse() {
            return Host::new(value)
                .map(|host| (host, ip))
                .map_err(|_| InvalidType::Regex);
        }

        // domain ip
        if let Ok(ip) = value.parse() {
            return Host::new(key)
                .map(|host| (host, ip))
                .map_err(|_| InvalidType::Regex);
        }

        Err(InvalidType::IpAddr)
    }

    pub fn parse(mut self) -> BoxFuture<'static, Result<ParseConfig>> {
        async move {
            let mut parse = ParseConfig::new();

            for (n, line) in self.read_to_string().await?.lines().enumerate() {
                if line.is_empty() {
                    continue;
                }

                // remove comment
                // example # ... -> example
                let line: Cow<str> = COMMENT_REGEX.replace(line, "");

                if line.trim().is_empty() {
                    continue;
                }

                macro_rules! invalid {
                    ($type: expr) => {{
                        parse.invalid.push(Invalid {
                            line: n + 1,
                            source: line.to_string(),
                            kind: $type,
                        });
                        continue;
                    }};
                }

                let (key, value) = match Self::split(&line) {
                    Some(d) => d,
                    None => invalid!(InvalidType::Other),
                };

                match key {
                    "bind" => match value.parse::<SocketAddr>() {
                        Ok(addr) => parse.bind.push(addr),
                        Err(_) => invalid!(InvalidType::SocketAddr),
                    },
                    "proxy" => match value.parse::<SocketAddr>() {
                        Ok(addr) => parse.proxy.push(addr),
                        Err(_) => invalid!(InvalidType::SocketAddr),
                    },
                    "timeout" => match value.parse::<u64>() {
                        Ok(timeout) => parse.timeout = Some(timeout),
                        Err(_) => invalid!(InvalidType::Timeout),
                    },
                    "import" => {
                        let mut path = Path::new(value).to_path_buf();
                        if path.is_relative() {
                            if let Some(parent) = self.path.parent() {
                                path = parent.join(path);
                            }
                        }
                        parse.extend(Config::new(path).await?.parse().await?);
                    }
                    _ => match Self::parse_host(key, value) {
                        Ok(record) => parse.hosts.push(record),
                        Err(kind) => invalid!(kind),
                    },
                }
            }

            Ok(parse)
        }
            .boxed()
    }
}

#[cfg(test)]
mod test_host {
    use super::*;

    #[test]
    fn test_create() {}

    #[test]
    fn test_text() {
        let host = Host::new("example.com").unwrap();
        assert!(host.is_match("example.com"));
        assert!(!host.is_match("-example.com"));
        assert!(!host.is_match("example.com.cn"));
    }

    #[test]
    fn test_wildcard() {
        let host = Host::new("*.example.com").unwrap();
        assert!(host.is_match("test.example.com"));
        assert!(!host.is_match("test.example.test"));
        assert!(!host.is_match("test.test.com"));

        let host = Host::new("*.example.*").unwrap();
        assert!(host.is_match("test.example.test"));
        assert!(!host.is_match("example.com"));
        assert!(!host.is_match("test.test.test"));
    }

    #[test]
    fn test_regex() {
        let host = Host::new("^example.com$").unwrap();
        assert!(host.is_match("example.com"));
        assert!(!host.is_match("test.example.com"));
    }
}
