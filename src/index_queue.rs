use crate::denylist::Denylist;
use crate::index_result::IndexResult;
use actix_web::body::MessageBody;
use cid::Cid;
use crossbeam_queue::ArrayQueue;
use dashmap::{DashMap, Map};
use log::{info, trace, warn};
use scraper::{Html, Selector};
use std::collections::HashMap;
use std::io::Read;
use std::ops::Not;
use std::sync::RwLock;
use std::time::Duration;

// skip anything bigger than this rather than pulling it all into memory
const MAX_BODY_BYTES: u64 = 2 * 1024 * 1024;

/**
 * Fetch a url and return its body, but only if the gateway says it's html and it fits in
 * MAX_BODY_BYTES. Ok(None) means it was skipped: images, video, etc are never downloaded.
 */
fn fetch_html(url: &str) -> Result<Option<String>, reqwest::Error> {
    let client = reqwest::blocking::Client::builder()
        // public gateways can take ~50s on a cache miss
        .timeout(Duration::from_secs(90))
        .build()?;
    let response = client.get(url).send()?.error_for_status()?;
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !content_type.starts_with("text/html") {
        warn!("Skipping {}: content type {:?}", url, content_type);
        return Ok(None);
    }
    let mut body = Vec::new();
    if let Err(err) = response.take(MAX_BODY_BYTES + 1).read_to_end(&mut body) {
        warn!("Skipping {}: error reading body: {}", url, err);
        return Ok(None);
    }
    if body.len() as u64 > MAX_BODY_BYTES {
        warn!("Skipping {}: larger than {} bytes", url, MAX_BODY_BYTES);
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&body).into_owned()))
}

#[derive(Debug, PartialEq)]
pub enum Enqueued {
    Queued,
    InvalidCid,
    Blocked,
}

pub struct IndexQueue {
    // queue of items to index
    pub queue: ArrayQueue<String>,
    pub queue_set: DashMap<String, ()>, // used to quickly determine duplicates in queue

    // index results (cid -> result)
    pub map: DashMap<String, IndexResult>,

    // used for searching. Maps keyword to unique set of cids
    pub keywords: DashMap<String, DashMap<String, ()>>,
    // used to rank the keywords. Everytime keywords is updated, rank should be updated with the
    // number of cids in the dashmap above
    pub keyword_rank: DashMap<String, u32>,

    // checked before every fetch, see set_denylist
    denylist: RwLock<Denylist>,
}

impl IndexQueue {
    pub fn new() -> Self {
        IndexQueue {
            queue: ArrayQueue::new(1000),
            queue_set: DashMap::new(),
            map: DashMap::new(),
            keywords: DashMap::new(),
            keyword_rank: DashMap::new(),
            denylist: RwLock::new(Denylist::empty()),
        }
    }

    /**
     * Replace the denylist and drop anything already indexed that is now on it. Search only
     * returns cids still in the map, so purged pages disappear from results.
     */
    pub fn set_denylist(&self, denylist: Denylist) {
        self.map
            .retain(|item, result| !denylist.is_blocked(item) && !denylist.is_blocked(&result.cid));
        *self.denylist.write().unwrap() = denylist;
    }

    fn is_blocked(&self, item: &str) -> bool {
        let blocked = self.denylist.read().unwrap().is_blocked(item);
        if blocked {
            warn!("Denylisted, skipping {}", item);
        }
        blocked
    }

    /**
     * Queue a "cid" or "cid/path" for indexing. Nothing is queued unless it returns Queued.
     */
    pub fn enqueue(&self, item: String) -> Enqueued {
        let root = item.split('/').next().unwrap_or("");
        if Cid::try_from(root).is_err() {
            warn!("Not a CID, ignoring {}", item);
            return Enqueued::InvalidCid;
        }
        if self.is_blocked(&item) {
            return Enqueued::Blocked;
        }

        if self.map.contains_key(&item) {
            trace!("Already indexed {}", item);
            return Enqueued::Queued;
        }

        if self.queue_set.contains_key(&*item) {
            info!("{} already in queue", item.clone());
        } else {
            info!("Enqueuing {}", item);
            let _ = self.queue.push(item.clone());
            self.queue_set.insert(item.clone(), ());
        }
        Enqueued::Queued
    }

    pub fn queue_length(&self) -> usize {
        self.queue.len()
    }

    pub fn index_length(&self) -> usize {
        self.map.len()
    }

    pub fn keyword_length(&self) -> usize {
        self.keywords.len()
    }

    /*
     * Returns the top n keywords by the number of CIDs they map to
     */
    pub fn top_keywords(&self, n: usize) -> Vec<(String, u32)> {
        let all_keywords_iter = self.keyword_rank.clone().into_iter();
        let mut all_keywords: Vec<(String, u32)> = all_keywords_iter.collect();
        all_keywords.sort_by(|a, b| b.1.cmp(&a.1));
        all_keywords.iter().take(n).cloned().collect()
    }

    pub fn search(&self, query: String) -> Vec<IndexResult> {
        let mut results = Vec::new();

        // for the search, we could iterate through all of the indexed results and then search
        // each result for a keyword, or instead we could do a keyword lookup in the keyword map
        // which will give us a list of CIDs that contain the keyword.

        if self.keywords.contains_key(&query) {
            let cid_map: DashMap<String, ()> = self.keywords.get(&query).unwrap().clone();
            cid_map.into_iter().for_each(|(cid, _)| {
                if self.map.contains_key(&cid) {
                    let index_result: IndexResult = self.map.get(&cid).unwrap().clone();
                    results.push(index_result);
                }
            });
        }

        return results;
    }

    pub fn start(&self, gateway: String) {
        loop {
            if self.queue.is_empty() == false {
                let try_item = self.queue.pop();
                if try_item.is_some() {
                    let item = try_item.unwrap();
                    self.queue_set.remove(&*item);
                    if self.is_blocked(&item) {
                        continue;
                    }
                    warn!("Indexing {}", item);

                    let result = self.retrieve_content(gateway.clone(), item.clone());

                    if result.is_some() {
                        self.map.insert(item.clone(), result.unwrap());
                    } else {
                        warn!("Error retrieving CID {}", item);
                        // self.enqueue(item.clone()); // for now give up on error
                    }
                }
            }
        }
    }

    /**
     * Use the http client to obtain the page from the ipfs gateway. If there is a failure to
     * obtain the CID, we give up for now.
     */
    fn retrieve_content(&self, gateway: String, cid: String) -> Option<IndexResult> {
        let url = format!("http://{}/ipfs/{}", gateway, cid);
        warn!("Retreiving {}", url);
        let html = match fetch_html(&url) {
            Ok(Some(html)) => html,
            Ok(None) => return None,
            Err(err) => {
                warn!("Error: {}, not re-enqueue-ing cid", err);
                // self.enqueue(cid.clone()); // for now give up on error
                return None;
            }
        };
        let mut document = Html::parse_document(html.as_str());
        let result = self.detect_redirect(url, cid.clone(), document.clone());
        let mut fullcid = cid.clone();
        if result.is_some() {
            (fullcid, document) = result.unwrap();
        }
        trace!("received: {:?}", html.as_str());

        self.process_content(gateway, fullcid.clone(), document.clone())
    }

    /**
     * Determine if the response we've received requires another request from a redirect.
     * If it requires another request, we will have an updated "full cid" as well.
     */
    fn detect_redirect(&self, url: String, cid: String, document: Html) -> Option<(String, Html)> {
        // ipfs.io does not use normal redirects (301, 307, etc) in the status code, so reqwest client
        // can't detect it. We will have to parse the meta http-equiv tag to get the redirect url.
        let selector = Selector::parse("noscript").unwrap();
        let noscript = document.select(&selector).next();

        if noscript.is_some() {
            info!("found noscript");
            let inner_html = noscript.unwrap().inner_html();
            if inner_html.find("meta http-equiv=\"refresh\"").is_none() {
                warn!("no meta http-equiv=\"refresh\" found");
            } else {
                info!("found meta http-equiv=\"refresh\"");
                let start_bytes = inner_html.find("url=").unwrap_or(0);
                let end_bytes = inner_html[start_bytes..]
                    .find("\"")
                    .unwrap_or(inner_html.len())
                    + start_bytes;
                let redirect_url = &inner_html[start_bytes + 4..end_bytes];
                warn!("Redirecting to {}", redirect_url);
                // assuming relative
                let newurl = format!("{}/{}", url, redirect_url);

                let fullcid = cid.clone() + "/" + redirect_url;
                if self.is_blocked(&fullcid) {
                    return None;
                }
                return match fetch_html(&newurl) {
                    Ok(Some(html)) => Some((fullcid, Html::parse_document(html.as_str()))),
                    Ok(None) => None,
                    Err(e) => {
                        warn!("error retrieving content from {}: {}", url, e);
                        if e.is_timeout() {
                            self.enqueue(fullcid.clone());
                        }
                        None
                    }
                };
            }
        }
        None
    }

    /**
     * Process the content of the page, extract keywords, enqueue more cids, return the IndexResult
     */
    fn process_content(&self, gateway: String, cid: String, document: Html) -> Option<IndexResult> {
        let fullcid = cid.clone();

        let selector = Selector::parse("title").unwrap();
        let titletag = document.select(&selector).next();
        let mut title: String = "".to_string();
        if titletag.is_some() {
            title = titletag.unwrap().text().collect();
        }

        // todo: get all relative links and add them to the index
        let selector = Selector::parse("a").unwrap();
        for element in document.select(&selector) {
            let link = element.value().attr("href").unwrap_or("");
            if link.starts_with(format!("http://{}/ipfs/", gateway).as_str()) {
                let cid = link[12 + gateway.len()..].to_string();
                warn!("found link to {}", cid);
                self.enqueue(cid);
            } else if link.starts_with(format!("https://{}/ipfs/", gateway).as_str()) {
                let cid = link[13 + gateway.len()..].to_string();
                warn!("found link to {}", cid);
                self.enqueue(cid);
            } else if link.starts_with("http") || link.starts_with("https") {
                //info!("found link to external url: {}", link);
            } else if link.starts_with("#") {
                // ignore anchors on same page
            } else {
                // relative link to current top cid
                //info!("found relative link to {}", link);
                //let root_cid = fullcid.clone()[0..fullcid.find("/").unwrap_or(fullcid.len())].to_string();
                //let full_relative = root_cid + "/" + link;

                let last_slash = fullcid.rfind("/").unwrap_or(fullcid.len());

                if link.is_empty() {
                    // warn!("link is empty, just a link to the same doc, skipping")
                } else if link.starts_with("../A") {
                    // handle weird issue where index pages have a ../A/ relative link when they shouldn't
                    let full_relative =
                        fullcid.clone()[0..last_slash].to_string() + "/" + &link[4..];
                    warn!("relative link with cid: {}, link: {}", full_relative, link);
                    self.enqueue(full_relative);
                } else {
                    let full_relative = fullcid.clone()[0..last_slash].to_string() + "/" + link;
                    warn!("relative link with cid: {}, link: {}", full_relative, link);
                    self.enqueue(full_relative);
                }
            }
        }
        let selector = Selector::parse("body").unwrap();
        let body = document.select(&selector).next();
        let mut index_keywords: HashMap<String, u32> = HashMap::new();
        if body.is_some() {
            // collect up the tags in the body, and get the contents within them without their tags
            let inner = body.unwrap().text().collect::<Vec<_>>();
            let mut content = inner.join(" ");
            // this leaves a ton of whitespace between things, so do this next step to remove that
            let iter = content.split_whitespace();
            content = iter.fold(String::new(), |a, b| a + b + " ");
            content = content.trim_start().trim_end().to_string();

            // get the frequency of words and turn it into a btree
            // https://stackoverflow.com/questions/41220872/how-if-possible-to-sort-a-btreemap-by-value-in-rust
            let iter = content.split_whitespace();
            for word in iter {
                if word.len() > 3 {
                    let word = word.to_lowercase();
                    if index_keywords.contains_key(&word) {
                        let count = index_keywords.get(&word).cloned().unwrap();
                        index_keywords.insert(word.clone(), count + 1);
                    } else {
                        index_keywords.insert(word.clone(), 1);
                    }

                    if self.keywords.contains_key(word.as_str()) {
                        let keyword_map = self.keywords.get(word.as_str()).unwrap();
                        keyword_map.insert(cid.clone(), ());
                        let size = keyword_map.len();
                        self.keyword_rank.insert(word.clone(), size as u32);
                    } else {
                        let keyword_map: DashMap<String, ()> = DashMap::new();
                        keyword_map.insert(cid.clone(), ());
                        let size = keyword_map.len();
                        self.keywords.insert(word.clone(), keyword_map);
                        self.keyword_rank.insert(word.clone(), size as u32);
                    }
                }
            }

            if content.contains("no link named") {
                warn!("ipfs error on page {}, likely doesn't exist", fullcid);
            }

            let excerpt_len = if content.len() < 128 {
                content.len()
            } else {
                128
            };
            let end = content.char_indices().map(|(i, _)| i).nth(excerpt_len);
            if end.is_some() {
                let endunwrap = end.unwrap();
                let excerpt = content[..endunwrap].to_string();
                return Some(IndexResult::new(fullcid, title, excerpt, index_keywords));
            } else {
                return Some(IndexResult::new(
                    fullcid,
                    title,
                    "".to_string(),
                    index_keywords,
                ));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;
    use std::io::Write;
    use std::net::TcpListener;

    // serves a single http response with the given content type and body, returns its url
    fn serve_once(content_type: &'static str, body: Vec<u8>) -> String {
        serve_status_once("200 OK", content_type, body)
    }

    fn serve_status_once(
        status: &'static str,
        content_type: &'static str,
        body: Vec<u8>,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let header = format!(
                "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                status,
                content_type,
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body);
        });
        url
    }

    #[test]
    fn set_denylist_purges_indexed_and_blocks_fetches() {
        let bad = "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi";
        let good = "bafybeiaysi4s6lnjev27ln5icwm6tueaw2vdykrtjkwiphwekaywqhcjze";
        let queue = IndexQueue::new();
        for cid in [bad, good] {
            let item = format!("{}/page", cid);
            let result = IndexResult::new(item.clone(), "".into(), "".into(), HashMap::new());
            queue.map.insert(item, result);
        }

        let hash: String = sha2::Sha256::digest(format!("{}/", bad).as_bytes())
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect();
        queue.set_denylist(Denylist::parse(&format!("//{}\n", hash)));

        assert_eq!(queue.index_length(), 1);
        assert!(queue.map.contains_key(&format!("{}/page", good)));
        assert!(queue.is_blocked(bad));
        assert!(!queue.is_blocked(good));
    }

    #[test]
    fn fetch_html_returns_html() {
        let url = serve_once("text/html; charset=utf-8", b"<title>hi</title>".to_vec());
        assert_eq!(
            fetch_html(&url).unwrap().as_deref(),
            Some("<title>hi</title>")
        );
    }

    #[test]
    fn fetch_html_skips_non_html() {
        let url = serve_once("image/png", vec![0x89, b'P', b'N', b'G']);
        assert_eq!(fetch_html(&url).unwrap(), None);
    }

    #[test]
    fn fetch_html_skips_oversized_body() {
        let url = serve_once("text/html", vec![b'a'; MAX_BODY_BYTES as usize + 1]);
        assert_eq!(fetch_html(&url).unwrap(), None);
    }

    #[test]
    fn fetch_html_rejects_error_status() {
        let url = serve_status_once(
            "429 Too Many Requests",
            "text/html",
            b"<p>slow down</p>".to_vec(),
        );
        assert!(fetch_html(&url).is_err());
    }

    #[test]
    fn fetch_html_reports_connection_errors() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(fetch_html(&format!("http://127.0.0.1:{}/", port)).is_err());
    }
}
