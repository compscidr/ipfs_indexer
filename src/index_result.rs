use std::{collections::HashMap, fmt};

pub struct IndexResult {
    pub cid: String,
    pub title: String,
    pub excerpt: String,
    pub keywords: HashMap<String, u32>,
}

impl IndexResult {
    pub fn new(
        cid: String,
        title: String,
        excerpt: String,
        keywords: HashMap<String, u32>,
    ) -> IndexResult {
        IndexResult {
            cid: cid,
            title: title,
            excerpt: excerpt,
            keywords: keywords,
        }
    }

    /**
     * Returns the top n keywords. Todo: use a tree structure to store the rankings of the keywords
     * so that this is faster
     */
    pub fn top_n_keywords(&self, n: u32) -> Vec<(&String, &u32)> {
        let mut hash_vec: Vec<(&String, &u32)> = self.keywords.iter().collect();
        hash_vec.sort_by(|a, b| b.1.cmp(a.1));
        hash_vec.iter().take(n as usize).cloned().collect()
    }
}

impl fmt::Display for IndexResult {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "CID: {} \nTitle: {}\n{}\nKeywords: {:?}",
            self.cid,
            self.title,
            self.excerpt,
            self.top_n_keywords(10)
        )
    }
}
