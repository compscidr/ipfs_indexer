/*!
 * What discovery sources produce (#213): a CID (or cid/path) to queue, or a name to resolve
 * through the gateway's /ipns/ path (a DNSLink domain, a .eth name or an IPNS key). Also decodes
 * ENS contenthash records (EIP-1577) into entries.
 */
use cid::multibase::Base;
use cid::{Cid, CidGeneric};

// the multicodecs of an IPNS key and of the identity multihash
const LIBP2P_KEY: u64 = 0x72;
const IDENTITY: u64 = 0x00;

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// "cid" or "cid/path", as enqueue() takes it
    Cid(String),
    /// resolved through /ipns/<name>/
    Name(String),
}

impl Entry {
    /// The text it's stored and shown as
    pub fn as_str(&self) -> &str {
        match self {
            Entry::Cid(s) | Entry::Name(s) => s,
        }
    }
}

/// An entry from a seeds file line, a submission or a stored row: "cid", "cid/path",
/// "/ipfs/..." or "ipfs://..."; or a domain, a .eth name or an IPNS key, bare or as "/ipns/..."
/// or "ipns://...". None for anything else.
pub fn parse(input: &str) -> Option<Entry> {
    let s = input.trim();
    if let Some(rest) = s
        .strip_prefix("/ipfs/")
        .or_else(|| s.strip_prefix("ipfs://"))
    {
        return cid_entry(rest);
    }
    if let Some(rest) = s
        .strip_prefix("/ipns/")
        .or_else(|| s.strip_prefix("ipns://"))
    {
        return name(rest.trim_end_matches('/'));
    }
    cid_entry(s).or_else(|| name(s))
}

// a CID, maybe with a path; an IPNS key isn't content, it's a name
fn cid_entry(s: &str) -> Option<Entry> {
    let s = s.trim_end_matches('/');
    let cid = Cid::try_from(s.split('/').next()?).ok()?;
    (cid.codec() != LIBP2P_KEY).then(|| Entry::Cid(s.to_string()))
}

// a domain (it has dots, which no CID's base does) or an IPNS key
fn name(s: &str) -> Option<Entry> {
    if s.contains('.') {
        return domain(s).map(Entry::Name);
    }
    let cid = Cid::try_from(s).ok()?;
    (cid.codec() == LIBP2P_KEY).then(|| Entry::Name(s.to_string()))
}

/// A DNS name, lowercased (DNSLink or .eth): 2 or more labels of [a-z0-9-], each 1-63 long and
/// not starting or ending with "-", at most 253 in all, and not an IP address
pub fn domain(s: &str) -> Option<String> {
    let s = s.to_ascii_lowercase();
    let labels: Vec<&str> = s.split('.').collect();
    let label_ok = |label: &&str| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    };
    // a numeric top-level label: an IPv4 address (IPv6 has ':' and fails the labels)
    let ip = labels
        .last()
        .is_some_and(|tld| tld.chars().all(|c| c.is_ascii_digit()));
    (s.len() <= 253 && labels.len() >= 2 && labels.iter().all(label_ok) && !ip).then_some(s)
}

/// What an ENS contenthash record (EIP-1577) points at
#[derive(Debug, PartialEq)]
pub enum Contenthash {
    Set(Entry),
    /// the record was cleared
    Cleared,
    /// not IPFS or IPNS (Swarm, Onion...), or not decodable
    Other,
}

pub fn contenthash(bytes: &[u8]) -> Contenthash {
    match bytes {
        [] => Contenthash::Cleared,
        // ipfs-ns (0xe3, as a varint), then a CID
        [0xe3, 0x01, cid @ ..] => match Cid::try_from(cid) {
            Ok(cid) if cid.codec() != LIBP2P_KEY => Contenthash::Set(Entry::Cid(cid.to_string())),
            _ => Contenthash::Other,
        },
        // ipns-ns (0xe5): an IPNS key, or a DNS name inlined as an identity multihash (which can
        // be longer than a hash, hence the larger CID)
        [0xe5, 0x01, cid @ ..] => match CidGeneric::<256>::try_from(cid) {
            Ok(cid) if cid.codec() == LIBP2P_KEY => {
                match cid.to_string_of_base(Base::Base36Lower) {
                    Ok(key) => Contenthash::Set(Entry::Name(key)),
                    Err(_) => Contenthash::Other,
                }
            }
            Ok(cid) if cid.hash().code() == IDENTITY => {
                match std::str::from_utf8(cid.hash().digest())
                    .ok()
                    .and_then(domain)
                {
                    Some(name) => Contenthash::Set(Entry::Name(name)),
                    None => Contenthash::Other,
                }
            }
            _ => Contenthash::Other,
        },
        _ => Contenthash::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cid::multihash::Multihash;

    const V1: &str = "bafybeiaysi4s6lnjev27ln5icwm6tueaw2vdykrtjkwiphwekaywqhcjze";
    const V0: &str = "QmXoypizjW3WknFiJnKLwHCnL72vedxjQkDDP1mXWo6uco";

    // an IPNS key (an ed25519 public key as an identity multihash), and its CID
    fn key_cid() -> CidGeneric<64> {
        let mut key = vec![8, 1, 18, 32];
        key.extend([7u8; 32]);
        CidGeneric::new_v1(LIBP2P_KEY, Multihash::wrap(IDENTITY, &key).unwrap())
    }

    fn key() -> String {
        key_cid().to_string_of_base(Base::Base36Lower).unwrap()
    }

    fn cid(s: &str) -> Option<Entry> {
        Some(Entry::Cid(s.to_string()))
    }

    fn name(s: &str) -> Option<Entry> {
        Some(Entry::Name(s.to_string()))
    }

    #[test]
    fn cids_with_or_without_a_prefix_or_a_path() {
        assert_eq!(parse(V1), cid(V1));
        assert_eq!(
            parse(&format!(" /ipfs/{}/wiki/A ", V1)),
            cid(&format!("{}/wiki/A", V1))
        );
        assert_eq!(parse(&format!("ipfs://{}/", V0)), cid(V0));
        assert_eq!(
            parse(&format!("{}/index.html", V1)),
            cid(&format!("{}/index.html", V1))
        );
    }

    #[test]
    fn names_are_domains_eth_names_and_ipns_keys() {
        assert_eq!(parse("Docs.IPFS.tech"), name("docs.ipfs.tech"));
        assert_eq!(parse("/ipns/vitalik.eth/"), name("vitalik.eth"));
        assert_eq!(parse(&format!("ipns://{}", key())), name(&key()));
        assert_eq!(parse(&key()), name(&key()));
        assert_eq!(
            parse(&format!("/ipfs/{}", key())),
            None,
            "a key isn't content"
        );
    }

    #[test]
    fn anything_else_is_not_an_entry() {
        let long_label = format!("{}.com", "a".repeat(64));
        let long_name = format!("{}com", "a.".repeat(127));
        for bad in [
            "",
            "wikipedia",
            "two words.com",
            "192.168.1.1",
            "::1",
            "[::1]",
            "-bad.com",
            "bad-.com",
            "a..com",
            "ftp://x.com",
            "notacid/wiki",
            "/ipns/docs.ipfs.tech/page",
            &long_label,
            &long_name,
        ] {
            assert_eq!(parse(bad), None, "{:?}", bad);
        }
    }

    fn ipfs_ns(cid_bytes: Vec<u8>) -> Vec<u8> {
        [vec![0xe3, 0x01], cid_bytes].concat()
    }

    fn ipns_ns(cid_bytes: Vec<u8>) -> Vec<u8> {
        [vec![0xe5, 0x01], cid_bytes].concat()
    }

    #[test]
    fn ipfs_records_are_cids() {
        let v1 = Cid::try_from(V1).unwrap().to_bytes();
        let v0 = Cid::try_from(V0).unwrap().to_bytes();
        assert_eq!(
            contenthash(&ipfs_ns(v1)),
            Contenthash::Set(Entry::Cid(V1.into()))
        );
        assert_eq!(
            contenthash(&ipfs_ns(v0)),
            Contenthash::Set(Entry::Cid(V0.into()))
        );
    }

    #[test]
    fn ipns_records_are_keys_or_inlined_domains() {
        assert_eq!(
            contenthash(&ipns_ns(key_cid().to_bytes())),
            Contenthash::Set(Entry::Name(key()))
        );
        // older records inline a DNSLink name as an identity multihash
        let inlined = CidGeneric::<256>::new_v1(
            0x70,
            Multihash::<256>::wrap(IDENTITY, b"app.uniswap.org").unwrap(),
        );
        assert_eq!(
            contenthash(&ipns_ns(inlined.to_bytes())),
            Contenthash::Set(Entry::Name("app.uniswap.org".into()))
        );
    }

    #[test]
    fn cleared_swarm_and_garbage_records() {
        assert_eq!(contenthash(&[]), Contenthash::Cleared);
        assert_eq!(
            contenthash(&[0xe4, 0x01, 0xfa, 0x01]),
            Contenthash::Other,
            "swarm"
        );
        assert_eq!(contenthash(&[0xe3, 0x01, 0xff, 0xff]), Contenthash::Other);
        assert_eq!(contenthash(&[0x00]), Contenthash::Other);
        assert_eq!(
            contenthash(&ipfs_ns(key_cid().to_bytes())),
            Contenthash::Other,
            "an IPNS key in an ipfs-ns record"
        );
    }
}
