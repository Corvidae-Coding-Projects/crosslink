use anyhow::Result;

#[must_use]
pub fn extract_body(content: &str) -> &str {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return content;
    }
    let after_first = &trimmed[3..];
    let after_first = after_first.trim_start_matches(['\r', '\n']);
    after_first.find("\n---").map_or(content, |end_idx| {
        let after_closing = &after_first[end_idx + 4..];

        after_closing
            .strip_prefix("\r\n")
            .or_else(|| after_closing.strip_prefix('\n'))
            .unwrap_or(after_closing)
    })
}

#[must_use]
pub fn parse_heading(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_end();
    if !trimmed.starts_with('#') {
        return None;
    }
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }

    let rest = &trimmed[hashes..];
    if !rest.starts_with(' ') {
        return None;
    }
    Some((hashes, rest[1..].trim()))
}

pub fn find_section_range(lines: &[&str], heading: &str) -> Result<(usize, usize)> {
    let query = heading.trim();
    let (query_level, query_text) = if query.starts_with('#') {
        match parse_heading(query) {
            Some((level, text)) => (Some(level), text),
            None => (None, query),
        }
    } else {
        (None, query)
    };

    let mut heading_idx = None;
    let mut heading_level = 0;
    for (i, line) in lines.iter().enumerate() {
        if let Some((level, text)) = parse_heading(line) {
            let text_matches = text == query_text;
            let level_matches = query_level.is_none() || query_level == Some(level);
            if text_matches && level_matches {
                heading_idx = Some(i);
                heading_level = level;
                break;
            }
        }
    }

    let start = heading_idx
        .ok_or_else(|| anyhow::anyhow!("Section heading '{heading}' not found in the page"))?;

    let mut end = lines.len();
    for (i, line) in lines.iter().enumerate().skip(start + 1) {
        if let Some((level, _)) = parse_heading(line) {
            if level <= heading_level {
                end = i;
                break;
            }
        }
    }

    Ok((start, end))
}

pub fn replace_section_content(body: &str, heading: &str, new_content: &str) -> Result<String> {
    let lines: Vec<&str> = body.lines().collect();
    let (start, end) = find_section_range(&lines, heading)?;

    let mut result = String::new();

    for line in &lines[..=start] {
        result.push_str(line);
        result.push('\n');
    }

    if !new_content.is_empty() {
        result.push('\n');
        result.push_str(new_content);
        if !new_content.ends_with('\n') {
            result.push('\n');
        }
    }

    if end < lines.len() {
        result.push('\n');
        for line in &lines[end..] {
            result.push_str(line);
            result.push('\n');
        }
    }

    Ok(result)
}

pub fn append_to_section_content(body: &str, heading: &str, new_content: &str) -> Result<String> {
    let lines: Vec<&str> = body.lines().collect();
    let (_, end) = find_section_range(&lines, heading)?;

    let mut result = String::new();

    for line in &lines[..end] {
        result.push_str(line);
        result.push('\n');
    }

    if !new_content.is_empty() {
        result.push('\n');
        result.push_str(new_content);
        if !new_content.ends_with('\n') {
            result.push('\n');
        }
    }

    if end < lines.len() {
        result.push('\n');
        for line in &lines[end..] {
            result.push_str(line);
            result.push('\n');
        }
    }

    Ok(result)
}
