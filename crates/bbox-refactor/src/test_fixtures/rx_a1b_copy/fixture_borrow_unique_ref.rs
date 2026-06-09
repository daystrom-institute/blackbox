use std::collections::HashMap;

struct Foo {
    cache: HashMap<String, String>,
}

impl Foo {
    // EXPECT: unique_ref
    fn insert(&mut self, k: String, v: String) {
        self.cache.insert(k, v);
    }
}
