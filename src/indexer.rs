use chashmap::CHashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::{sync, thread, time};
use std::sync::atomic::{AtomicBool, Ordering};
use cid::multihash::{Code, MultihashDigest};
use cid::Cid;
use log::{info,warn};
use std::fmt;
use scraper::{Html,Selector};
use std::collections::HashMap;

pub const RAW: u64 = 0x55;

struct IndexResult {
    pub cid: String,
    pub title: String,
    pub excerpt: String,
    pub keywords: HashMap<String, u32>
}

impl IndexResult {
    pub fn new(cid: String, title: String, excerpt: String, keywords: HashMap<String, u32>) -> IndexResult {
        IndexResult {
            cid: cid,
            title: title,
            excerpt: excerpt,
            keywords: keywords
        }
    }

    /**
     * Returns the top n keywords. Todo: use a tree structure to store the rankings of the keywords
     * so that this is faster
     */
    pub fn top_n_keywords(&self, n: u32) -> Vec<(&String, &u32)>{
        let mut hash_vec: Vec<(&String, &u32)> = self.keywords.iter().collect();
        hash_vec.sort_by(|a, b| b.1.cmp(a.1));
        hash_vec.iter().take(n as usize).cloned().collect()
    }
}

impl fmt::Display for IndexResult {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "CID: {} \nTitle: {}\n{}\nKeywords: {:?}", self.cid, self.title, self.excerpt, self.top_n_keywords(10))
    }
}

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
}

// took some ideas from here: https://stackoverflow.com/questions/42043823/design-help-threading-within-a-struct
impl Indexer {
    pub fn new() -> Indexer {
        let (tx, rx) = channel();
        Indexer {
            map: sync::Arc::new(CHashMap::new()),
            queue: (Some(tx), Some(rx)),
            running: sync::Arc::new(AtomicBool::new(false)),
            handle: None,
            poison_pill: Cid::new_v1(RAW, Code::Sha2_256.digest(b"Poison Pill")),
        }
    }

    pub fn enqueue_cid_with_path(&mut self, cid: Cid, relative_path: String) {
        let key = cid.to_string() + "/" + &relative_path;
        if self.map.contains_key(&key) {
            info!("cid {} already in map", key);
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
                    info!("cid {} already in queue", cid);
                    return;
                } else {
                    let res = Self::retreive_content(cid.clone());
                    map.insert(cid.clone(), res.0.unwrap());
                    info!("indexed cid {}. Got {} more cids", cid, res.1.len());
                    for new_cid in res.1 {
                        if map.contains_key(&new_cid) {
                            info!("cid {} already in map", new_cid);
                        } else {
                            info!("enqueueing cid {}", new_cid);
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

    fn retreive_content(cid: String) -> (Option<IndexResult>, Vec<String>) {
        let mut cids = Vec::new();
        let url = format!("https://ipfs.io/ipfs/{}", cid);
        info!("retreiving content from {}", url);
        let client = reqwest::blocking::Client::new();
        // assume we get an ok response and not an error
        let response = client.get(&url).send().unwrap();

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
                return (None, cids);
            } else {
                info!("found meta http-equiv=\"refresh\"");
            }
            let start_bytes = inner_html.find("url=").unwrap_or(0);
            let end_bytes = inner_html[start_bytes..].find("\"").unwrap_or(inner_html.len()) + start_bytes;
            let redirect_url = &inner_html[start_bytes + 4..end_bytes];
            // assuming relative
            let newurl = format!("{}/{}", url, redirect_url);
            let response = client.get(&newurl).send().unwrap();
            html = response.text().unwrap();
            document = Html::parse_document(html.as_str());
            fullcid = cid.clone() + redirect_url;
        } else {
            info!("not a direct");
        }
        //info!("recevied: {:?}", html.as_str());

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
            if link.starts_with("http://ipfs.io/ipfs/") {
                let cid = link[20..].to_string();
                info!("found link to {}", cid);
                cids.push(cid);
            } else if link.starts_with("https://ipfs.io/ipfs/") {
                let cid = link[21..].to_string();
                info!("found link to {}", cid);
                cids.push(cid);
            } else if link.starts_with("http") || link.starts_with("https") {
                //info!("found link to external url: {}", link);
            } else {
                // relative link to current top cid
                //info!("found relative link to {}", link);
                let root_cid = fullcid.clone()[0..fullcid.find("/").unwrap_or(fullcid.len())].to_string();
                let full_relative = root_cid + "/" + link;
                info!("relative link with cid: {}", full_relative);
                cids.push(full_relative);
            }
        }
        let selector = Selector::parse("body").unwrap();
        let body = document.select(&selector).next();
        let mut excerpt = "".to_string();
        let mut keywords : HashMap<String, u32> = HashMap::new();
        if body.is_some() {
            // collect up the tags in the body, and get the contents within them without their tags
            let inner = body.unwrap().text().collect::<Vec<_>>();
            let mut content = inner.join(" ");
            // this leaves a ton of whitespace between things, so do this next step to remove that
            let iter = content.split_whitespace();
            content = iter.fold(String::new(), | a,b| a + b + " ");
            content = content.trim_start().trim_end().to_string();

            // get the frequency of words and turn it into a btree
            // https://stackoverflow.com/questions/41220872/how-if-possible-to-sort-a-btreemap-by-value-in-rust
            let iter = content.split_whitespace();
            for word in iter {
                if word.len() > 3 {
                    let word = word.to_lowercase();
                    if keywords.contains_key(&word) {
                        let count = keywords.get(&word).unwrap();
                        keywords.insert(word, count + 1);
                    } else {
                        keywords.insert(word, 1);
                    }
                }
            }

            excerpt = content[..128].to_string();
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