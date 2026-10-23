/*!
 * The tunable settings (#304): one registry of each setting's name, env var, range, default and
 * whether a change is live or needs a restart. A value comes from an admin override stored in
 * index.sqlite, then the env (iac), then the default; a reset drops the override.
 * ETH_RPC_URL holds an API key: its value is never reported, logged or put in an error.
 */
use crate::ens::host_of;
use crate::store::{now_secs, Store};
use log::{info, warn};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, RwLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    CrawlWorkers,
    GatewayRequestsPerMin,
    DiscoveryPerHour,
    HealthMinRate,
    HealthGraceSecs,
    IndexMaxAgeDays,
    IndexCacheMb,
    PauseWindow,
    PauseFailures,
    PauseMinSecs,
    PauseMaxSecs,
    PageAttempts,
    PageTimeoutSecs,
    PageMaxBytes,
    NameTimeoutSecs,
    EthLogsMaxBlocks,
    EthRequestSecs,
    EthRpcUrl,
}

/// Whether a change takes effect at once or at the next start
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applies {
    Live,
    Restart,
}

/// Where a setting's value came from; `Env` is what iac sets
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Admin,
    Env,
    Default,
}

impl Applies {
    pub fn label(self) -> &'static str {
        match self {
            Applies::Live => "live",
            Applies::Restart => "restart",
        }
    }
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Source::Admin => "admin",
            Source::Env => "iac",
            Source::Default => "default",
        }
    }
}

struct Spec {
    name: &'static str,
    env_var: &'static str,
    label: &'static str,
    // unused for the secret; gateway_requests_per_min's is for an own node (see default_of)
    default: u64,
    min: u64,
    max: u64,
    applies: Applies,
}

const fn spec(
    name: &'static str,
    env_var: &'static str,
    label: &'static str,
    default: u64,
    (min, max): (u64, u64),
    applies: Applies,
) -> Spec {
    Spec {
        name,
        env_var,
        label,
        default,
        min,
        max,
        applies,
    }
}

use Applies::{Live, Restart};

// in Key's order: Key::spec indexes it
const SPECS: [Spec; 18] = [
    spec(
        "crawl_workers",
        "CRAWL_WORKERS",
        "Crawl workers: pages fetched at once",
        10,
        (1, 500),
        Live,
    ),
    spec(
        "gateway_requests_per_min",
        "GATEWAY_REQUESTS_PER_MIN",
        "Gateway requests a minute, across all workers (0: no limit)",
        0,
        (0, 100_000),
        Live,
    ),
    spec(
        "discovery_per_hour",
        "DISCOVERY_PER_HOUR",
        "Discovered roots queued an hour (0: no limit)",
        500,
        (0, 100_000),
        Live,
    ),
    spec(
        "health_min_rate",
        "HEALTH_MIN_RATE",
        "Lowest healthy indexing rate, pages a minute (0: check off)",
        500,
        (0, 1_000_000),
        Live,
    ),
    spec(
        "health_grace_secs",
        "HEALTH_GRACE_SECS",
        "Seconds after a start before /health checks the rate",
        900,
        (0, 86_400),
        Live,
    ),
    spec(
        "index_max_age_days",
        "INDEX_MAX_AGE_DAYS",
        "Days a page stays indexed before it expires",
        30,
        (1, 3650),
        Live,
    ),
    spec(
        "index_cache_mb",
        "INDEX_CACHE_MB",
        "MB of the index SQLite keeps in memory",
        1024,
        (0, 16_384),
        Live,
    ),
    spec(
        "pause_window",
        "PAUSE_WINDOW",
        "Pauses: a root's recent fetches looked at",
        20,
        (1, 1000),
        Live,
    ),
    spec(
        "pause_failures",
        "PAUSE_FAILURES",
        "Pauses: failures among them that pause the root",
        16,
        (1, 1000),
        Live,
    ),
    spec(
        "pause_min_secs",
        "PAUSE_MIN_SECS",
        "Pauses: the first pause, seconds",
        60,
        (1, 86_400),
        Live,
    ),
    spec(
        "pause_max_secs",
        "PAUSE_MAX_SECS",
        "Pauses: the longest pause, seconds",
        900,
        (1, 86_400),
        Live,
    ),
    spec(
        "page_attempts",
        "PAGE_ATTEMPTS",
        "Fetch attempts before a page is given up on",
        3,
        (1, 100),
        Live,
    ),
    spec(
        "page_timeout_secs",
        "PAGE_TIMEOUT_SECS",
        "Seconds a page fetch may take",
        90,
        (5, 3600),
        Live,
    ),
    spec(
        "page_max_bytes",
        "PAGE_MAX_BYTES",
        "Largest page fetched, bytes",
        2_097_152,
        (65_536, 67_108_864),
        Live,
    ),
    spec(
        "name_timeout_secs",
        "NAME_TIMEOUT_SECS",
        "Seconds a name lookup may take",
        30,
        (5, 3600),
        Live,
    ),
    spec(
        "eth_logs_max_blocks",
        "ETH_LOGS_MAX_BLOCKS",
        "ENS scan: most blocks one eth_getLogs request asks for",
        10_000,
        (1, 100_000),
        Restart,
    ),
    spec(
        "eth_request_secs",
        "ETH_REQUEST_SECS",
        "ENS scan: seconds between requests",
        1,
        (0, 3600),
        Restart,
    ),
    spec(
        "eth_rpc_url",
        "ETH_RPC_URL",
        "ENS scan: the Ethereum RPC URL (holds an API key, so never shown)",
        0,
        (0, 0),
        Restart,
    ),
];

// (lower, upper): the first may not be above the second
const PAIRS: [(Key, Key); 2] = [
    (Key::PauseFailures, Key::PauseWindow),
    (Key::PauseMinSecs, Key::PauseMaxSecs),
];

impl Key {
    pub const ALL: [Key; 18] = [
        Key::CrawlWorkers,
        Key::GatewayRequestsPerMin,
        Key::DiscoveryPerHour,
        Key::HealthMinRate,
        Key::HealthGraceSecs,
        Key::IndexMaxAgeDays,
        Key::IndexCacheMb,
        Key::PauseWindow,
        Key::PauseFailures,
        Key::PauseMinSecs,
        Key::PauseMaxSecs,
        Key::PageAttempts,
        Key::PageTimeoutSecs,
        Key::PageMaxBytes,
        Key::NameTimeoutSecs,
        Key::EthLogsMaxBlocks,
        Key::EthRequestSecs,
        Key::EthRpcUrl,
    ];

    fn spec(self) -> &'static Spec {
        &SPECS[self as usize]
    }

    /// The API id, e.g. `crawl_workers`
    pub fn name(self) -> &'static str {
        self.spec().name
    }

    pub fn from_name(name: &str) -> Option<Key> {
        Key::ALL.into_iter().find(|key| key.name() == name)
    }
}

#[derive(Clone)]
enum Val {
    Num(u64),
    Secret(String),
}

// how an error names a setting: by its env var at startup, by its API id for the admin
#[derive(Clone, Copy)]
enum By {
    Env,
    Admin,
}

fn called(key: Key, by: By) -> &'static str {
    match by {
        By::Env => key.spec().env_var,
        By::Admin => key.name(),
    }
}

// a value for key, or why it isn't one; never echoes the secret
fn check(key: Key, raw: &str, by: By) -> Result<Val, String> {
    let spec = key.spec();
    if key == Key::EthRpcUrl {
        let http = raw.starts_with("http://") || raw.starts_with("https://");
        return match http && host_of(raw).is_some() {
            true => Ok(Val::Secret(raw.to_string())),
            false => Err(format!(
                "{} must be an http(s):// URL with a host",
                called(key, by)
            )),
        };
    }
    match raw.parse::<u64>() {
        Ok(n) if (spec.min..=spec.max).contains(&n) => Ok(Val::Num(n)),
        _ => Err(format!(
            "{} must be a whole number from {} to {}, got {}",
            called(key, by),
            spec.min,
            spec.max,
            shown_raw(raw)
        )),
    }
}

// a bad value as an error shows it; a long one or a URL may be a pasted secret, so isn't echoed
fn shown_raw(raw: &str) -> String {
    match raw.contains("://") || raw.chars().count() > 32 {
        true => "a non-number".to_string(),
        false => format!("{:?}", raw),
    }
}

// one pair rule over a view of the values
fn check_pair(lower: Key, upper: Key, value: &impl Fn(Key) -> u64, by: By) -> Result<(), String> {
    let (low, high) = (value(lower), value(upper));
    if low > high {
        return Err(format!(
            "{} must be at most {}: {} is above {}",
            called(lower, by),
            called(upper, by),
            low,
            high
        ));
    }
    Ok(())
}

fn check_pairs(value: impl Fn(Key) -> u64, by: By) -> Result<(), String> {
    PAIRS
        .into_iter()
        .try_for_each(|(lower, upper)| check_pair(lower, upper, &value, by))
}

// an admin override; one that failed validation when loaded holds the reason and is ignored
struct Override {
    value: Result<Val, String>,
    actor: String,
    set_at: i64,
}

type Overrides = HashMap<Key, Override>;

pub struct Settings {
    // the default gateway rate depends on it
    public_gateway: bool,
    env: HashMap<Key, Val>,
    overrides: RwLock<Overrides>,
    // serializes set and reset, so readers wait only for the in-memory change, not the store
    writes: Mutex<()>,
}

impl Settings {
    /// The settings from the env, `env` looking a variable up. Err is the fatal message for a
    /// bad value. An empty ETH_RPC_URL counts as unset.
    pub fn from_env(
        env: impl Fn(&str) -> Option<String>,
        public_gateway: bool,
    ) -> Result<Settings, String> {
        let mut values = HashMap::new();
        for key in Key::ALL {
            let Some(raw) = env(key.spec().env_var) else {
                continue;
            };
            if key == Key::EthRpcUrl && raw.is_empty() {
                continue;
            }
            values.insert(key, check(key, &raw, By::Env)?);
        }
        let settings = Settings {
            public_gateway,
            env: values,
            overrides: RwLock::new(HashMap::new()),
            writes: Mutex::new(()),
        };
        check_pairs(|key| settings.fallback_num(key), By::Env)?;
        Ok(settings)
    }

    /// No env, and an own node
    pub fn defaults() -> Settings {
        Settings {
            public_gateway: false,
            env: HashMap::new(),
            overrides: RwLock::new(HashMap::new()),
            writes: Mutex::new(()),
        }
    }

    /// The value in use; every key but EthRpcUrl is a number
    pub fn get(&self, key: Key) -> u64 {
        self.num(&self.overrides.read().unwrap(), key)
    }

    /// EthRpcUrl's value in use, if any. Never log or return it.
    pub fn secret(&self, key: Key) -> Option<String> {
        assert_eq!(key, Key::EthRpcUrl, "{} isn't a secret", key.name());
        match self.value(&self.overrides.read().unwrap(), key) {
            Some(Val::Secret(url)) => Some(url),
            _ => None,
        }
    }

    /**
     * Reads the overrides stored in `store`, replacing any held. One that isn't valid any more,
     * alone or with the others (a pair rule), is logged and kept as ignored: it never stops
     * startup.
     */
    pub fn load(&self, store: &Store) {
        let rows = match store.settings_overrides() {
            Ok(rows) => rows,
            Err(err) => {
                warn!("Couldn't read the settings overrides, using none: {}", err);
                return;
            }
        };
        let mut overrides = HashMap::new();
        for (name, raw, actor, set_at) in rows {
            let Some(key) = Key::from_name(&name) else {
                warn!(
                    "Ignoring the stored setting {}: there's no such setting",
                    name
                );
                continue;
            };
            let value = check(key, &raw, By::Admin);
            overrides.insert(
                key,
                Override {
                    value,
                    actor,
                    set_at,
                },
            );
        }
        // a broken pair ignores the upper's override first, then the lower's; the env values
        // passed the rules, so one of the two is an override
        for (lower, upper) in PAIRS {
            loop {
                let view = |key| self.num(&overrides, key);
                let Err(reason) = check_pair(lower, upper, &view, By::Admin) else {
                    break;
                };
                let upper_set = matches!(overrides.get(&upper), Some(o) if o.value.is_ok());
                let victim = if upper_set { upper } else { lower };
                match overrides.get_mut(&victim) {
                    Some(o) if o.value.is_ok() => o.value = Err(reason),
                    _ => break,
                }
            }
        }
        for key in Key::ALL {
            if let Some(Override {
                value: Err(reason), ..
            }) = overrides.get(&key)
            {
                warn!("Ignoring the stored setting {}: {}", key.name(), reason);
            }
        }
        *self.overrides.write().unwrap() = overrides;
    }

    /// Overrides `key` with `raw` by `actor`, saved in `store`, after checking it alone and
    /// against the other values in use
    pub fn set(&self, store: &Store, key: Key, raw: &str, actor: &str) -> Result<Applies, String> {
        let value = check(key, raw, By::Admin)?;
        // held across the check and the write, so two sets can't break a pair rule between them
        let _writing = self.writes.lock().unwrap();
        let saved = match &value {
            Val::Num(n) => {
                let n = *n;
                let overrides = self.overrides.read().unwrap();
                check_pairs(
                    |k| if k == key { n } else { self.num(&overrides, k) },
                    By::Admin,
                )?;
                n.to_string()
            }
            Val::Secret(url) => url.clone(),
        };
        let at = now_secs();
        store
            .set_setting(key.name(), &saved, actor, at)
            .map_err(|err| format!("Couldn't save {}: {}", key.name(), err))?;
        self.overrides.write().unwrap().insert(
            key,
            Override {
                value: Ok(value),
                actor: actor.to_string(),
                set_at: at,
            },
        );
        match key {
            Key::EthRpcUrl => info!("ETH RPC URL changed by {}", actor),
            _ => info!("Setting {} set to {} by {}", key.name(), saved, actor),
        }
        Ok(key.spec().applies)
    }

    /// Drops `key`'s override, so the env value or the default is used, unless that would break
    /// a pair rule
    pub fn reset(&self, store: &Store, key: Key, actor: &str) -> Result<Applies, String> {
        let _writing = self.writes.lock().unwrap();
        if key != Key::EthRpcUrl {
            let overrides = self.overrides.read().unwrap();
            check_pairs(
                |k| match k == key {
                    true => self.fallback_num(k),
                    false => self.num(&overrides, k),
                },
                By::Admin,
            )?;
        }
        store
            .clear_setting(key.name())
            .map_err(|err| format!("Couldn't reset {}: {}", key.name(), err))?;
        self.overrides.write().unwrap().remove(&key);
        info!("Setting {} reset by {}", key.name(), actor);
        Ok(key.spec().applies)
    }

    /// Every setting, in table order, for the admin page; the secret is only "set" or "not set"
    pub fn report(&self) -> Value {
        let overrides = self.overrides.read().unwrap();
        let shown = |value: Option<Val>| match value {
            Some(Val::Num(n)) => json!(n),
            Some(Val::Secret(_)) => json!("set"),
            None => Value::Null,
        };
        let rows = Key::ALL
            .into_iter()
            .map(|key| {
                let spec = key.spec();
                let secret = key == Key::EthRpcUrl;
                let stored = overrides.get(&key);
                let value = match (self.value(&overrides, key), secret) {
                    (None, true) => json!("not set"),
                    (value, _) => shown(value),
                };
                let bound = |n: u64| if secret { Value::Null } else { json!(n) };
                json!({
                    "name": spec.name,
                    "label": spec.label,
                    "env_var": spec.env_var,
                    "value": value,
                    "source": self.source(&overrides, key).label(),
                    "iac": shown(self.env.get(&key).cloned()),
                    "default": shown(self.default_of(key)),
                    "min": bound(spec.min),
                    "max": bound(spec.max),
                    "applies": spec.applies.label(),
                    "actor": stored.map(|o| o.actor.clone()),
                    "set_at": stored.map(|o| o.set_at),
                    "ignored": stored.and_then(|o| o.value.as_ref().err().cloned()),
                })
            })
            .collect();
        Value::Array(rows)
    }

    fn source(&self, overrides: &Overrides, key: Key) -> Source {
        if matches!(overrides.get(&key), Some(o) if o.value.is_ok()) {
            Source::Admin
        } else if self.env.contains_key(&key) {
            Source::Env
        } else {
            Source::Default
        }
    }

    fn default_of(&self, key: Key) -> Option<Val> {
        match key {
            Key::EthRpcUrl => None,
            // self-throttle on the public gateway (Filebase allows 200/min; this leaves room for
            // redirect fetches); an own node has no limit unless configured
            Key::GatewayRequestsPerMin if self.public_gateway => Some(Val::Num(100)),
            _ => Some(Val::Num(key.spec().default)),
        }
    }

    // the env value, else the default: what a reset falls back to
    fn fallback(&self, key: Key) -> Option<Val> {
        self.env.get(&key).cloned().or_else(|| self.default_of(key))
    }

    fn value(&self, overrides: &Overrides, key: Key) -> Option<Val> {
        match overrides.get(&key) {
            Some(Override {
                value: Ok(value), ..
            }) => Some(value.clone()),
            _ => self.fallback(key),
        }
    }

    fn num(&self, overrides: &Overrides, key: Key) -> u64 {
        as_num(key, self.value(overrides, key))
    }

    fn fallback_num(&self, key: Key) -> u64 {
        as_num(key, self.fallback(key))
    }
}

fn as_num(key: Key, value: Option<Val>) -> u64 {
    match value {
        Some(Val::Num(n)) => n,
        _ => panic!("{} isn't a number", key.name()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // an env lookup over fixed pairs, so tests never touch the real environment
    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key| vars.get(key).cloned()
    }

    fn from(pairs: &[(&str, &str)]) -> Result<Settings, String> {
        Settings::from_env(env(pairs), false)
    }

    fn entry(report: &serde_json::Value, name: &str) -> serde_json::Value {
        report
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == name)
            .unwrap()
            .clone()
    }

    #[test]
    fn defaults_match_the_table() {
        let own = Settings::defaults();
        let expected = [
            (Key::CrawlWorkers, 10),
            (Key::GatewayRequestsPerMin, 0),
            (Key::DiscoveryPerHour, 500),
            (Key::HealthMinRate, 500),
            (Key::HealthGraceSecs, 900),
            (Key::IndexMaxAgeDays, 30),
            (Key::IndexCacheMb, 1024),
            (Key::PauseWindow, 20),
            (Key::PauseFailures, 16),
            (Key::PauseMinSecs, 60),
            (Key::PauseMaxSecs, 900),
            (Key::PageAttempts, 3),
            (Key::PageTimeoutSecs, 90),
            (Key::PageMaxBytes, 2_097_152),
            (Key::NameTimeoutSecs, 30),
            (Key::EthLogsMaxBlocks, 10_000),
            (Key::EthRequestSecs, 1),
        ];
        for (key, value) in expected {
            assert_eq!(own.get(key), value, "{}", key.name());
        }
        assert_eq!(own.secret(Key::EthRpcUrl), None);
        let public = Settings::from_env(env(&[]), true).unwrap();
        assert_eq!(public.get(Key::GatewayRequestsPerMin), 100);
        // every key once, in table order, and each name maps back
        for (at, key) in Key::ALL.into_iter().enumerate() {
            assert_eq!(key as usize, at, "{}", key.name());
            assert_eq!(Key::from_name(key.name()), Some(key));
        }
        assert_eq!(Key::from_name("nope"), None);
        let names: Vec<String> = own
            .report()
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap().to_string())
            .collect();
        let table: Vec<&str> = Key::ALL.iter().map(|k| k.name()).collect();
        assert_eq!(names, table);
    }

    #[test]
    fn env_values_are_used_and_bad_ones_are_fatal() {
        assert_eq!(
            from(&[("CRAWL_WORKERS", "60")])
                .unwrap()
                .get(Key::CrawlWorkers),
            60
        );
        assert_eq!(
            from(&[("CRAWL_WORKERS", "0")]).err().unwrap(),
            "CRAWL_WORKERS must be a whole number from 1 to 500, got \"0\""
        );
        // it used to fall back to 30
        assert!(from(&[("INDEX_MAX_AGE_DAYS", "x")]).is_err());
        // iac's values
        let iac = from(&[("ETH_LOGS_MAX_BLOCKS", "10"), ("ETH_REQUEST_SECS", "6")]).unwrap();
        assert_eq!(iac.get(Key::EthLogsMaxBlocks), 10);
        assert_eq!(iac.get(Key::EthRequestSecs), 6);
        // 0 means off or no limit where the table allows it
        for (var, key) in [
            ("HEALTH_MIN_RATE", Key::HealthMinRate),
            ("DISCOVERY_PER_HOUR", Key::DiscoveryPerHour),
            ("GATEWAY_REQUESTS_PER_MIN", Key::GatewayRequestsPerMin),
            ("ETH_REQUEST_SECS", Key::EthRequestSecs),
        ] {
            assert_eq!(from(&[(var, "0")]).unwrap().get(key), 0, "{}", var);
        }
        assert_eq!(
            from(&[("HEALTH_MIN_RATE", "1200")])
                .unwrap()
                .get(Key::HealthMinRate),
            1200
        );
        assert_eq!(
            from(&[("DISCOVERY_PER_HOUR", "2000")])
                .unwrap()
                .get(Key::DiscoveryPerHour),
            2000
        );
        for (var, bad) in [
            ("CRAWL_WORKERS", "-1"),
            ("CRAWL_WORKERS", "ten"),
            ("CRAWL_WORKERS", ""),
            ("CRAWL_WORKERS", "501"),
            ("HEALTH_MIN_RATE", ""),
            ("HEALTH_MIN_RATE", "-1"),
            ("HEALTH_MIN_RATE", "fast"),
            ("HEALTH_MIN_RATE", "1.5"),
            ("DISCOVERY_PER_HOUR", "lots"),
            ("DISCOVERY_PER_HOUR", "-1"),
            ("ETH_LOGS_MAX_BLOCKS", ""),
            ("ETH_LOGS_MAX_BLOCKS", "0"),
            ("ETH_LOGS_MAX_BLOCKS", "-1"),
            ("ETH_LOGS_MAX_BLOCKS", "ten"),
            ("ETH_LOGS_MAX_BLOCKS", "1.5"),
            ("ETH_REQUEST_SECS", ""),
            ("ETH_REQUEST_SECS", "-1"),
            ("ETH_REQUEST_SECS", "six"),
            ("GATEWAY_REQUESTS_PER_MIN", "many"),
            ("INDEX_CACHE_MB", "big"),
            ("PAGE_MAX_BYTES", "1000"),
        ] {
            let err = from(&[(var, bad)]).err();
            assert!(err.is_some(), "{}={:?}", var, bad);
            assert!(err.unwrap().starts_with(var), "{}={:?}", var, bad);
        }
        // an empty ETH_RPC_URL is unset, as before; a bad one never shows its value
        assert_eq!(
            from(&[("ETH_RPC_URL", "")]).unwrap().secret(Key::EthRpcUrl),
            None
        );
        let err = from(&[("ETH_RPC_URL", "ftp://x/KEY123")]).err().unwrap();
        assert!(!err.contains("KEY123"), "{}", err);
        assert_eq!(
            from(&[("ETH_RPC_URL", "https://eth.example/v2/KEY123")])
                .unwrap()
                .secret(Key::EthRpcUrl),
            Some("https://eth.example/v2/KEY123".to_string())
        );
    }

    #[test]
    fn env_cross_field_rules_are_fatal() {
        assert!(from(&[("PAUSE_FAILURES", "21")]).is_err());
        assert!(from(&[("PAUSE_MIN_SECS", "1000")]).is_err());
        // fine when the other side moves too
        assert!(from(&[("PAUSE_FAILURES", "21"), ("PAUSE_WINDOW", "30")]).is_ok());
        assert!(from(&[("PAUSE_MIN_SECS", "1000"), ("PAUSE_MAX_SECS", "1000")]).is_ok());
    }

    #[test]
    fn an_override_beats_env_and_reset_falls_back() {
        let store = Store::open_in_memory().unwrap();
        let settings = from(&[("CRAWL_WORKERS", "60")]).unwrap();
        assert_eq!(
            settings.set(&store, Key::CrawlWorkers, "80", "a@b"),
            Ok(Applies::Live)
        );
        assert_eq!(settings.get(Key::CrawlWorkers), 80);
        let row = entry(&settings.report(), "crawl_workers");
        assert_eq!(row["source"], "admin");
        assert_eq!(row["value"], 80);
        assert_eq!(row["iac"], 60);
        assert_eq!(row["default"], 10);
        assert_eq!(row["actor"], "a@b");
        assert!(row["set_at"].as_i64().unwrap() > 0);
        assert!(row["ignored"].is_null());

        // a fresh Settings over the same store sees it
        let restarted = from(&[("CRAWL_WORKERS", "60")]).unwrap();
        restarted.load(&store);
        assert_eq!(restarted.get(Key::CrawlWorkers), 80);

        assert_eq!(
            settings.reset(&store, Key::CrawlWorkers, "a@b"),
            Ok(Applies::Live)
        );
        assert_eq!(settings.get(Key::CrawlWorkers), 60);
        let row = entry(&settings.report(), "crawl_workers");
        assert_eq!(row["source"], "iac");
        assert!(row["actor"].is_null());
        assert!(row["set_at"].is_null());
        assert!(store.settings_overrides().unwrap().is_empty());
        let row = entry(&settings.report(), "discovery_per_hour");
        assert_eq!(row["source"], "default");
        assert!(row["iac"].is_null());
    }

    #[test]
    fn set_refuses_out_of_range_and_cross_field_values() {
        let store = Store::open_in_memory().unwrap();
        let settings = Settings::defaults();
        let err = settings
            .set(&store, Key::PauseFailures, "21", "a@b")
            .err()
            .unwrap();
        assert!(err.contains("pause_window"), "{}", err);
        assert_eq!(settings.get(Key::PauseFailures), 16);
        assert_eq!(
            settings.set(&store, Key::PauseWindow, "30", "a@b"),
            Ok(Applies::Live)
        );
        assert_eq!(
            settings.set(&store, Key::PauseFailures, "21", "a@b"),
            Ok(Applies::Live)
        );
        // the window can't drop below the failures now, nor reset to 20
        assert!(settings.set(&store, Key::PauseWindow, "20", "a@b").is_err());
        assert!(settings.reset(&store, Key::PauseWindow, "a@b").is_err());
        assert_eq!(settings.get(Key::PauseWindow), 30);
        // and the same pair rule for the pause lengths
        assert!(settings
            .set(&store, Key::PauseMinSecs, "1000", "a@b")
            .is_err());
        assert!(settings
            .set(&store, Key::PauseMaxSecs, "59", "a@b")
            .is_err());

        let before = store.settings_overrides().unwrap();
        for bad in ["abc", "0", "501", "", "-3"] {
            assert!(
                settings.set(&store, Key::CrawlWorkers, bad, "a@b").is_err(),
                "{:?}",
                bad
            );
        }
        assert_eq!(store.settings_overrides().unwrap(), before);
        assert_eq!(settings.get(Key::CrawlWorkers), 10);
    }

    #[test]
    fn a_bad_number_that_may_be_a_secret_is_not_echoed() {
        let store = Store::open_in_memory().unwrap();
        let settings = Settings::defaults();
        let err = settings
            .set(
                &store,
                Key::CrawlWorkers,
                "https://x.example/v2/SECRETKEY",
                "a@b",
            )
            .err()
            .unwrap();
        assert!(!err.contains("SECRETKEY"), "{}", err);
        assert!(err.contains("got a non-number"), "{}", err);
        let long = "a".repeat(33);
        let err = settings
            .set(&store, Key::CrawlWorkers, &long, "a@b")
            .err()
            .unwrap();
        assert!(!err.contains(&long), "{}", err);
        // a short value is still shown
        let err = settings
            .set(&store, Key::CrawlWorkers, "abc", "a@b")
            .err()
            .unwrap();
        assert!(err.contains("got \"abc\""), "{}", err);
    }

    #[test]
    fn the_secret_is_never_reported_or_echoed() {
        let store = Store::open_in_memory().unwrap();
        let settings = Settings::defaults();
        let row = entry(&settings.report(), "eth_rpc_url");
        assert_eq!(row["value"], "not set");
        assert!(row["iac"].is_null());
        assert_eq!(
            settings.set(
                &store,
                Key::EthRpcUrl,
                "https://eth.example/v2/KEY123",
                "a@b"
            ),
            Ok(Applies::Restart)
        );
        assert_eq!(
            settings.secret(Key::EthRpcUrl),
            Some("https://eth.example/v2/KEY123".to_string())
        );
        let report = settings.report();
        assert!(!report.to_string().contains("KEY123"));
        assert_eq!(entry(&report, "eth_rpc_url")["value"], "set");
        let err = settings
            .set(&store, Key::EthRpcUrl, "not a url KEY123", "a@b")
            .err()
            .unwrap();
        assert!(!err.contains("KEY123"), "{}", err);
        // an iac value is only said to be there
        let iac = from(&[("ETH_RPC_URL", "https://eth.example/v2/KEY456")]).unwrap();
        let report = iac.report();
        assert!(!report.to_string().contains("KEY456"));
        assert_eq!(entry(&report, "eth_rpc_url")["iac"], "set");
        assert_eq!(entry(&report, "eth_rpc_url")["source"], "iac");
    }

    #[test]
    fn an_invalid_stored_override_is_ignored_not_fatal() {
        let store = Store::open_in_memory().unwrap();
        store
            .set_setting("crawl_workers", "9999", "a@b", 1)
            .unwrap();
        // a pair that's only bad together: the dependent one is ignored
        store
            .set_setting("pause_failures", "500", "a@b", 1)
            .unwrap();
        // not a setting (any more)
        store.set_setting("gone", "1", "a@b", 1).unwrap();
        let settings = from(&[("CRAWL_WORKERS", "60")]).unwrap();
        settings.load(&store);
        assert_eq!(settings.get(Key::CrawlWorkers), 60);
        assert_eq!(settings.get(Key::PauseFailures), 16);
        let report = settings.report();
        for name in ["crawl_workers", "pause_failures"] {
            let row = entry(&report, name);
            assert!(row["ignored"].is_string(), "{}", name);
            assert_eq!(
                row["source"],
                if name == "crawl_workers" {
                    "iac"
                } else {
                    "default"
                }
            );
        }
        // a stored secret that's bad never shows in the reason
        let store = Store::open_in_memory().unwrap();
        store
            .set_setting("eth_rpc_url", "junk KEY789", "a@b", 1)
            .unwrap();
        let settings = Settings::defaults();
        settings.load(&store);
        assert_eq!(settings.secret(Key::EthRpcUrl), None);
        assert!(!settings.report().to_string().contains("KEY789"));
    }

    #[test]
    fn restart_settings_say_so() {
        let store = Store::open_in_memory().unwrap();
        let settings = Settings::defaults();
        assert_eq!(
            settings.set(&store, Key::EthLogsMaxBlocks, "50", "a@b"),
            Ok(Applies::Restart)
        );
        assert_eq!(
            settings.set(&store, Key::CrawlWorkers, "5", "a@b"),
            Ok(Applies::Live)
        );
        let report = settings.report();
        assert_eq!(entry(&report, "eth_logs_max_blocks")["applies"], "restart");
        assert_eq!(entry(&report, "crawl_workers")["applies"], "live");
    }
}
