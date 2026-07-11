//! Probe: run the production chunker registry over sample files.
fn main() {
    let registry = bbox_chunker::default_registry();
    let mut files: Vec<String> = Vec::new();
    for arg in std::env::args().skip(1) {
        if let Some(list) = arg.strip_prefix('@') {
            files.extend(
                std::fs::read_to_string(list)
                    .unwrap()
                    .lines()
                    .map(str::to_string),
            );
        } else {
            files.push(arg);
        }
    }
    for arg in files {
        let path = std::path::PathBuf::from(&arg);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                println!("{arg}: READ ERR {e}");
                continue;
            }
        };
        let sniff = &bytes[..bytes.len().min(4096)];
        match registry.iter().find(|c| c.claims(&path, sniff)) {
            Some(chunker) => match chunker.chunk(&path, &bytes) {
                Ok((chunks, _)) => {
                    let kinds: std::collections::BTreeMap<_, usize> =
                        chunks.iter().fold(Default::default(), |mut m, c| {
                            *m.entry(c.chunk_kind.clone()).or_default() += 1;
                            m
                        });
                    println!(
                        "{} [{}] -> {} chunks {:?}",
                        path.file_name().unwrap().to_string_lossy(),
                        chunker.format_id(),
                        chunks.len(),
                        kinds
                    );
                }
                Err(e) => println!("{arg}: CHUNK ERR {e}"),
            },
            None => println!("{}: UNCLAIMED", path.file_name().unwrap().to_string_lossy()),
        }
    }
}
