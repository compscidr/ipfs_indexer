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

    // ponytail: only legacy hex entries are parsed, badbits ships nothing else today
    pub fn parse(text: &str) -> Denylist {
        let hashes = text
            .lines()
            .filter_map(|line| line.strip_prefix("//"))
            .filter_map(parse_hex32)
            .collect();
        Denylist { hashes }
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
        let mut segments = item.split('/').filter(|s| !s.is_empty());
        let cid = match segments
            .next()
            .and_then(|c| Cid::try_from(c).ok())
            .and_then(|c| c.into_v1().ok())
        {
            Some(cid) => cid,
            None => return true,
        };
        // "cid/" blocks the whole tree, then "cid/a", "cid/a/b", ...
        let mut key = format!("{}/", cid);
        if self.contains(&key) {
            return true;
        }
        for (i, segment) in segments.enumerate() {
            if i > 0 {
                key.push('/');
            }
            key.push_str(segment);
            if self.contains(&key) {
                return true;
            }
        }
        false
    }

    fn contains(&self, key: &str) -> bool {
        self.hashes
            .contains(&<[u8; 32]>::from(Sha256::digest(key.as_bytes())))
    }
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
    fn unparseable_cid_is_blocked() {
        assert!(Denylist::empty().is_blocked("notacid/wiki"));
        assert!(!Denylist::empty().is_blocked(OTHER));
    }
}
