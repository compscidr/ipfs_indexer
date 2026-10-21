/*!
 * Lists .eth sites from Ethereum (#213): ENS resolvers emit ContenthashChanged(node, hash) when a
 * name's content record changes. eth_getLogs over block ranges finds them all, and only the
 * latest per node counts. Providers cap ranges differently, so the range adapts: a JSON-RPC
 * error halves it, a success doubles it back.
 */
use crate::entry::{contenthash, Contenthash};
use log::warn;
use serde_json::{json, Value};
use std::time::Duration;

/// keccak256("ContenthashChanged(bytes32,bytes)")
pub const TOPIC: &str = "0xe379c1624ed7e714cc0937528a32359d69d5281337765313dba4e081b72d7578";
/// The first ENS registry's deployment (May 2017): no record is older
pub const START_BLOCK: u64 = 3_327_417;
pub const MAX_RANGE: u64 = 10_000;
// an error keeps this many characters of a provider's answer
const ERROR_CHARS: usize = 240;
// a path segment or query value this long may be an API key, and is scrubbed from errors
const KEY_CHARS: usize = 8;

#[derive(Debug, PartialEq)]
pub enum RpcError {
    /// no usable answer: no connection, a timeout, a rate limit, a body that isn't JSON-RPC
    Transport(String),
    /// the provider answered with a JSON-RPC error, e.g. a range or result-count limit
    Rpc(String),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::Transport(msg) => write!(f, "{}", msg),
            RpcError::Rpc(msg) => write!(f, "provider error: {}", msg),
        }
    }
}

pub trait Rpc {
    fn call(&self, method: &str, params: Value) -> Result<Value, RpcError>;
}

/// JSON-RPC over HTTP. The URL carries the provider's API key, so it's never logged or put in an
/// error.
pub struct HttpRpc {
    url: String,
    client: reqwest::blocking::Client,
}

impl HttpRpc {
    pub fn new(url: String) -> Result<HttpRpc, String> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| e.without_url().to_string())?;
        Ok(HttpRpc { url, client })
    }

    // text from the provider or the HTTP client with the URL's secrets (the key is often in its
    // path or query, or its userinfo) replaced by "…", then cut to ERROR_CHARS. A provider may
    // echo the request path or just the key, so each path segment and query value long enough
    // to be a key counts too. Scrubbing comes first: a cut through a secret would leave a part
    // of it no longer matching.
    fn scrub(&self, text: &str) -> String {
        let mut secrets = vec![self.url.clone()];
        if let Ok(url) = reqwest::Url::parse(&self.url) {
            secrets.push(url.path().to_string());
            secrets.extend(url.query().map(str::to_string));
            secrets.push(url.username().to_string());
            secrets.extend(url.password().map(str::to_string));
            // short segments like "v2" aren't keys, and blanking them would garble errors
            let parts = url
                .path_segments()
                .into_iter()
                .flatten()
                .map(str::to_string);
            let values = url.query_pairs().map(|(_, value)| value.into_owned());
            secrets.extend(parts.chain(values).filter(|s| s.len() >= KEY_CHARS));
        }
        let mut text = text.to_string();
        // longest first, so a part never leaves a fragment of the whole
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        for secret in secrets.iter().filter(|s| !s.is_empty() && *s != "/") {
            text = text.replace(secret.as_str(), "…");
        }
        match text.char_indices().nth(ERROR_CHARS) {
            Some((at, _)) => format!("{}…", &text[..at]),
            None => text,
        }
    }
}

/// An RPC URL's host, or None if it isn't a URL with one
pub fn host_of(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
}

impl Rpc for HttpRpc {
    fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let transport =
            |e: reqwest::Error| RpcError::Transport(self.scrub(&e.without_url().to_string()));
        let response = self
            .client
            .post(&self.url)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .map_err(transport)?;
        // a rate limit is a reason to wait, not to ask for fewer blocks
        if response.status().as_u16() == 429 {
            return Err(RpcError::Transport("rate limited (429)".to_string()));
        }
        let status = response.status();
        let text = response.text().map_err(transport)?;
        answer(&text).map_err(|err| match err {
            RpcError::Transport(msg) => {
                RpcError::Transport(self.scrub(&format!("HTTP {}: {}", status, msg)))
            }
            RpcError::Rpc(msg) => RpcError::Rpc(self.scrub(&msg)),
        })
    }
}

// a JSON-RPC response body: its result, or its error
fn answer(text: &str) -> Result<Value, RpcError> {
    let mut v: Value = serde_json::from_str(text)
        // the whole body: scrub() cuts it once the secrets are out
        .map_err(|_| RpcError::Transport(format!("not a JSON-RPC answer: {}", text)))?;
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string);
        return Err(RpcError::Rpc(msg.unwrap_or_else(|| err.to_string())));
    }
    v.get_mut("result")
        .map(Value::take)
        .ok_or_else(|| RpcError::Transport("no result in the answer".to_string()))
}

/// One ContenthashChanged log: the ENS node and what its record points at now
#[derive(Debug, PartialEq)]
pub struct Event {
    pub node: String,
    pub record: Contenthash,
}

/// Where the scan is: the next block to read, and how many blocks a request asks for
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scan {
    pub next: u64,
    pub range: u64,
}

impl Scan {
    pub fn starting(next: u64) -> Scan {
        Scan {
            next,
            range: MAX_RANGE,
        }
    }
}

/// The latest block number
pub fn latest_block(rpc: &dyn Rpc) -> Result<u64, RpcError> {
    let v = rpc.call("eth_blockNumber", json!([]))?;
    v.as_str()
        .and_then(hex_u64)
        .ok_or_else(|| RpcError::Transport(format!("not a block number: {:.100}", v)))
}

/**
 * One eth_getLogs request, from scan.next to at most `latest`. On success the scan moves past
 * the range and the range doubles (up to MAX_RANGE); a JSON-RPC error halves it (down to 1) so
 * the next request asks for less; a transport error leaves the scan alone. Logs that can't be
 * read are skipped with a warning.
 */
#[cfg(test)]
pub fn step(rpc: &dyn Rpc, scan: &mut Scan, latest: u64) -> Result<Vec<Event>, RpcError> {
    step_within(rpc, scan, latest, MAX_RANGE)
}

/// step(), never asking for more than `max` blocks: a provider's per-request limit (Alchemy's
/// free plan allows 10), so the range doesn't grow past it only to be refused
pub fn step_within(
    rpc: &dyn Rpc,
    scan: &mut Scan,
    latest: u64,
    max: u64,
) -> Result<Vec<Event>, RpcError> {
    scan.range = scan.range.clamp(1, max.max(1));
    if scan.next > latest {
        return Ok(Vec::new());
    }
    let to = latest.min(scan.next + scan.range - 1);
    let params = json!([{
        "fromBlock": format!("{:#x}", scan.next),
        "toBlock": format!("{:#x}", to),
        "topics": [TOPIC],
    }]);
    let logs = match rpc.call("eth_getLogs", params) {
        Ok(logs) => logs,
        Err(RpcError::Rpc(msg)) => {
            scan.range = (scan.range / 2).max(1);
            return Err(RpcError::Rpc(msg));
        }
        Err(err) => return Err(err),
    };
    let logs = logs
        .as_array()
        .ok_or_else(|| RpcError::Transport("eth_getLogs didn't return a list".to_string()))?;
    let events = logs
        .iter()
        .filter_map(|log| {
            let read = event(log);
            if read.is_none() {
                warn!("ENS scan: skipping a log I couldn't read: {:.300}", log);
            }
            read
        })
        .collect();
    scan.next = to + 1;
    scan.range = (scan.range * 2).min(max.max(1));
    Ok(events)
}

// a log's node (topics[1]) and its contenthash (data: ABI-encoded bytes)
fn event(log: &Value) -> Option<Event> {
    let node = log.get("topics")?.get(1)?.as_str()?.to_string();
    let data = hex_bytes(log.get("data")?.as_str()?)?;
    Some(Event {
        node,
        record: contenthash(abi_bytes(&data)?),
    })
}

fn hex_u64(s: &str) -> Option<u64> {
    u64::from_str_radix(s.strip_prefix("0x")?, 16).ok()
}

fn hex_bytes(s: &str) -> Option<Vec<u8>> {
    let s = s.strip_prefix("0x")?;
    if !s.is_ascii() || s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

// ABI-encoded `bytes`: a 32-byte offset, and there a 32-byte length and the bytes themselves
fn abi_bytes(data: &[u8]) -> Option<&[u8]> {
    let word = |at: usize| -> Option<usize> {
        let w = data.get(at..at.checked_add(32)?)?;
        if w[..24].iter().any(|b| *b != 0) {
            return None;
        }
        Some(u64::from_be_bytes(w[24..].try_into().ok()?) as usize)
    };
    let offset = word(0)?;
    let len = word(offset)?;
    let start = offset.checked_add(32)?;
    data.get(start..start.checked_add(len)?)
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// Answers calls from a list, in order, and records them
    pub struct FakeRpc {
        answers: RefCell<VecDeque<Result<Value, RpcError>>>,
        calls: RefCell<Vec<(String, Value)>>,
    }

    impl FakeRpc {
        pub fn new(answers: Vec<Result<Value, RpcError>>) -> FakeRpc {
            FakeRpc {
                answers: RefCell::new(answers.into()),
                calls: RefCell::new(Vec::new()),
            }
        }

        pub fn calls(&self) -> Vec<(String, Value)> {
            self.calls.borrow().clone()
        }
    }

    impl Rpc for FakeRpc {
        fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
            self.calls.borrow_mut().push((method.to_string(), params));
            self.answers
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Err(RpcError::Transport("no more answers".into())))
        }
    }

    /// A ContenthashChanged log as a provider returns it, the contenthash ABI-encoded
    pub fn log(node: &str, hash: &[u8]) -> Value {
        let mut data = vec![0u8; 31];
        data.push(32);
        data.extend([0u8; 24]);
        data.extend((hash.len() as u64).to_be_bytes());
        data.extend(hash);
        data.resize(64 + hash.len().div_ceil(32) * 32, 0);
        let hex: String = data.iter().map(|b| format!("{:02x}", b)).collect();
        json!({
            "address": "0x231b0ee14048e9dccd1d247744d114a4eb5e8e63",
            "topics": [TOPIC, node],
            "data": format!("0x{}", hex),
            "blockNumber": "0x18feaf6",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{log, FakeRpc};
    use super::*;
    use crate::entry::Entry;

    const V1: &str = "bafybeiaysi4s6lnjev27ln5icwm6tueaw2vdykrtjkwiphwekaywqhcjze";

    fn ipfs_record() -> Vec<u8> {
        [vec![0xe3, 0x01], cid::Cid::try_from(V1).unwrap().to_bytes()].concat()
    }

    fn range_of(call: &(String, Value)) -> (u64, u64) {
        let p = &call.1[0];
        let n = |k: &str| {
            u64::from_str_radix(p[k].as_str().unwrap().trim_start_matches("0x"), 16).unwrap()
        };
        (n("fromBlock"), n("toBlock"))
    }

    #[test]
    fn logs_decode_to_records_and_unreadable_ones_are_skipped() {
        let rpc = FakeRpc::new(vec![Ok(json!([
            log("0xaa", &ipfs_record()),
            log("0xbb", &[]),
            log("0xcc", &[0xe4, 0x01, 0xfa]),
            {"topics": [TOPIC], "data": "0x"},
            {"topics": [TOPIC, "0xdd"], "data": "0x1234"},
        ]))]);
        let mut scan = Scan::starting(START_BLOCK);
        let events = step(&rpc, &mut scan, START_BLOCK + 50_000).unwrap();
        assert_eq!(
            events,
            vec![
                Event {
                    node: "0xaa".into(),
                    record: Contenthash::Set(Entry::Cid(V1.into()))
                },
                Event {
                    node: "0xbb".into(),
                    record: Contenthash::Cleared
                },
                Event {
                    node: "0xcc".into(),
                    record: Contenthash::Other
                },
            ]
        );
        let calls = rpc.calls();
        assert_eq!(calls[0].0, "eth_getLogs");
        assert_eq!(calls[0].1[0]["topics"], json!([TOPIC]));
        assert!(calls[0].1[0].get("address").is_none(), "every resolver");
        assert_eq!(range_of(&calls[0]), (START_BLOCK, START_BLOCK + 9_999));
        assert_eq!(
            scan,
            Scan {
                next: START_BLOCK + 10_000,
                range: MAX_RANGE
            }
        );
    }

    #[test]
    fn the_range_halves_on_an_rpc_error_and_doubles_back() {
        let too_big = || {
            Err(RpcError::Rpc(
                "ranges over 10000 blocks are not supported".into(),
            ))
        };
        let rpc = FakeRpc::new(vec![too_big(), too_big(), Ok(json!([])), Ok(json!([]))]);
        let mut scan = Scan::starting(100);
        assert!(step(&rpc, &mut scan, 1_000_000).is_err());
        assert_eq!(
            scan,
            Scan {
                next: 100,
                range: 5_000
            }
        );
        assert!(step(&rpc, &mut scan, 1_000_000).is_err());
        assert_eq!(
            scan,
            Scan {
                next: 100,
                range: 2_500
            }
        );
        step(&rpc, &mut scan, 1_000_000).unwrap();
        assert_eq!(
            scan,
            Scan {
                next: 2_600,
                range: 5_000
            }
        );
        step(&rpc, &mut scan, 1_000_000).unwrap();
        assert_eq!(
            scan,
            Scan {
                next: 7_600,
                range: 10_000
            }
        );
        let ranges: Vec<_> = rpc.calls().iter().map(range_of).collect();
        assert_eq!(
            ranges,
            vec![(100, 10_099), (100, 5_099), (100, 2_599), (2_600, 7_599)]
        );
    }

    #[test]
    fn a_provider_cap_keeps_every_request_within_it() {
        let rpc = FakeRpc::new(vec![
            Ok(json!([])),
            Err(RpcError::Rpc("no".into())),
            Ok(json!([])),
        ]);
        let mut scan = Scan::starting(100);
        step_within(&rpc, &mut scan, 1_000_000, 10).unwrap();
        assert!(step_within(&rpc, &mut scan, 1_000_000, 10).is_err());
        step_within(&rpc, &mut scan, 1_000_000, 10).unwrap();
        let ranges: Vec<_> = rpc.calls().iter().map(range_of).collect();
        assert_eq!(ranges, vec![(100, 109), (110, 119), (110, 114)]);
        assert_eq!(
            scan,
            Scan {
                next: 115,
                range: 10
            },
            "back to the cap, not past it"
        );
    }

    #[test]
    fn the_range_never_goes_below_one_block() {
        let rpc = FakeRpc::new(vec![Err(RpcError::Rpc("no".into()))]);
        let mut scan = Scan {
            next: 100,
            range: 1,
        };
        assert!(step(&rpc, &mut scan, 1_000).is_err());
        assert_eq!(
            scan,
            Scan {
                next: 100,
                range: 1
            }
        );
    }

    #[test]
    fn a_transport_error_keeps_the_range_and_position() {
        let rpc = FakeRpc::new(vec![Err(RpcError::Transport("timed out".into()))]);
        let mut scan = Scan::starting(100);
        assert_eq!(
            step(&rpc, &mut scan, 1_000_000),
            Err(RpcError::Transport("timed out".into()))
        );
        assert_eq!(scan, Scan::starting(100));
    }

    #[test]
    fn the_last_request_stops_at_the_latest_block_and_then_there_are_none() {
        let rpc = FakeRpc::new(vec![Ok(json!([]))]);
        let mut scan = Scan::starting(100);
        step(&rpc, &mut scan, 150).unwrap();
        assert_eq!(range_of(&rpc.calls()[0]), (100, 150));
        assert_eq!(scan.next, 151);
        assert_eq!(step(&rpc, &mut scan, 150), Ok(vec![]));
        assert_eq!(rpc.calls().len(), 1, "caught up: no request");
    }

    #[test]
    fn latest_block_reads_the_hex_number() {
        let rpc = FakeRpc::new(vec![Ok(json!("0x18fec3e"))]);
        assert_eq!(latest_block(&rpc), Ok(26_209_342));
        assert_eq!(rpc.calls()[0].0, "eth_blockNumber");
    }

    #[test]
    fn answers_are_results_or_rpc_errors() {
        assert_eq!(
            answer(
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":35,"message":"ranges over 10000 blocks are not supported on free plan"}}"#
            ),
            Err(RpcError::Rpc(
                "ranges over 10000 blocks are not supported on free plan".into()
            ))
        );
        assert!(matches!(
            answer("<html>busy</html>"),
            Err(RpcError::Transport(_))
        ));
        assert_eq!(
            answer(r#"{"jsonrpc":"2.0","id":1,"result":[1]}"#),
            Ok(json!([1]))
        );
    }

    #[test]
    fn http_errors_never_include_the_url() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let rpc = HttpRpc::new(format!("http://127.0.0.1:{}/v2/SECRETKEY", port)).unwrap();
        assert_eq!(
            host_of(&format!("http://127.0.0.1:{}/v2/SECRETKEY", port)).as_deref(),
            Some("127.0.0.1")
        );
        match rpc.call("eth_blockNumber", json!([])) {
            Err(RpcError::Transport(msg)) => assert!(!msg.contains("SECRETKEY"), "{}", msg),
            other => panic!("{:?}", other),
        }
    }

    // a server answering every request with a 404 whose body is made from the request path, as
    // some providers' error pages echo it
    fn answering_server(body: impl Fn(&str) -> String + Send + 'static) -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                let body = body(&path);
                let reply = format!(
                    "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        port
    }

    fn json_error(message: String) -> String {
        json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -1, "message": message}}).to_string()
    }

    // the error a provider answering with `body` turns into, for a URL holding three secrets
    fn error_from(body: impl Fn(&str) -> String + Send + 'static) -> String {
        let port = answering_server(body);
        let rpc = HttpRpc::new(format!(
            "http://user:PASSWORD@127.0.0.1:{}/v2/SECRETKEY?apikey=QUERYKEY",
            port
        ))
        .unwrap();
        match rpc.call("eth_blockNumber", json!([])) {
            Err(RpcError::Transport(msg)) | Err(RpcError::Rpc(msg)) => msg,
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn a_key_cut_short_or_echoed_on_its_own_is_scrubbed() {
        let errors = [
            // the path straddles the length errors are cut to
            error_from(|path| format!("{}Cannot POST {}", "x".repeat(180), path)),
            // the key alone, without the path around it
            error_from(|_| "invalid project id SECRETKEY".to_string()),
            error_from(|_| json_error("bad api key QUERYKEY".to_string())),
            error_from(|_| json_error(format!("{} PASSWORD", "y".repeat(190)))),
        ];
        for msg in errors {
            for part in ["SECR", "QUER", "PASS"] {
                assert!(!msg.contains(part), "{} in {}", part, msg);
            }
        }
    }

    #[test]
    fn provider_answers_echoing_the_url_are_scrubbed() {
        for json_error in [false, true] {
            let port = answering_server(move |path| {
                if json_error {
                    self::json_error(format!("bad {}", path))
                } else {
                    format!("Cannot POST {}", path)
                }
            });
            let rpc = HttpRpc::new(format!(
                "http://user:PASSWORD@127.0.0.1:{}/v2/SECRETKEY?apikey=QUERYKEY",
                port
            ))
            .unwrap();
            let msg = match rpc.call("eth_blockNumber", json!([])) {
                Err(RpcError::Transport(msg)) | Err(RpcError::Rpc(msg)) => msg,
                other => panic!("{:?}", other),
            };
            for secret in ["SECRETKEY", "QUERYKEY", "PASSWORD"] {
                assert!(!msg.contains(secret), "{} in {}", secret, msg);
            }
            assert!(msg.contains('…'), "{}", msg);
        }
    }
}
