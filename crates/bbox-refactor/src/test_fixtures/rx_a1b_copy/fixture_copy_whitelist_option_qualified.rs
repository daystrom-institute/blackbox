struct Foo {
    maybe: core::option::Option<u32>,
}

impl Foo {
    // EXPECT: copy
    fn get(&self) -> core::option::Option<u32> {
        self.maybe
    }
}
