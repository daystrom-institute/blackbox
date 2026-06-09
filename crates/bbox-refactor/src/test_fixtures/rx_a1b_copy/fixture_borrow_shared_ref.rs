struct Foo {
    count: u32,
}

impl Foo {
    // EXPECT: copy
    fn get_count(&self) -> u32 {
        let n = self.count;
        n
    }
}
