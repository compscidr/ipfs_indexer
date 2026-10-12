use cid::Cid;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::time::Duration;

pub const BADBITS_URL: &str = "https://badbits.dwebops.pub/badbits.deny";

/**
 * Protocol Labs' badbits denylist. Entries are "legacy" double hashes: hex sha256 of
 * "<cidv1 base32>/<path>", so the list never contains the CIDs themselves.
 * https://specs.ipfs.tech/compact-denylist-format/
 */
pub struct Denylist {
    hashes: HashSet<[u8; 32]>,
}

impl Denylist {
    pub fn empty() -> Denylist {
        Denylist {
            hashes: HashSet::new(),
        }
    }

    /**
     * Parses badbits-style "//<hex sha256>" entries. Plain "cid", "cid/path" or "/ipfs/cid/path"
     * lines (for a hand-maintained local list) block that CID or path. Other lines are ignored.
     */
    // ponytail: only legacy hex entries and plain CIDs, badbits ships nothing else today
    pub fn parse(text: &str) -> Denylist {
        let hashes = text
            .lines()
            .map(str::trim)
            .filter_map(|line| match line.strip_prefix("//") {
                Some(hex) => parse_hex32(hex),
                None => prefix_keys(line.trim_start_matches("/ipfs/"))
                    .and_then(|keys| keys.last().map(|key| sha256(key))),
            })
            .collect();
        Denylist { hashes }
    }

    pub fn load_file(path: &str) -> std::io::Result<Denylist> {
        Ok(Denylist::parse(&std::fs::read_to_string(path)?))
    }

    pub fn fetch() -> Result<Denylist, reqwest::Error> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?;
        let text = client.get(BADBITS_URL).send()?.error_for_status()?.text()?;
        Ok(Denylist::parse(&text))
    }

    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    /**
     * True if the CID or any path prefix of a "cid/path" item is listed. Anything that doesn't
     * start with a parseable CID counts as blocked.
     */
    pub fn is_blocked(&self, item: &str) -> bool {
        match prefix_keys(item) {
            Some(keys) => keys.iter().any(|key| self.hashes.contains(&sha256(key))),
            None => true,
        }
    }
}

/**
 * Canonical "cid[/path]": resolves "." and ".." the way a URL does (RFC 3986, so in
 * "wiki//../X" the ".." removes the empty segment), then drops empty segments, a trailing
 * slash, "?query" and "#fragment". None if there's no root or the path climbs above it,
 * so ".." can never step into a different CID.
 */
pub fn normalize_item(item: &str) -> Option<String> {
    let item = item.split(['?', '#']).next().unwrap_or("");
    let mut parts = item.split('/');
    let root = parts.next().filter(|r| !r.is_empty())?;
    let mut stack: Vec<&str> = Vec::new();
    for segment in parts {
        match segment {
            "." => {}
            ".." => {
                stack.pop()?;
            }
            _ => stack.push(segment),
        }
    }
    let path: Vec<&str> = stack.into_iter().filter(|s| !s.is_empty()).collect();
    Some(if path.is_empty() {
        root.to_string()
    } else {
        format!("{}/{}", root, path.join("/"))
    })
}

/**
 * The denylist keys for a "cid/path" item: "<cidv1>/" (the whole tree), then "<cidv1>/a",
 * "<cidv1>/a/b", ... Empty segments are ignored. None if it doesn't start with a valid CID.
 */
fn prefix_keys(item: &str) -> Option<Vec<String>> {
    // normalized first, so every spelling of a path (wiki//X, wiki/a/../X) maps to the same keys
    let item = normalize_item(item)?;
    let mut segments = item.split('/').filter(|s| !s.is_empty());
    let cid = Cid::try_from(segments.next()?).ok()?.into_v1().ok()?;
    let mut key = format!("{}/", cid);
    let mut keys = vec![key.clone()];
    for (i, segment) in segments.enumerate() {
        if i > 0 {
            key.push('/');
        }
        key.push_str(segment);
        keys.push(key.clone());
    }
    Some(keys)
}

fn sha256(key: &str) -> [u8; 32] {
    Sha256::digest(key.as_bytes()).into()
}

fn parse_hex32(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // the same content as CIDv0 and CIDv1
    const V0: &str = "QmbWqxBEKC3P8tqsKc98xmWNzrzDtRLMiMPL8wBuTGsMnR";
    const V1: &str = "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi";
    const OTHER: &str = "bafybeiaysi4s6lnjev27ln5icwm6tueaw2vdykrtjkwiphwekaywqhcjze";

    fn entry(key: &str) -> String {
        let hex: String = Sha256::digest(key.as_bytes())
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect();
        format!("//{}\n", hex)
    }

    #[test]
    fn parses_badbits_header_and_entries() {
        let text = format!(
            "name: \"badbits\"\n---\n{}{}//nothex\n",
            entry("a/"),
            entry("b/")
        );
        assert_eq!(Denylist::parse(&text).len(), 2);
    }

    #[test]
    fn blocked_root_blocks_both_cid_versions_and_all_paths() {
        let list = Denylist::parse(&entry(&format!("{}/", V1)));
        assert!(list.is_blocked(V1));
        assert!(list.is_blocked(V0));
        assert!(list.is_blocked(&format!("{}/wiki/Some_Page", V0)));
        assert!(!list.is_blocked(OTHER));
    }

    #[test]
    fn blocked_path_blocks_only_that_subtree() {
        let list = Denylist::parse(&entry(&format!("{}/bad", V1)));
        assert!(list.is_blocked(&format!("{}/bad", V1)));
        assert!(list.is_blocked(&format!("{}//bad/deeper/", V1)));
        assert!(!list.is_blocked(V1));
        assert!(!list.is_blocked(&format!("{}/good", V1)));
    }

    #[test]
    fn plain_cid_lines_block_like_hashed_entries() {
        let list = Denylist::parse(&format!("# takedowns\n{}\n/ipfs/{}/bad/page/\n", V0, OTHER));
        assert_eq!(list.len(), 2);
        assert!(list.is_blocked(V1));
        assert!(list.is_blocked(&format!("{}/anything", V1)));
        assert!(list.is_blocked(&format!("{}/bad/page/deeper", OTHER)));
        assert!(!list.is_blocked(&format!("{}/bad", OTHER)));
        assert!(!list.is_blocked(OTHER));
    }

    #[test]
    fn normalizes_paths_like_a_url() {
        let n = |s: &str| normalize_item(s);
        assert_eq!(n("bafyA/wiki//X").as_deref(), Some("bafyA/wiki/X"));
        assert_eq!(n("bafyA/wiki/./X").as_deref(), Some("bafyA/wiki/X"));
        assert_eq!(n("bafyA/wiki/a/../X").as_deref(), Some("bafyA/wiki/X"));
        // RFC 3986: ".." removes the empty segment, as the gateway resolves it
        assert_eq!(
            n("bafyA/wiki//../Technetium").as_deref(),
            Some("bafyA/wiki/Technetium")
        );
        assert_eq!(n("bafyA/wiki/X/").as_deref(), Some("bafyA/wiki/X"));
        assert_eq!(n("bafyA/wiki/X#section").as_deref(), Some("bafyA/wiki/X"));
        assert_eq!(n("bafyA/wiki/X?q=1").as_deref(), Some("bafyA/wiki/X"));
        assert_eq!(n("bafyA").as_deref(), Some("bafyA"));
        assert_eq!(n("bafyA/").as_deref(), Some("bafyA"));
    }

    #[test]
    fn rejects_paths_that_climb_out_of_their_cid() {
        assert_eq!(normalize_item("bafyA/.."), None);
        assert_eq!(normalize_item("bafyA/wiki/../../bafyB"), None);
        assert_eq!(normalize_item("/bafyA"), None, "no root CID");
        assert_eq!(normalize_item(""), None);
    }

    #[test]
    fn takedowns_match_every_spelling_of_a_path() {
        // a takedown of cid/wiki/X must also block the non-canonical forms the crawler produces
        let list = Denylist::parse(&format!("{}/wiki/X\n", V1));
        assert!(list.is_blocked(&format!("{}/wiki//../X", V1)));
        assert!(list.is_blocked(&format!("{}/wiki/a/../X", V1)));
        assert!(list.is_blocked(&format!("{}/wiki//X", V1)));
        assert!(list.is_blocked(&format!("{}/wiki/./X/deeper", V1)));
        assert!(!list.is_blocked(&format!("{}/wiki/Y", V1)));
        assert!(
            list.is_blocked(&format!("{}/..", V1)),
            "escaping paths fail closed"
        );
    }

    #[test]
    fn unparseable_cid_is_blocked() {
        assert!(Denylist::empty().is_blocked("notacid/wiki"));
        assert!(!Denylist::empty().is_blocked(OTHER));
    }
}
