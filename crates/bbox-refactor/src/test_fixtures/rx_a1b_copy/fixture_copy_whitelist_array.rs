struct Foo {
    buf: [u8; 32],
}

impl Foo {
    // EXPECT: copy
    fn first(&self) -> u8 {
        self.buf[0]
    }
}
