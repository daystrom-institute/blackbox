# bro-cli

## Checkout-local composites

- A CLI composite may combine a typed daemon result with checkout-local work,
  but it must not link the daemon runtime. Shared validation and write logic
  belongs in a dependency-clean leaf crate.
- Canonicalize an explicit checkout root before MCP initialization and pass it
  as session transport context. Tool arguments must not provide a second
  project selector that can disagree with the admitted session checkout.

## Validation

- Always use `cargo nextest run --workspace` with a package filter for targeted checks.
