use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::cli::GlobalArgs;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("{path} is not valid TOML: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("could not write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error(
        "no API key. Set api_key_file in your config, or export AUTO_COMMIT_API_KEY. \
         A local endpoint that wants no key still needs one of these set to any value"
    )]
    NoApiKey,

    #[error("no such setting: {0}")]
    UnknownKey(String),

    #[error("{key} does not accept the value {value:?}")]
    BadValue { key: String, value: String },
}

/// One layer of configuration. Every field is optional so that a layer can
/// state only what it overrides.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Layer {
    pub model: Option<String>,
    pub endpoint: Option<String>,
    pub api_key_file: Option<PathBuf>,
    pub endpoint_file: Option<PathBuf>,
    pub context_commits: Option<usize>,
    pub max_diff_bytes: Option<usize>,
    pub max_file_diff_bytes: Option<usize>,
    pub max_tool_calls: Option<u32>,
    pub max_tool_bytes: Option<usize>,
    pub exclude: Option<Vec<String>>,
    pub conventional_commits: Option<bool>,
    pub subject_max_len: Option<usize>,
    pub body: Option<bool>,
    pub candidates: Option<usize>,
    pub show_thinking: Option<bool>,
}

impl Layer {
    fn read(path: &PathBuf) -> Result<Option<Self>, ConfigError> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.clone(),
                    source,
                })
            }
        };

        toml::from_str(&text)
            .map(Some)
            .map_err(|source| ConfigError::Parse {
                path: path.clone(),
                source,
            })
    }

    fn merge(&mut self, over: Layer) {
        macro_rules! take {
            ($($field:ident),* $(,)?) => {
                $(if over.$field.is_some() { self.$field = over.$field; })*
            };
        }

        take!(
            model,
            endpoint,
            api_key_file,
            endpoint_file,
            context_commits,
            max_diff_bytes,
            max_file_diff_bytes,
            max_tool_calls,
            max_tool_bytes,
            exclude,
            conventional_commits,
            subject_max_len,
            body,
            candidates,
            show_thinking,
        );
    }
}

#[derive(Debug)]
pub struct Config {
    pub model: String,
    pub endpoint: String,
    pub api_key_file: Option<PathBuf>,
    pub endpoint_file: Option<PathBuf>,
    pub context_commits: usize,
    pub max_diff_bytes: usize,
    pub max_file_diff_bytes: usize,
    pub max_tool_calls: u32,
    pub max_tool_bytes: usize,
    pub exclude: Vec<String>,
    pub conventional_commits: bool,
    pub subject_max_len: usize,
    pub body: bool,
    pub candidates: usize,
    pub show_thinking: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: "gpt-5.6-luna".into(),
            endpoint: "https://api.openai.com/v1".into(),
            api_key_file: None,
            endpoint_file: None,
            context_commits: 10,
            max_diff_bytes: 60_000,
            max_file_diff_bytes: 12_000,
            max_tool_calls: 2,
            max_tool_bytes: 24_000,
            exclude: [
                "*.lock",
                "*.min.js",
                "*.min.css",
                "*.snap",
                "package-lock.json",
                "pnpm-lock.yaml",
                "yarn.lock",
                "Cargo.lock",
                "go.sum",
                "poetry.lock",
                "vendor/**",
                "dist/**",
            ]
            .iter()
            .map(|glob| (*glob).to_owned())
            .collect(),
            conventional_commits: false,
            subject_max_len: 72,
            body: true,
            candidates: 1,
            show_thinking: false,
        }
    }
}

/// Where the user's own config lives, and where `config set` writes.
pub fn user_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));

    base.join("auto-commit/config.toml")
}

const SYSTEM_PATH: &str = "/etc/auto-commit/config.toml";

impl Config {
    /// System file, then user file, then repository file, then environment,
    /// then flags. Each layer overrides only the fields it sets.
    pub fn load(repo_root: Option<&PathBuf>, args: &GlobalArgs) -> Result<Self, ConfigError> {
        let mut layer = Layer::default();

        for path in [
            PathBuf::from(SYSTEM_PATH),
            user_path(),
            repo_root
                .map(|root| root.join(".auto-commit.toml"))
                .unwrap_or_default(),
        ] {
            if let Some(found) = Layer::read(&path)? {
                layer.merge(found);
            }
        }

        // The OPENAI_ names are read as a fallback because that is what people
        // already have exported, not because this tool is tied to that API.
        let endpoint = args
            .endpoint
            .clone()
            .or_else(|| env_any(&["AUTO_COMMIT_ENDPOINT", "OPENAI_BASE_URL"]));

        layer.merge(Layer {
            model: env_any(&["AUTO_COMMIT_MODEL"]),
            ..Default::default()
        });

        layer.merge(Layer {
            model: args.model.clone(),
            endpoint: endpoint.clone(),
            context_commits: args.context_commits,
            max_tool_calls: args.max_tool_calls,
            candidates: args.candidates,
            show_thinking: args.show_thinking.then_some(true),
            conventional_commits: args.conventional.then_some(true),
            ..Default::default()
        });

        // An endpoint named on the command line or in the environment replaces
        // a configured endpoint_file, which would otherwise shadow it.
        if endpoint.is_some() {
            layer.endpoint_file = None;
        }

        let defaults = Config::default();

        Ok(Config {
            model: layer.model.unwrap_or(defaults.model),
            endpoint: layer.endpoint.unwrap_or(defaults.endpoint),
            api_key_file: layer.api_key_file,
            endpoint_file: layer.endpoint_file,
            context_commits: layer.context_commits.unwrap_or(defaults.context_commits),
            max_diff_bytes: layer.max_diff_bytes.unwrap_or(defaults.max_diff_bytes),
            max_file_diff_bytes: layer
                .max_file_diff_bytes
                .unwrap_or(defaults.max_file_diff_bytes),
            max_tool_calls: layer.max_tool_calls.unwrap_or(defaults.max_tool_calls),
            max_tool_bytes: layer.max_tool_bytes.unwrap_or(defaults.max_tool_bytes),
            exclude: layer.exclude.unwrap_or(defaults.exclude),
            conventional_commits: layer
                .conventional_commits
                .unwrap_or(defaults.conventional_commits),
            subject_max_len: layer.subject_max_len.unwrap_or(defaults.subject_max_len),
            body: layer.body.unwrap_or(defaults.body),
            candidates: layer.candidates.unwrap_or(defaults.candidates),
            show_thinking: layer.show_thinking.unwrap_or(defaults.show_thinking),
        })
    }

    /// The key itself, from the file the config points at or from the
    /// environment. Never logged and never put in an event.
    pub fn api_key(&self) -> Result<String, ConfigError> {
        match &self.api_key_file {
            Some(path) => read_secret(path),
            None => {
                env_any(&["AUTO_COMMIT_API_KEY", "OPENAI_API_KEY"]).ok_or(ConfigError::NoApiKey)
            }
        }
    }

    /// The endpoint can be a secret too. A gateway URL often carries a tenant
    /// or a token in its path, so it gets the same file treatment as the key
    /// and is read at run time rather than written into any config file.
    ///
    /// The field of the same name holds the literal from the config; this
    /// reads the file when one was given instead.
    pub fn endpoint(&self) -> Result<String, ConfigError> {
        match &self.endpoint_file {
            Some(path) => read_secret(path),
            None => Ok(self.endpoint.clone()),
        }
    }

    pub fn get(&self, key: &str) -> Result<String, ConfigError> {
        Ok(match key {
            "model" => self.model.clone(),
            // A value that came from a file is reported as the path it came
            // from. `config get` must never be a way to read a secret out.
            "endpoint" => match &self.endpoint_file {
                Some(path) => format!("<read from {}>", path.display()),
                None => self.endpoint.clone(),
            },
            "endpoint_file" => show_path(self.endpoint_file.as_deref()),
            "api_key_file" => show_path(self.api_key_file.as_deref()),
            "context_commits" => self.context_commits.to_string(),
            "max_diff_bytes" => self.max_diff_bytes.to_string(),
            "max_file_diff_bytes" => self.max_file_diff_bytes.to_string(),
            "max_tool_calls" => self.max_tool_calls.to_string(),
            "max_tool_bytes" => self.max_tool_bytes.to_string(),
            "exclude" => self.exclude.join(", "),
            "conventional_commits" => self.conventional_commits.to_string(),
            "subject_max_len" => self.subject_max_len.to_string(),
            "body" => self.body.to_string(),
            "candidates" => self.candidates.to_string(),
            "show_thinking" => self.show_thinking.to_string(),
            other => return Err(ConfigError::UnknownKey(other.to_owned())),
        })
    }

    pub const KEYS: &'static [&'static str] = &[
        "model",
        "endpoint",
        "api_key_file",
        "endpoint_file",
        "context_commits",
        "max_diff_bytes",
        "max_file_diff_bytes",
        "max_tool_calls",
        "max_tool_bytes",
        "exclude",
        "conventional_commits",
        "subject_max_len",
        "body",
        "candidates",
        "show_thinking",
    ];
}

/// Write one key into the user config file, leaving the other layers alone.
pub fn set(key: &str, value: &str) -> Result<PathBuf, ConfigError> {
    if !Config::KEYS.contains(&key) {
        return Err(ConfigError::UnknownKey(key.to_owned()));
    }

    let path = user_path();

    let mut table: toml::Table = match fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source,
        })?,
        Err(source) if source.kind() == io::ErrorKind::NotFound => toml::Table::new(),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.clone(),
                source,
            })
        }
    };

    table.insert(key.to_owned(), parse_value(key, value)?);

    let rendered = toml::to_string_pretty(&table).expect("a TOML table always serializes");

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    fs::write(&path, rendered).map_err(|source| ConfigError::Write {
        path: path.clone(),
        source,
    })?;

    Ok(path)
}

/// The config file is typed, so a string from the command line has to be
/// turned into the type that key expects before it is written.
fn parse_value(key: &str, value: &str) -> Result<toml::Value, ConfigError> {
    let bad = || ConfigError::BadValue {
        key: key.to_owned(),
        value: value.to_owned(),
    };

    Ok(match key {
        "model" | "endpoint" | "api_key_file" | "endpoint_file" => {
            toml::Value::String(value.to_owned())
        }
        "conventional_commits" | "body" => toml::Value::Boolean(value.parse().map_err(|_| bad())?),
        "exclude" => toml::Value::Array(
            value
                .split(',')
                .map(|glob| toml::Value::String(glob.trim().to_owned()))
                .collect(),
        ),
        _ => toml::Value::Integer(value.parse().map_err(|_| bad())?),
    })
}

/// The first of these variables that is set to something other than
/// whitespace. Later names are compatibility fallbacks.
fn env_any(names: &[&str]) -> Option<String> {
    names
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_owned())
}

/// Reads a value that was kept out of every config file on purpose. The
/// content is returned to the caller and never rendered anywhere else.
fn read_secret(path: &Path) -> Result<String, ConfigError> {
    let value = fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;

    Ok(value.trim().to_owned())
}

fn show_path(path: Option<&Path>) -> String {
    path.map(|path| path.display().to_string())
        .unwrap_or_default()
}
