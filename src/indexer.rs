use super::index_result::IndexResult;
use chashmap::CHashMap;
use cid::multihash::{Code, MultihashDigest};
use cid::Cid;
use log::{info, trace, warn};
use scraper::{Html, Selector};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::{sync, thread, time};

pub const RAW: u64 = 0x55;

pub struct Indexer {
    // this map is for keeping track of which entries have been indexed
    map: sync::Arc<CHashMap<String, IndexResult>>,

    // todo: we need a btree map from search term to CID which is sorted on
    // a weighted score for that search term

    // this is the outstanding queue of entries to index
    queue: (Option<Sender<String>>, Option<Receiver<String>>),
    running: sync::Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    poison_pill: Cid,
    ipfs_gateway: String,
}

// took some ideas from here: https://stackoverflow.com/questions/42043823/design-help-threading-within-a-struct
impl Indexer {
    pub fn new(ipfs_gateway: String) -> Indexer {
        let (tx, rx) = channel();
        Indexer {
            map: sync::Arc::new(CHashMap::new()),
            queue: (Some(tx), Some(rx)),
            running: sync::Arc::new(AtomicBool::new(false)),
            handle: None,
            poison_pill: Cid::new_v1(RAW, Code::Sha2_256.digest(b"Poison Pill")),
            ipfs_gateway: ipfs_gateway,
        }
    }

    pub fn enqueue_cid_with_path(&mut self, cid: Cid, relative_path: String) {
        let key = cid.to_string() + "/" + &relative_path;
        if self.map.contains_key(&key) {
            trace!("cid {} already in map", key);
            return;
        } else {
            info!("enqueueing cid {}", key);
            match &self.queue.0 {
                Some(queue) => {
                    if let Err(e) = queue.send(key.clone()) {
                        warn!("error sending cid {} to queue: {}", key, e);
                    }
                }
                None => {
                    warn!("queue is closed");
                }
            }
        }
    }

    pub fn enqueue_cid(&mut self, cid: Cid) {
        self.enqueue_cid_with_path(cid, "".to_string());
    }

    pub fn start(&mut self) {
        if self.running.load(Ordering::SeqCst) {
            warn!("indexer already running");
            return;
        }
        self.running.store(true, Ordering::SeqCst);
        let running = self.running.clone();
        let rx = self.queue.1.take().unwrap();
        let poison_pill = self.poison_pill.clone();
        let map = sync::Arc::clone(&self.map);
        let tx = self.queue.0.clone().unwrap();
        let gateway = self.ipfs_gateway.clone();
        self.handle = Some(thread::spawn(move || {
            info!("indexer thread started");
            while running.load(Ordering::SeqCst) {
                let cid = rx.recv().unwrap();
                info!("processing cid {}", cid);
                if cid == poison_pill.to_string() {
                    info!("received poison pill, stopping indexer thread");
                    break;
                }
                if map.contains_key(&cid) {
                    trace!("cid {} already in map", cid);
                    continue;
                } else {
                    let res = Self::retreive_content(gateway.clone(), cid.clone());
                    if res.0.is_some() {
                        map.insert(cid.clone(), res.0.unwrap());
                    }
                    info!(
                        "indexed cid {}. Have {} entries. Have {} more cids to add to the queue",
                        cid,
                        map.len(),
                        res.1.len()
                    );
                    for new_cid in res.1 {
                        if map.contains_key(&new_cid) {
                            trace!("cid {} already in map", new_cid);
                        } else {
                            trace!("enqueueing cid {}", new_cid);
                            tx.send(new_cid.clone()).unwrap();
                        }
                    }
                }
            }
            info!("indexer thread stopped");
        }));
        while !self.running.load(Ordering::SeqCst) {
            info!("waiting for indexer to start");
            thread::sleep(time::Duration::from_millis(100));
        }
        info!("indexer started");
    }

    fn retreive_content(gateway: String, cid: String) -> (Option<IndexResult>, Vec<String>) {
        let mut cids = Vec::new();
        let url = format!("http://{}/ipfs/{}", gateway, cid);
        info!("retreiving content from {}", url);
        let client = reqwest::blocking::Client::new();

        let result = client.get(&url).send();
        let result = match result {
            Ok(r) => r,
            Err(e) => {
                warn!("error retrieving content from {}: {}", url, e);
                if e.is_timeout() {
                    cids.push(cid);
                } else {
                    // had to add this so we keep retrying if the indexer comes up
                    // before ipfs service does in docker
                    warn!("ipfs service may be down");
                    cids.push(cid);
                }
                return (None, cids);
            }
        };
        let response = result;

        // todo: check the file type header and only proceed this way if it is actually html, otherwise index as some other file type
        // plus some meta data
        let mut html = response.text().unwrap();
        let mut document = Html::parse_document(html.as_str());

        // ipfs.io does not use normal redirects (301, 307, etc) in the status code, so reqwest client
        // can't detect it. We will have to parse the meta http-equiv tag to get the redirect url.
        let selector = Selector::parse("noscript").unwrap();
        let noscript = document.select(&selector).next();
        let mut fullcid = cid.clone();
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
                // assuming relative
                let newurl = format!("{}/{}", url, redirect_url);

                fullcid = cid.clone() + redirect_url;
                let result = client.get(&newurl).send();
                let result = match result {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("error retrieving content from {}: {}", url, e);
                        if e.is_timeout() {
                            cids.push(fullcid);
                        }
                        return (None, cids);
                    }
                };
                let response = result;
                html = response.text().unwrap();
                document = Html::parse_document(html.as_str());
            }
        }
        trace!("recevied: {:?}", html.as_str());

        let selector = Selector::parse("title").unwrap();
        let titletag = document.select(&selector).next(); // = document.select(&selector).next().unwrap().text().collect();
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
                info!("found link to {}", cid);
                cids.push(cid);
            } else if link.starts_with(format!("https://{}/ipfs/", gateway).as_str()) {
                let cid = link[13 + gateway.len()..].to_string();
                info!("found link to {}", cid);
                cids.push(cid);
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
                let full_relative = fullcid.clone()[0..last_slash].to_string() + "/" + link;
                //info!("relative link with cid: {}", full_relative);
                cids.push(full_relative);
            }
        }
        let selector = Selector::parse("body").unwrap();
        let body = document.select(&selector).next();
        let mut excerpt = "".to_string();
        let mut keywords: HashMap<String, u32> = HashMap::new();
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
                    if keywords.contains_key(&word) {
                        let count = keywords.get(&word).cloned().unwrap();
                        keywords.insert(word, count + 1);
                    } else {
                        keywords.insert(word, 1);
                    }
                }
            }

            if content.contains("no link named") {
                warn!("ipfs error on page {}, likely doesn't exist", fullcid);
            }

            let end = content.char_indices().map(|(i, _)| i).nth(128).unwrap();
            excerpt = content[..end].to_string();
        }
        let result = IndexResult::new(fullcid, title, excerpt.to_string(), keywords);

        info!("retreived content for cid {}:\n{}", cid, result);
        (Some(result), cids)
    }

    pub fn stop(&mut self) {
        if !self.running.load(Ordering::SeqCst) {
            warn!("trying to stop before indexer started");
            return;
        }
        self.enqueue_cid(self.poison_pill);
        self.running.store(false, Ordering::SeqCst);
        self.handle.take().unwrap().join().unwrap();
    }
}
