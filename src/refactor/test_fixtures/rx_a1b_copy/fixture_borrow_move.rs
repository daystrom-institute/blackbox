struct Foo {
    string: String,
}

impl Foo {
    // EXPECT: move
    fn take_string(self) -> String {
        let s = self.string;
        s
    }
}
