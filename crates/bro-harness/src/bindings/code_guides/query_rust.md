RUST SHAPES
  Items: function_item (name: identifier, parameters, return_type, body),
  struct_item, enum_item, trait_item, impl_item (type:, trait:, body:
  declaration_list), mod_item, use_declaration, macro_invocation.
  Visibility is a visibility_modifier child, not a field. Methods are
  function_item nodes inside an impl_item's declaration_list. Expressions
  sit under the _expression supertype; a call is call_expression with
  function: and arguments:.
  Each example below is executed against its fixture by the harness tests.

### Function names
```rust
pub fn beta() -> u8 { 7 }
fn gamma() {}
```
```query
(function_item name: (identifier) @name)
```
Captures: name=beta, name=gamma

### Public functions
```rust
pub fn beta() -> u8 { 7 }
fn gamma() {}
```
```query
(function_item (visibility_modifier) name: (identifier) @name)
```
Captures: name=beta

### Functions returning Result
```rust
fn load() -> Result<u8, String> { Ok(1) }
fn plain() -> u8 { 1 }
```
```query
(function_item name: (identifier) @name return_type: (generic_type type: (type_identifier) @ty (#eq? @ty "Result")))
```
Captures: name=load, ty=Result

### Methods of one type
```rust
struct Probe;
impl Probe {
    fn get(&self) -> u8 { 1 }
}
fn free() {}
```
```query
(impl_item type: (type_identifier) @ty body: (declaration_list (function_item name: (identifier) @method)))
```
Captures: ty=Probe, method=get

### Anonymous token: unsafe blocks
```rust
fn f() { unsafe { g() } }
fn g() {}
```
```query
(unsafe_block "unsafe" @kw)
```
Captures: kw=unsafe

### Supertype narrowed to a subtype
```rust
fn f() -> u8 { 1 + 2 }
```
```query
(_expression/binary_expression operator: "+" @op)
```
Captures: op=+

### Valid query, nothing to match
```rust
fn f() {}
```
```query
(struct_item name: (type_identifier) @name)
```
Captures: none

### Invalid node type
```query-invalid
(function_declaration) @f
```
Error: Invalid node type

### Invalid field name
```query-invalid
(function_item method: (identifier) @m)
```
Error: Invalid field name

### Existing field under a kind that never carries it
```query-invalid
(struct_item parameters: (parameters) @p)
```
Error: Impossible pattern
