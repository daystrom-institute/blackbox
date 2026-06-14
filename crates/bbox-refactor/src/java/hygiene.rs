use super::*;

pub fn organize_java_imports(project_dir: &Path, source_path: &Path) -> Result<Vec<FileEdit>> {
    heuristic_java_organize_imports(project_dir, source_path)
}

pub fn organize_java_imports_text(project_dir: &Path, source: &str) -> Result<String> {
    heuristic_java_organize_imports_text(project_dir, source)
}

pub fn normalize_java_whitespace_text(source: &str) -> String {
    let mut lines = source.lines().map(normalize_java_line).collect::<Vec<_>>();
    normalize_java_package_import_spacing(&mut lines);
    collapse_java_blank_runs(&mut lines);
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

pub fn normalize_java_whitespace_file(source_path: &Path) -> Result<Vec<FileEdit>> {
    let parsed = parse_source_file(source_path)?;
    if parsed.language != "java" {
        bail!("java.normalizeWhitespace only supports java files");
    }
    let rewritten = normalize_java_whitespace_text(&parsed.source);
    if rewritten == parsed.source {
        return Ok(Vec::new());
    }
    Ok(vec![whole_file_edit(
        source_path,
        &parsed.source,
        rewritten,
    )])
}

pub fn java_hygiene_file(
    project_dir: &Path,
    source_path: &Path,
    imports: bool,
    whitespace: bool,
) -> Result<(Vec<FileEdit>, Vec<String>)> {
    let parsed = parse_source_file(source_path)?;
    if parsed.language != "java" {
        bail!("java.hygiene only supports java files");
    }
    let mut current = parsed.source.clone();
    let mut applied = Vec::new();
    if imports {
        let organized = organize_java_imports_text(project_dir, &current)?;
        if organized != current {
            current = organized;
            applied.push("organize_imports".to_string());
        }
    }
    if whitespace {
        let normalized = normalize_java_whitespace_text(&current);
        if normalized != current {
            current = normalized;
            applied.push("normalize_whitespace".to_string());
        }
    }
    if current == parsed.source {
        return Ok((Vec::new(), applied));
    }
    Ok((
        vec![whole_file_edit(source_path, &parsed.source, current)],
        applied,
    ))
}

fn whole_file_edit(source_path: &Path, source: &str, rewritten: String) -> FileEdit {
    FileEdit {
        path: path_string(source_path),
        original_sha256: sha256_hex(source.as_bytes()),
        edits: vec![TextEdit {
            byte_start: 0,
            byte_end: source.len(),
            replacement: rewritten,
        }],
        new_text: None,
    }
}

fn normalize_java_line(line: &str) -> String {
    let trimmed = line.trim_end();
    let leading_spaces = trimmed.bytes().take_while(|b| *b == b' ').count();
    if leading_spaces == 0 || leading_spaces % 4 != 1 {
        return trimmed.to_string();
    }
    if trimmed.as_bytes().get(leading_spaces) == Some(&b'\t') {
        return trimmed.to_string();
    }
    let rest = &trimmed[leading_spaces..];
    if !should_dedent_one_space(rest) {
        return trimmed.to_string();
    }
    format!("{}{}", " ".repeat(leading_spaces - 1), rest)
}

fn should_dedent_one_space(rest: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "return ",
        "throw ",
        "if ",
        "for ",
        "while ",
        "switch ",
        "try ",
        "catch ",
        "finally",
        "final ",
        "var ",
        "this.",
        "super.",
        "@",
        "public ",
        "protected ",
        "private ",
        "static ",
    ];
    PREFIXES.iter().any(|prefix| rest.starts_with(prefix))
}

fn normalize_java_package_import_spacing(lines: &mut Vec<String>) {
    if let Some(package_idx) = lines.iter().position(|line| {
        line.trim_start().starts_with("package ") && line.trim_end().ends_with(';')
    }) {
        normalize_single_blank_after(lines, package_idx);
    }
    if let Some(last_import_idx) = lines.iter().rposition(|line| {
        line.trim_start().starts_with("import ") && line.trim_end().ends_with(';')
    }) {
        normalize_single_blank_after(lines, last_import_idx);
    }
}

fn normalize_single_blank_after(lines: &mut Vec<String>, idx: usize) {
    let cursor = idx + 1;
    while cursor < lines.len() && lines[cursor].trim().is_empty() {
        lines.remove(cursor);
    }
    if cursor < lines.len() {
        lines.insert(cursor, String::new());
    }
}

fn collapse_java_blank_runs(lines: &mut Vec<String>) {
    let mut out = Vec::with_capacity(lines.len());
    let mut previous_blank = false;
    for line in lines.drain(..) {
        let is_blank = line.trim().is_empty();
        if is_blank && previous_blank {
            continue;
        }
        previous_blank = is_blank;
        out.push(if is_blank { String::new() } else { line });
    }
    *lines = out;
}
