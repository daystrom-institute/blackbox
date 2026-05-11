struct Foo {
    n: u32,
}

impl Foo {
    // EXPECT: copy
    fn get(&self) -> u32 {
        self.n
    }
}
