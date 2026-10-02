//! The user's saved settings: `$PERF_VIZ_CONFIG`, else `$XDG_CONFIG_HOME/perf-viz/config.toml`,
//! else `~/.config/perf-viz/config.toml`.
//!
//! Every field is optional; a missing one means "use the default". Command-line flags override the
//! file for that run without changing it. The app saves the file when the user changes a setting.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use perf_profile::{load, symbolize::DwarfMode};

use crate::fmt::Cost;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    System,
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum TabName {
    Summary,
    Flame,
    Topdown,
    Bottomup,
    Callercallee,
    Timeline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dwarf {
    /// Use DWARF (inlined frames, file:line) where a module has debug info.
    Auto,
    /// Never load DWARF: function names only, but faster.
    Never,
}

/// How the file is read and turned into a profile (the `[reader]` table). Unset values keep the
/// loader's own defaults, which are tuned for the machine's core count.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReaderConfig {
    /// Threads reading the file in parallel, each on its own region.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readers: Option<usize>,
    /// Threads parsing and unwinding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threads: Option<usize>,
    /// Size of one region of the file, in MiB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region_mb: Option<u64>,
    /// How many regions may be open at once (memory use is about `region_mb` times this).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_flight: Option<usize>,
    /// Process a region only after this many following regions are committed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lookahead: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dwarf: Option<Dwarf>,
    /// Drop the file from the page cache before loading (to time a cold read).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cold: Option<bool>,
}

impl ReaderConfig {
    fn is_empty(&self) -> bool {
        *self == ReaderConfig::default()
    }

    /// No reader setting has been chosen (by hand or by the tuner).
    pub fn is_empty_settings(&self) -> bool {
        self.is_empty()
    }

    fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("readers", self.readers.map(|value| value as u64)),
            ("threads", self.threads.map(|value| value as u64)),
            ("region_mb", self.region_mb),
            ("in_flight", self.in_flight.map(|value| value as u64)),
        ] {
            if value == Some(0) {
                return Err(format!("reader.{name} must be at least 1"));
            }
        }
        Ok(())
    }

    /// Apply the configured values on top of `load_options` (which holds the defaults).
    pub fn apply(&self, load_options: &mut load::Options) {
        if let Some(value) = self.readers {
            load_options.readers = value;
        }
        if let Some(value) = self.threads {
            load_options.workers = value;
        }
        if let Some(value) = self.region_mb {
            load_options.region_mb = value;
        }
        if let Some(value) = self.in_flight {
            load_options.in_flight = value;
        }
        if let Some(value) = self.lookahead {
            load_options.lookahead = value;
        }
        if let Some(dwarf) = self.dwarf {
            load_options.dwarf = match dwarf {
                Dwarf::Auto => DwarfMode::Auto,
                Dwarf::Never => DwarfMode::Never,
            };
        }
        if let Some(cold) = self.cold {
            load_options.cold = cold;
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// `system` (follow the desktop), `light` or `dark`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<Theme>,
    /// What the views add up: `samples` or `cycles`. Unset: cycles if the recording has periods.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    /// Show the call trees with suspect stack tails cut off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncate_suspect_tails: Option<bool>,
    /// The tab shown when a profile opens (the last one used).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab: Option<TabName>,
    /// Set once the user has been asked whether to tune the reader (asked only the first time a file opens).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tuning_prompted: Option<bool>,
    /// The `[reader]` table: how the perf.data reader is configured.
    #[serde(skip_serializing_if = "ReaderConfig::is_empty")]
    pub reader: ReaderConfig,
}

impl Config {
    pub fn parse(text: &str) -> Result<Config, String> {
        let config: Config = toml::from_str(text).map_err(|error| error.to_string())?;
        config.reader.validate()?;
        Ok(config)
    }

    pub fn to_toml(&self) -> String {
        let body = toml::to_string_pretty(self).expect("a config always serializes");
        format!(
            "# perf-viz settings. Written by perf-viz when you change one; edit freely.\n# theme = \"system\" | \"light\" | \"dark\"; cost = \"samples\" | \"cycles\";\n# tab = \"summary\" | \"flame\" | \"topdown\" | \"bottomup\" | \"callercallee\" | \"timeline\"\n# [reader]: readers, threads, region_mb, in_flight, lookahead, dwarf = \"auto\" | \"never\", cold\n{body}"
        )
    }
}

/// Where the config file lives, if a location can be determined.
pub fn path() -> Option<PathBuf> {
    path_from(|k| {
        std::env::var_os(k)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    })
}

fn path_from(var: impl Fn(&str) -> Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = var("PERF_VIZ_CONFIG") {
        return Some(path);
    }
    let base = var("XDG_CONFIG_HOME").or_else(|| var("HOME").map(|home| home.join(".config")))?;
    Some(base.join("perf-viz").join("config.toml"))
}

/// Load the settings. A missing file is normal (defaults); an unreadable or invalid one is reported
/// on stderr and ignored, so a typo never stops the viewer from starting.
pub fn load() -> Config {
    let Some(path) = path() else {
        return Config::default();
    };
    load_from(&path)
}

pub fn load_from(path: &Path) -> Config {
    match fs::read_to_string(path) {
        Ok(text) => Config::parse(&text).unwrap_or_else(|error| {
            eprintln!("perf-viz: ignoring {}: {error}", path.display());
            Config::default()
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(error) => {
            eprintln!("perf-viz: cannot read {}: {error}", path.display());
            Config::default()
        }
    }
}

/// Write the settings atomically (temp file, then rename), creating the directory if needed.
pub fn save_to(path: &Path, config: &Config) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, config.to_toml())?;
    fs::rename(&tmp, path)
}

pub fn save(config: &Config) {
    let Some(path) = path() else { return };
    if let Err(error) = save_to(&path, config) {
        eprintln!("perf-viz: cannot save {}: {error}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_means_all_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn settings_round_trip() {
        let config = Config {
            theme: Some(Theme::Light),
            cost: Some(Cost::Samples),
            truncate_suspect_tails: Some(true),
            tab: Some(TabName::Callercallee),
            reader: ReaderConfig {
                readers: Some(3),
                threads: Some(6),
                region_mb: Some(64),
                in_flight: Some(4),
                lookahead: Some(2),
                dwarf: Some(Dwarf::Never),
                cold: Some(true),
            },
            tuning_prompted: Some(true),
        };
        assert_eq!(Config::parse(&config.to_toml()).unwrap(), config);
        assert_eq!(
            Config::parse(&Config::default().to_toml()).unwrap(),
            Config::default(),
            "comments only"
        );
    }

    #[test]
    fn values_are_lowercase_words() {
        let config = Config::parse(
            "theme = \"dark\"\ncost = \"cycles\"\ntab = \"flame\"\ntruncate_suspect_tails = false",
        )
        .unwrap();
        assert_eq!(
            (
                config.theme,
                config.cost,
                config.tab,
                config.truncate_suspect_tails
            ),
            (
                Some(Theme::Dark),
                Some(Cost::Cycles),
                Some(TabName::Flame),
                Some(false)
            )
        );
    }

    #[test]
    fn reader_table_is_parsed_and_applied_over_the_defaults() {
        let config = Config::parse("theme = \"dark\"\n[reader]\nreaders = 2\nregion_mb = 32\ndwarf = \"never\"\ncold = true").unwrap();
        let mut load_options = load::Options::default();
        let defaults = load_options.clone();
        config.reader.apply(&mut load_options);
        assert_eq!(
            (
                load_options.readers,
                load_options.region_mb,
                load_options.dwarf,
                load_options.cold
            ),
            (2, 32, DwarfMode::Never, true)
        );
        assert_eq!(
            (
                load_options.workers,
                load_options.in_flight,
                load_options.lookahead
            ),
            (defaults.workers, defaults.in_flight, defaults.lookahead),
            "unset values keep the defaults"
        );
        assert!(Config::default().reader.is_empty());
        assert!(
            Config::default()
                .to_toml()
                .lines()
                .all(|line| line.starts_with('#')),
            "an empty config writes only comments"
        );
    }

    #[test]
    fn zero_sized_reader_settings_are_rejected() {
        for bad in [
            "readers = 0",
            "threads = 0",
            "region_mb = 0",
            "in_flight = 0",
        ] {
            assert!(Config::parse(&format!("[reader]\n{bad}")).is_err(), "{bad}");
        }
        assert!(
            Config::parse("[reader]\nlookahead = 0").is_ok(),
            "no lookahead is valid"
        );
        assert!(Config::parse("[reader]\nreaderz = 2").is_err());
    }

    #[test]
    fn bad_values_and_unknown_keys_are_errors() {
        assert!(Config::parse("theme = \"purple\"").is_err());
        assert!(
            Config::parse("thme = \"dark\"").is_err(),
            "a typo'd key is reported, not silently ignored"
        );
    }

    #[test]
    fn location_prefers_the_override_then_xdg_then_home() {
        let env = |pairs: &'static [(&str, &str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|pair| pair.0 == k)
                    .map(|pair| PathBuf::from(pair.1))
            }
        };
        assert_eq!(
            path_from(env(&[("PERF_VIZ_CONFIG", "/x/c.toml"), ("HOME", "/h")])),
            Some("/x/c.toml".into())
        );
        assert_eq!(
            path_from(env(&[("XDG_CONFIG_HOME", "/xdg"), ("HOME", "/h")])),
            Some("/xdg/perf-viz/config.toml".into())
        );
        assert_eq!(
            path_from(env(&[("HOME", "/h")])),
            Some("/h/.config/perf-viz/config.toml".into())
        );
        assert_eq!(path_from(env(&[])), None);
    }

    #[test]
    fn save_creates_directories_and_load_reads_it_back() {
        let dir = std::env::temp_dir().join(format!("perf-viz-test-{}", std::process::id()));
        let file = dir.join("nested").join("config.toml");
        let config = Config {
            theme: Some(Theme::Dark),
            ..Default::default()
        };
        save_to(&file, &config).unwrap();
        assert_eq!(load_from(&file), config);
        assert_eq!(load_from(&dir.join("missing.toml")), Config::default());
        fs::write(&file, "theme = 3").unwrap();
        assert_eq!(
            load_from(&file),
            Config::default(),
            "invalid file is ignored"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
