use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use walkdir::WalkDir;

use super::ReindexConfig;
use crate::parser;
use crate::transcripts::types::TranscriptStorage;

/// Extract a human-readable project name from the file path.
/// Claude Code encodes project paths as directory names: `/home/user/repos/foo` → `-home-user-repos-foo`
#[cfg(test)]
pub(super) fn extract_project_from_path(file_path: &Path, projects_root: &Path) -> String {
    let relative = file_path.strip_prefix(projects_root).unwrap_or(file_path);

    // First path component is the encoded project dir
    if let Some(first) = relative.components().next() {
        let encoded = first.as_os_str().to_string_lossy();
        // Decode: leading `-` → `/`, internal `-` could be path sep or literal hyphen.
        // Best-effort: replace leading `-` sequences that look like path segments.
        // The encoded form uses `-` for `/` — we can't perfectly disambiguate, but
        // the raw encoded string is still useful for display and filtering.
        return encoded.to_string();
    }

    "unknown".to_string()
}

/// Locate the on-disk JSONL transcript file for a given session ID.
/// Scans Claude account roots (`<root>/projects/*/<session_id>.jsonl`) and
/// the Codex sessions tree (`<codex_root>/sessions/**/rollout-*<session_id>*.jsonl`).
/// Bounded WalkDir depth keeps the scan cheap enough to run per-request.
pub fn find_session_file(
    session_id: &str,
    roots: &[(String, PathBuf)],
    codex_root: Option<&Path>,
) -> Option<PathBuf> {
    if session_id.is_empty() || session_id == "pending" {
        return None;
    }

    let registry_config = ReindexConfig {
        roots: roots.to_vec(),
        codex_root: codex_root.map(Path::to_path_buf),
        meta_path: PathBuf::new(),
        projects_path: PathBuf::new(),
        knowledge_path: PathBuf::new(),
        threads_path: PathBuf::new(),
        roadmap_path: PathBuf::new(),
        // Runtime lookup path (per-request locate, never a scan): resolve the
        // harness sessions dir from the live env like from_runtime_config.
        harness_sessions_dir: Some(crate::transcripts::harness_sessions::env_sessions_dir()),
    };
    let registry = crate::transcripts::adapters::TranscriptAdapterRegistry::from_reindex_config(
        &registry_config,
    );
    for provider in [
        crate::orchestration::providers::Provider::Glm,
        crate::orchestration::providers::Provider::Deepseek,
        crate::orchestration::providers::Provider::Minimax,
        crate::orchestration::providers::Provider::Brodex,
        crate::orchestration::providers::Provider::VibeBh,
    ] {
        if let Ok(Some(location)) = registry.locate(provider, session_id)
            && matches!(
                location.storage,
                TranscriptStorage::JsonlFile
                    | TranscriptStorage::HistoryJsonl
                    | TranscriptStorage::JsonFile
            )
        {
            return Some(location.path);
        }
    }

    // Claude layout: <root>/projects/<encoded-project>/<session-id>.jsonl
    let claude_filename = format!("{session_id}.jsonl");
    for (_account, root) in roots {
        let projects_dir = root.join("projects");
        if !projects_dir.exists() {
            continue;
        }
        for entry in WalkDir::new(&projects_dir)
            .follow_links(true)
            .max_depth(3)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let p = entry.path();
            if p.file_name().is_some_and(|n| n == claude_filename.as_str()) {
                return Some(p.to_path_buf());
            }
        }
    }

    // Codex layout: <codex_root>/sessions/YYYY/MM/DD/rollout-<ts>-<session-id>.jsonl
    if let Some(cr) = codex_root {
        let sessions_dir = cr.join("sessions");
        if sessions_dir.exists() {
            for entry in WalkDir::new(&sessions_dir)
                .follow_links(true)
                .max_depth(5)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                let p = entry.path();
                if p.extension().map(|e| e != "jsonl").unwrap_or(true) {
                    continue;
                }
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default();
                if name.contains(session_id) {
                    return Some(p.to_path_buf());
                }
            }
        }
    }

    None
}

/// Extract session ID from Codex filename: rollout-YYYY-MM-DDTHH-MM-SS-UUID.jsonl
pub(super) fn extract_codex_session_id(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    // Format: rollout-2026-04-12T13-09-35-019d8319-6ffe-78b0-904b-4bfdb2a9cdb5
    // The UUID is the last 5 hyphen-separated groups (36 chars including hyphens)
    // Find the UUID portion by looking for the pattern after the datetime
    if let Some(idx) = stem.find('T') {
        // After T we have HH-MM-SS-UUID, skip past HH-MM-SS- (9 chars)
        let after_t = &stem[idx + 1..];
        if after_t.len() > 9 {
            return after_t[9..].to_string();
        }
    }
    stem
}

/// Extract cwd from the session_meta line in a Codex JSONL file (first line).
pub(super) fn extract_codex_cwd(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines().take(5) {
        let line = line.ok()?;
        let v: serde_json::Value = serde_json::from_str(&line).ok()?;
        if v["type"].as_str() == Some("session_meta") {
            return v["payload"]["cwd"].as_str().map(String::from);
        }
    }
    None
}

pub(super) struct SessionEntry {
    pub(super) session_id: String,
    pub(super) account: String,
    pub(super) project: String,
    pub(super) start_time: String,
    pub(super) duration_minutes: u64,
    #[allow(dead_code)]
    pub(super) user_messages: u64,
    pub(super) first_prompt: String,
    pub(super) name: String,
}

/// Extract the first user prompt from a Codex session file (reads first ~30 lines).
pub(super) fn extract_codex_first_prompt(path: &Path) -> String {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let reader = BufReader::new(file);
    for line in reader.lines().take(30) {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v["type"].as_str() == Some("response_item")
            && v["payload"]["role"].as_str() == Some("user")
        {
            // Extract text from content blocks
            if let Some(blocks) = v["payload"]["content"].as_array() {
                for block in blocks {
                    if block["type"].as_str() == Some("input_text") {
                        if let Some(text) = block["text"].as_str() {
                            // Skip system/env context blocks
                            if text.starts_with('<') || text.starts_with('#') {
                                continue;
                            }
                            let t = text.trim();
                            if t.len() > 120 {
                                let mut end = 120;
                                while end > 0 && !t.is_char_boundary(end) {
                                    end -= 1;
                                }
                                return format!("{}...", &t[..end]);
                            }
                            return t.to_string();
                        }
                    }
                }
            }
        }
    }
    String::new()
}

/// Detect the caller's session by finding the most recently modified transcript
/// whose tail contains the search query in a user message. Provider-agnostic.
pub(super) fn detect_caller_session(config: &ReindexConfig, query: &str) -> Option<String> {
    let query_lower = query.to_lowercase();
    let now = std::time::SystemTime::now();
    let max_age_secs = 300; // 5-minute window

    // Collect recently-modified JSONL transcripts (max_depth=4 to bound WalkDir)
    let mut recent: Vec<(PathBuf, u64)> = Vec::new();
    let scan = |dir: &Path, out: &mut Vec<(PathBuf, u64)>| {
        for entry in WalkDir::new(dir)
            .follow_links(true)
            .max_depth(4)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let p = entry.path();
            if !p.extension().map(|e| e == "jsonl").unwrap_or(false) {
                continue;
            }
            if p.to_string_lossy().contains("/subagents/") {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if let Ok(mtime) = meta.modified() {
                    if let Ok(age) = now.duration_since(mtime) {
                        if age.as_secs() < max_age_secs {
                            out.push((p.to_path_buf(), age.as_secs()));
                        }
                    }
                }
            }
        }
    };

    for (_name, root) in &config.roots {
        let projects_dir = root.join("projects");
        if projects_dir.exists() {
            scan(&projects_dir, &mut recent);
        }
    }
    if let Some(ref codex_root) = config.codex_root {
        let sessions_dir = codex_root.join("sessions");
        if sessions_dir.exists() {
            scan(&sessions_dir, &mut recent);
        }
    }

    // Most recently modified first
    recent.sort_by_key(|(_p, age)| *age);

    for (path, _age) in &recent {
        // Read tail safely — use read_to_end + lossy conversion to handle mid-char seeks
        use std::io::{Read, Seek, SeekFrom};
        let mut file = match fs::File::open(path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        let _ = file.seek(SeekFrom::Start(size.saturating_sub(65536)));
        let mut raw = Vec::new();
        let _ = file.read_to_end(&mut raw);
        let tail = String::from_utf8_lossy(&raw);

        let is_codex = path.to_string_lossy().contains("/.codex/");
        let lines: Vec<&str> = tail.lines().collect();
        let start = lines.len().saturating_sub(50);

        for line in &lines[start..] {
            let v: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };

            // Extract user message text — handle both String and Array content
            let user_text: Option<String> = if is_codex {
                if v["type"].as_str() == Some("response_item")
                    && v["payload"]["role"].as_str() == Some("user")
                {
                    parser::extract_tool_result_text(&v["payload"])
                } else {
                    None
                }
            } else {
                if v["type"].as_str() == Some("user") {
                    parser::extract_tool_result_text(&v["message"])
                } else {
                    None
                }
            };

            if let Some(text) = user_text {
                if text.to_lowercase().contains(&query_lower) {
                    if is_codex {
                        return Some(extract_codex_session_id(path));
                    } else if let Some(stem) = path.file_stem() {
                        let s = stem.to_string_lossy().to_string();
                        if looks_like_uuid(&s) {
                            return Some(s);
                        }
                    }
                }
            }
        }
    }
    None
}

/// Build a map of session UUID -> friendly name from Claude session files.
/// Claude stores sessions in ~/.claude/sessions/{pid}.json with { sessionId, name? }.
pub(super) fn load_claude_session_names(roots: &[(String, PathBuf)]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (_account, root) in roots {
        let sessions_dir = root.join("sessions");
        if !sessions_dir.exists() {
            continue;
        }
        let entries = match fs::read_dir(&sessions_dir) {
            Ok(d) => d,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e != "json").unwrap_or(true) {
                continue;
            }
            let raw = match fs::read_to_string(&path) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let v: Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let (Some(sid), Some(name)) = (v["sessionId"].as_str(), v["name"].as_str()) {
                if !name.is_empty() {
                    map.insert(sid.to_string(), name.to_string());
                }
            }
        }
    }
    map
}

/// Build a map of session UUID -> friendly name from Codex session_index.jsonl.
/// Format: one JSON object per line: { "id": "UUID", "thread_name": "friendly-name" }
/// Append-only — last entry per ID wins (supports renames).
pub(super) fn load_codex_session_names(codex_root: Option<&PathBuf>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let root = match codex_root {
        Some(r) => r,
        None => return map,
    };
    let index_path = root.join("session_index.jsonl");
    let file = match fs::File::open(&index_path) {
        Ok(f) => f,
        Err(_) => return map,
    };
    let reader = BufReader::new(file);
    for line in reader.lines().map_while(Result::ok) {
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let (Some(id), Some(name)) = (v["id"].as_str(), v["thread_name"].as_str()) {
            if !name.is_empty() {
                map.insert(id.to_string(), name.to_string());
            }
        }
    }
    map
}

/// Resolve a friendly session name to a UUID. Checks both Claude and Codex sources.
/// Returns None if the input already looks like a UUID or no match is found.
pub(super) fn resolve_session_name(
    name: &str,
    roots: &[(String, PathBuf)],
    codex_root: Option<&PathBuf>,
) -> Option<String> {
    // If it already looks like a UUID, don't resolve
    if looks_like_uuid(name) {
        return None;
    }
    let name_lower = name.to_lowercase();

    // Check Claude sessions
    let claude_names = load_claude_session_names(roots);
    for (sid, n) in &claude_names {
        if n.to_lowercase() == name_lower {
            return Some(sid.clone());
        }
    }

    // Check Codex sessions
    let codex_names = load_codex_session_names(codex_root);
    for (sid, n) in &codex_names {
        if n.to_lowercase() == name_lower {
            return Some(sid.clone());
        }
    }

    None
}

/// Quick check: does this string look like a UUID (8-4-4-4-12 hex)?
pub(super) fn looks_like_uuid(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    s.chars().enumerate().all(|(i, c)| {
        if i == 8 || i == 13 || i == 18 || i == 23 {
            c == '-'
        } else {
            c.is_ascii_hexdigit()
        }
    })
}

/// Shorten a project path for display: /home/user/repos/foo -> foo
pub(super) fn shorten_project(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

pub(super) fn is_stop_word(w: &str) -> bool {
    matches!(
        w,
        // Determiners, pronouns, prepositions, conjunctions
        "the" | "a" | "an" | "and" | "or" | "but" | "in" | "on" | "at" | "to" | "for"
        | "of" | "with" | "by" | "from" | "as" | "into" | "about" | "between" | "through"
        | "after" | "before" | "above" | "below" | "over" | "under" | "again" | "further"
        | "then" | "once" | "than" | "that" | "this" | "these" | "those"
        | "is" | "are" | "was" | "were" | "be" | "been" | "being"
        | "have" | "has" | "had" | "having" | "do" | "does" | "did" | "doing"
        | "will" | "would" | "shall" | "should" | "may" | "might" | "must" | "can" | "could"
        | "not" | "no" | "nor" | "if" | "so" | "just" | "also" | "very" | "too"
        | "he" | "she" | "it" | "we" | "they" | "you" | "me" | "him" | "her" | "us" | "them"
        | "my" | "your" | "his" | "its" | "our" | "their" | "who" | "whom" | "which" | "what"
        | "all" | "each" | "every" | "both" | "few" | "more" | "most" | "other" | "some" | "any"
        | "such" | "only" | "own" | "same" | "here" | "there" | "when" | "where" | "how" | "why"
        | "out" | "up" | "off" | "down" | "now"
        // Common verbs
        | "get" | "got" | "make" | "made" | "let" | "see" | "look" | "use" | "used" | "using"
        | "need" | "want" | "know" | "think" | "say" | "said" | "come" | "take" | "give"
        | "try" | "run" | "set" | "put" | "keep" | "find" | "way"
        // Code noise
        | "true" | "false" | "null" | "none" | "var" | "const" | "new" | "return" | "import"
        | "export" | "default" | "void" | "string" | "class" | "type" | "self"
        | "src" | "else" | "case" | "break" | "enum" | "struct"
        // Transcript noise
        | "tool" | "result" | "content" | "text" | "message" | "user" | "assistant"
        | "file" | "line" | "error" | "output" | "input" | "command"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_looks_like_uuid() {
        assert!(looks_like_uuid("019d8319-6ffe-78b0-904b-4bfdb2a9cdb5"));
        assert!(looks_like_uuid("550e8400-e29b-41d4-a716-446655440000"));
        assert!(!looks_like_uuid("not-a-uuid"));
        assert!(!looks_like_uuid("019d8319-6ffe-78b0-904b-4bfdb2a9cdb")); // too short
        assert!(!looks_like_uuid("019d8319-6ffe-78b0-904b-4bfdb2a9cdb55")); // too long
        assert!(!looks_like_uuid("019d8319x6ffe-78b0-904b-4bfdb2a9cdb5")); // wrong separator
    }

    #[test]
    fn test_shorten_project() {
        assert_eq!(shorten_project("/home/user/repos/foo"), "foo");
        assert_eq!(shorten_project("bar"), "bar");
        assert_eq!(shorten_project(""), "");
    }

    #[test]
    fn test_is_stop_word() {
        assert!(is_stop_word("the"));
        assert!(is_stop_word("and"));
        assert!(is_stop_word("true"));
        assert!(is_stop_word("null"));
        assert!(is_stop_word("tool"));
        assert!(is_stop_word("result"));
        assert!(!is_stop_word("database"));
        assert!(!is_stop_word("migration"));
        assert!(!is_stop_word("rust"));
    }

    #[test]
    fn test_extract_project_from_path() {
        let root = Path::new("/home/user/.claude/projects");
        let path = Path::new(
            "/home/user/.claude/projects/-home-user-repos-my-cool-app/transcripts/abc.jsonl",
        );
        assert_eq!(
            extract_project_from_path(path, root),
            "-home-user-repos-my-cool-app"
        );

        let path2 = Path::new("some/other/path/foo.jsonl");
        assert_eq!(extract_project_from_path(path2, root), "some");
    }

    #[test]
    fn test_extract_codex_session_id() {
        let p1 =
            Path::new("rollout-2026-04-12T13-09-35-019d8319-6ffe-78b0-904b-4bfdb2a9cdb5.jsonl");
        assert_eq!(
            extract_codex_session_id(p1),
            "019d8319-6ffe-78b0-904b-4bfdb2a9cdb5"
        );

        let p2 = Path::new("not-matching-format.jsonl");
        assert_eq!(extract_codex_session_id(p2), "not-matching-format");

        let p3 = Path::new("rollout-2026-04-12T13-09-35.jsonl");
        assert_eq!(extract_codex_session_id(p3), "rollout-2026-04-12T13-09-35");
    }

    #[test]
    fn test_is_stop_word_case() {
        assert!(is_stop_word("the"));
        assert!(!is_stop_word("THE"));
        assert!(!is_stop_word("The"));
    }

    #[test]
    fn test_looks_like_uuid_casing() {
        assert!(looks_like_uuid("019d8319-6ffe-78b0-904b-4bfdb2a9cdb5"));
        assert!(looks_like_uuid("019D8319-6FFE-78B0-904B-4BFDB2A9CDB5"));
    }
}
