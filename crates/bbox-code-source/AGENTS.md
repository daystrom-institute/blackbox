# Code-source contract invariants

- This crate is the dependency-clean wire and filesystem-policy leaf. It may depend on corpus identity types and small serialization/hash utilities, never HTTP clients/servers, indexers, chunkers, vector stores, model runtimes, or the daemon root.
- Wire scope is always `PublishedScope`. A caller never chooses the corpus host's `project_id`.
- Relative paths are normalized slash paths. Reject absolute paths, empty or dot components, traversal, platform separators, controls, and non-UTF-8 inputs.
- Manifest and generation hashes use the versioned length-prefixed encodings in this crate. Do not substitute short hashes or serialization-dependent hashes.
- The shared walker policy is a security and parity boundary. Both the local walker and collector exclude symlinks, hidden directories, `.bbox`, build output, unsupported extensions, and oversize files.
