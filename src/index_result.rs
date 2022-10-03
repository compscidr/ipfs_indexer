use std::{collections::HashMap, fmt};

#[derive(Clone)]
pub struct IndexResult {
    pub cid: String,
    pub title: String,
    pub excerpt: String,
    pub keywords: HashMap<String, u32>, // maps keyword to occurrence count
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

impl fmt::Debug for IndexResult {
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

#[cfg(test)]
mod tests {
    use std::array::IntoIter;
    use std::{collections::HashMap, iter::FromIterator};
    use crate::index_result::IndexResult;

    #[test]
    fn single_keyword() {
        let keywords = HashMap::<_, _>::from_iter(IntoIter::new([("key1".to_string(), 1)]));

        let result = IndexResult::new(
            "1".to_string(),
            "title".to_string(),
            "excerpt".to_string(),
            keywords,
        );
        assert_eq!(result.top_n_keywords(10).len(), 1);
    }
    #[test]

    fn all_keywords() {
        let keywords = HashMap::<_, _>::from_iter(IntoIter::new([
            ("key1".to_string(), 1),
            ("key2".to_string(), 2),
        ]));

        let result = IndexResult::new(
            "1".to_string(),
            "title".to_string(),
            "excerpt".to_string(),
            keywords,
        );
        assert_eq!(result.top_n_keywords(2).len(), 2);
    }

    #[test]
    fn subset_of_keywords() {
        let keywords = HashMap::<_, _>::from_iter(IntoIter::new([
            ("key1".to_string(), 1),
            ("key2".to_string(), 2),
            ("key2".to_string(), 3),
        ]));

        let result = IndexResult::new(
            "1".to_string(),
            "title".to_string(),
            "excerpt".to_string(),
            keywords,
        );
        assert_eq!(result.top_n_keywords(2).len(), 2);
    }
}
