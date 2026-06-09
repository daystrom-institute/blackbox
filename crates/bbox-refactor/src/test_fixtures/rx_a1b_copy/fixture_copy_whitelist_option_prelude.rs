struct Foo {
    maybe: Option<u32>,
}

impl Foo {
    // EXPECT: copy
    fn get(&self) -> Option<u32> {
        self.maybe
    }
}
