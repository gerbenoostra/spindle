//! Markdown table parsing helpers.

/// Split a markdown table row into cells, respecting `\|` escapes.
pub fn split_row(row: &str) -> Vec<String> {
    let mut cells = vec![String::new()];
    let mut escaped = false;
    for c in row.chars() {
        match c {
            '\\' if !escaped => escaped = true,
            '|' if !escaped => cells.push(String::new()),
            _ => {
                if escaped && c != '|' {
                    cells.last_mut().expect("cells is never empty").push('\\');
                }
                escaped = false;
                cells.last_mut().expect("cells is never empty").push(c);
            }
        }
    }
    cells.iter().map(|cell| cell.trim().to_owned()).collect()
}

/// Return the first markdown table after `heading` as rows of trimmed cells.
pub fn table_after(markdown: &str, heading: &str) -> Vec<Vec<String>> {
    let section = markdown
        .split_once(heading)
        .unwrap_or_else(|| panic!("markdown has no `{heading}` heading"))
        .1;
    let mut rows = Vec::new();
    for line in section.lines().skip_while(|l| !l.starts_with('|')) {
        if !line.starts_with('|') {
            break;
        }
        rows.push(split_row(line.trim_matches('|')));
    }
    assert!(!rows.is_empty(), "no table found after `{heading}`");
    rows
}
