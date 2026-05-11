#[derive(Copy, Clone)]
struct MyCopyThing;

struct Foo {
    thing: MyCopyThing,
}

impl Foo {
    // EXPECT: unknown_copy
    fn get(&self) -> MyCopyThing {
        self.thing
    }
}
