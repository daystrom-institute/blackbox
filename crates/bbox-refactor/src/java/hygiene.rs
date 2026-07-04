use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JavaWhitespaceRange {
    Lines { start_line: usize, end_line: usize },
    Bytes { byte_start: usize, byte_end: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ByteRange {
    start: usize,
    end: usize,
}

#[derive(Clone, Debug)]
struct JavaLine<'a> {
    line_number: usize,
    byte_start: usize,
    content_end: usize,
    next_start: usize,
    text: &'a str,
}

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

pub fn normalize_java_whitespace_file_scoped(
    source_path: &Path,
    ranges: &[JavaWhitespaceRange],
) -> Result<Vec<FileEdit>> {
    if ranges.is_empty() {
        return normalize_java_whitespace_file(source_path);
    }
    let parsed = parse_source_file(source_path)?;
    if parsed.language != "java" {
        bail!("java.normalizeWhitespace only supports java files");
    }
    let byte_ranges = resolve_whitespace_ranges(&parsed.source, ranges)?;
    let edits = scoped_java_whitespace_edits(&parsed.source, &byte_ranges);
    if edits.is_empty() {
        return Ok(Vec::new());
    }
    Ok(vec![FileEdit {
        path: path_string(source_path),
        original_sha256: sha256_hex(parsed.source.as_bytes()),
        edits,
        new_text: None,
    }])
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

pub fn java_hygiene_file_scoped(
    project_dir: &Path,
    source_path: &Path,
    imports: bool,
    whitespace: bool,
    whitespace_ranges: &[JavaWhitespaceRange],
) -> Result<(Vec<FileEdit>, Vec<String>)> {
    if whitespace_ranges.is_empty() {
        return java_hygiene_file(project_dir, source_path, imports, whitespace);
    }
    let mut file_edits = Vec::new();
    let mut applied = Vec::new();
    if imports {
        let mut import_edits = organize_java_imports(project_dir, source_path)?;
        if !import_edits.is_empty() {
            applied.push("organize_imports".to_string());
            file_edits.append(&mut import_edits);
        }
    }
    if whitespace {
        let mut whitespace_edits =
            normalize_java_whitespace_file_scoped(source_path, whitespace_ranges)?;
        let import_spans = file_edits
            .iter()
            .flat_map(|file_edit| {
                file_edit.edits.iter().map(|edit| ByteRange {
                    start: edit.byte_start,
                    end: edit.byte_end,
                })
            })
            .collect::<Vec<_>>();
        for file_edit in &mut whitespace_edits {
            file_edit
                .edits
                .retain(|edit| !edit_overlaps_any(edit.byte_start, edit.byte_end, &import_spans));
        }
        whitespace_edits.retain(|file_edit| !file_edit.edits.is_empty());
        if !whitespace_edits.is_empty() {
            applied.push("normalize_whitespace".to_string());
            file_edits.append(&mut whitespace_edits);
        }
    }
    Ok((file_edits, applied))
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

fn resolve_whitespace_ranges(
    source: &str,
    ranges: &[JavaWhitespaceRange],
) -> Result<Vec<ByteRange>> {
    let lines = java_lines(source);
    ranges
        .iter()
        .map(|range| match *range {
            JavaWhitespaceRange::Bytes {
                byte_start,
                byte_end,
            } => {
                if byte_start > byte_end || byte_end > source.len() {
                    bail!(
                        "invalid byte range {byte_start}..{byte_end} for {} byte source",
                        source.len()
                    );
                }
                Ok(ByteRange {
                    start: byte_start,
                    end: byte_end,
                })
            }
            JavaWhitespaceRange::Lines {
                start_line,
                end_line,
            } => {
                if start_line == 0 || start_line > end_line {
                    bail!("invalid line range {start_line}..{end_line}");
                }
                let start = lines
                    .iter()
                    .find(|line| line.line_number == start_line)
                    .map(|line| line.byte_start)
                    .ok_or_else(|| anyhow!("line range starts past end of file: {start_line}"))?;
                let end = lines
                    .iter()
                    .find(|line| line.line_number == end_line)
                    .map(|line| line.next_start)
                    .ok_or_else(|| anyhow!("line range ends past end of file: {end_line}"))?;
                Ok(ByteRange { start, end })
            }
        })
        .collect()
}

fn scoped_java_whitespace_edits(source: &str, ranges: &[ByteRange]) -> Vec<TextEdit> {
    let lines = java_lines(source);
    let mut edits = Vec::new();
    let mut deleted_lines = HashSet::new();

    let mut previous_blank = false;
    for line in &lines {
        let is_blank = line.text.trim().is_empty();
        if is_blank
            && previous_blank
            && range_overlaps_any(line.byte_start, line.next_start, ranges)
        {
            edits.push(TextEdit {
                byte_start: line.byte_start,
                byte_end: line.next_start,
                replacement: String::new(),
            });
            deleted_lines.insert(line.line_number);
        }
        previous_blank = is_blank;
    }

    for line in &lines {
        if deleted_lines.contains(&line.line_number) {
            continue;
        }
        let normalized = normalize_java_line(line.text);
        if normalized != line.text && range_overlaps_any(line.byte_start, line.content_end, ranges)
        {
            edits.push(TextEdit {
                byte_start: line.byte_start,
                byte_end: line.content_end,
                replacement: normalized,
            });
        }
    }

    edits.sort_by_key(|edit| (edit.byte_start, edit.byte_end));
    let mut non_overlapping = Vec::new();
    let mut last_end = 0;
    for edit in edits {
        if edit.byte_start >= last_end {
            last_end = edit.byte_end;
            non_overlapping.push(edit);
        }
    }
    non_overlapping
}

fn java_lines(source: &str) -> Vec<JavaLine<'_>> {
    if source.is_empty() {
        return Vec::new();
    }
    let bytes = source.as_bytes();
    let mut lines = Vec::new();
    let mut byte_start = 0;
    let mut line_number = 1;
    for (idx, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            let mut content_end = idx;
            if content_end > byte_start && bytes[content_end - 1] == b'\r' {
                content_end -= 1;
            }
            lines.push(JavaLine {
                line_number,
                byte_start,
                content_end,
                next_start: idx + 1,
                text: &source[byte_start..content_end],
            });
            byte_start = idx + 1;
            line_number += 1;
        }
    }
    if byte_start < source.len() {
        lines.push(JavaLine {
            line_number,
            byte_start,
            content_end: source.len(),
            next_start: source.len(),
            text: &source[byte_start..],
        });
    }
    lines
}

fn range_overlaps_any(start: usize, end: usize, ranges: &[ByteRange]) -> bool {
    ranges
        .iter()
        .any(|range| span_overlaps_range(start, end, range.start, range.end))
}

fn edit_overlaps_any(start: usize, end: usize, ranges: &[ByteRange]) -> bool {
    ranges
        .iter()
        .any(|range| span_overlaps_range(start, end, range.start, range.end))
}

fn span_overlaps_range(start: usize, end: usize, range_start: usize, range_end: usize) -> bool {
    if start == end {
        return start >= range_start && start <= range_end;
    }
    start < range_end && end > range_start
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
