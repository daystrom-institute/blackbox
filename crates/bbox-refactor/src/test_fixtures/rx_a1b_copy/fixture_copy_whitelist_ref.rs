struct Foo<'a> {
    s: &'a str,
}

impl<'a> Foo<'a> {
    // EXPECT: copy
    fn get(&self) -> &'a str {
        self.s
    }
}
