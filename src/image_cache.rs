use std::{num::NonZeroUsize, sync::Arc};

use image::DynamicImage;
use lru::LruCache;

use crate::thumbnail::ThumbKey;

#[derive(Debug, Clone)]
pub struct CachedImage {
    pub image: Arc<DynamicImage>,
    pub bytes: usize,
}

#[derive(Debug)]
pub struct MemoryImageCache {
    cache: LruCache<ThumbKey, CachedImage>,
    current_bytes: usize,
    budget_bytes: usize,
}

impl MemoryImageCache {
    pub fn new(budget_bytes: usize) -> Self {
        let capacity = NonZeroUsize::new(4096).expect("non-zero cache capacity");
        Self {
            cache: LruCache::new(capacity),
            current_bytes: 0,
            budget_bytes: budget_bytes.max(1),
        }
    }

    pub fn get(&mut self, key: &ThumbKey) -> Option<Arc<DynamicImage>> {
        self.cache.get(key).map(|cached| cached.image.clone())
    }

    pub fn insert(&mut self, key: ThumbKey, image: Arc<DynamicImage>, bytes: usize) {
        if let Some(previous) = self.cache.put(key, CachedImage { image, bytes }) {
            self.current_bytes = self.current_bytes.saturating_sub(previous.bytes);
        }
        self.current_bytes = self.current_bytes.saturating_add(bytes);
        self.evict_to_budget();
    }

    pub fn contains(&self, key: &ThumbKey) -> bool {
        self.cache.contains(key)
    }

    fn evict_to_budget(&mut self) {
        while self.current_bytes > self.budget_bytes {
            match self.cache.pop_lru() {
                Some((_key, cached)) => {
                    self.current_bytes = self.current_bytes.saturating_sub(cached.bytes);
                }
                None => break,
            }
        }
    }
}
