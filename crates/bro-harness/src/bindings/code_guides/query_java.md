JAVA SHAPES
  Types: class_declaration, interface_declaration, enum_declaration,
  record_declaration (name:, body:). Members: method_declaration (type:,
  name:, parameters: formal_parameters, body:), constructor_declaration
  (name:, parameters:, body: constructor_body), field_declaration (type:,
  declarator: variable_declarator). Modifiers and annotations sit in a
  modifiers child, not a field; for field facts prefer code.fields over
  field_declaration queries. expression, statement and declaration are
  supertypes; the bundled grammar carries no subtype table, so
  code.nodeKinds reports their subtypes as null.
  Each example below is executed against its fixture by the harness tests.

### Method names
```java
class Probe {
    void run() {}
    int count() { return 1; }
}
```
```query
(method_declaration name: (identifier) @name)
```
Captures: name=run, name=count

### Constructors and their parameter lists
```java
class Probe {
    Probe(int count) {}
    void run() {}
}
```
```query
(constructor_declaration name: (identifier) @name parameters: (formal_parameters) @params)
```
Captures: name=Probe, params=(int count)

### Annotated methods
```java
class Probe {
    @Override
    public String toString() { return ""; }
    void plain() {}
}
```
```query
(method_declaration (modifiers (marker_annotation name: (identifier) @ann)) name: (identifier) @name)
```
Captures: ann=Override, name=toString

### Calls to one method name
```java
class Probe {
    void run() { save(); load(); }
    void save() {}
    void load() {}
}
```
```query
(method_invocation name: (identifier) @call (#eq? @call "save"))
```
Captures: call=save

### Anonymous token: throw statements
```java
class Probe {
    void run() { throw new IllegalStateException(); }
}
```
```query
(throw_statement "throw" @kw)
```
Captures: kw=throw

### Supertype node
```java
class Probe {
    void run() { return; }
}
```
```query
(block (statement) @stmt)
```
Captures: stmt=return;

### Valid query, nothing to match
```java
class Probe {}
```
```query
(constructor_declaration name: (identifier) @name)
```
Captures: none

### Invalid node type
```query-invalid
(function_item) @f
```
Error: Invalid node type

### Invalid field name
```query-invalid
(method_declaration method: (identifier) @m)
```
Error: Invalid field name

### Existing field under a kind that never carries it
```query-invalid
(class_declaration parameters: (formal_parameters) @p)
```
Error: Impossible pattern
