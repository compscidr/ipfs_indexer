use crate::index_result::IndexResult;
use actix_web::body::MessageBody;
use crossbeam_queue::ArrayQueue;
use dashmap::{DashMap, Map};
use log::{info, trace, warn};
use scraper::{Html, Selector};
use std::collections::HashMap;
use std::ops::Not;

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
}

impl IndexQueue {
    pub fn new() -> Self {
        IndexQueue {
            queue: ArrayQueue::new(1000),
            queue_set: DashMap::new(),
            map: DashMap::new(),
            keywords: DashMap::new(),
            keyword_rank: DashMap::new(),
        }
    }

    pub fn enqueue(&self, item: String) {
        if self.map.contains_key(&item) {
            trace!("Already indexed {}", item);
            return;
        }

        if self.queue_set.contains_key(&*item) {
            info!("{} already in queue", item.clone());
        } else {
            info!("Enqueuing {}", item);
            let _ = self.queue.push(item.clone());
            self.queue_set.insert(item.clone(), ());
        }
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
        let client = reqwest::blocking::Client::new();
        let result = client.get(&url).send();
        let response = match result {
            Ok(result) => result,
            Err(err) => {
                warn!("Error: {}, not re-enqueue-ing cid", err);
                // self.enqueue(cid.clone()); // for now give up on error
                return None;
            }
        };

        // todo: check the file type header and only proceed this way if it is actually html, otherwise index as some other file type
        // plus some meta data
        let html = response.text().unwrap();
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
                let client = reqwest::blocking::Client::new();
                let result = client.get(&newurl).send();
                let result = match result {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("error retrieving content from {}: {}", url, e);
                        if e.is_timeout() {
                            self.enqueue(fullcid.clone());
                        }
                        return None;
                    }
                };
                let response = result;
                let html = response.text().unwrap();
                return Some((fullcid, Html::parse_document(html.as_str())));
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
