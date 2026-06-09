use std::cell::RefCell;
use std::collections::HashMap;

struct Foo {
    cache: RefCell<HashMap<String, String>>,
}

impl Foo {
    // EXPECT: interior_mutation_call
    fn modify(&self) {
        self.cache.borrow_mut();
    }
}
