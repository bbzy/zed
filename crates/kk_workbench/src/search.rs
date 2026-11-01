use crate::{ChangeKind, ChangedFile};
use anyhow::{Context as _, Result, bail, ensure};
use rquickjs::{Context as JavascriptContext, Function, Runtime};
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum LineKind {
    Added,
    Removed,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ChangedLine {
    pub path: String,
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub kind: LineKind,
    pub line_number: u32,
    pub new_line_number: u32,
    pub content: String,
    pub hunk_new_start: u32,
    pub edit_block: usize,
    pub context: String,
}

#[derive(Clone, Copy, Debug)]
pub struct SearchOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
    pub added: bool,
    pub removed: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            case_sensitive: false,
            whole_word: false,
            regex: false,
            added: true,
            removed: true,
        }
    }
}

#[derive(Default, Debug)]
pub struct SearchResults {
    pub indices: Vec<usize>,
    pub count: usize,
}

pub fn search_diff(
    lines: &[ChangedLine],
    query: &str,
    options: SearchOptions,
    cancelled: &Arc<AtomicBool>,
) -> Result<SearchResults> {
    search_diff_with_timeout(lines, query, options, cancelled, Duration::from_secs(2))
}

fn search_diff_with_timeout(
    lines: &[ChangedLine],
    query: &str,
    options: SearchOptions,
    cancelled: &Arc<AtomicBool>,
    timeout: Duration,
) -> Result<SearchResults> {
    let mut results = SearchResults::default();
    if query.is_empty() {
        return Ok(results);
    }

    let started = Instant::now();
    let check_cancelled = || -> Result<()> {
        ensure!(!cancelled.load(Ordering::Relaxed), "Search cancelled");
        ensure!(
            started.elapsed() < timeout,
            "Search took too long. Refine the expression and try again."
        );
        Ok(())
    };
    check_cancelled()?;
    let runtime = Runtime::new().context("Could not initialize diff search")?;
    runtime.set_memory_limit(128 * 1024 * 1024);
    runtime.set_interrupt_handler(Some(Box::new({
        let cancelled = cancelled.clone();
        move || cancelled.load(Ordering::Relaxed) || started.elapsed() >= timeout
    })));
    let context = JavascriptContext::full(&runtime).context("Could not initialize diff search")?;
    context.with(|context| -> Result<()> {
        // Use the same non-Unicode ECMAScript RegExp flags as the reference worker.
        // Passing the query as data also preserves UTF-16 matching and empty matches.
        let compile: Function = context.eval(r#"
            (query, regex, wholeWord, caseSensitive, multiline) => {
                let body = regex ? query : query.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
                if (wholeWord) body = '\\b(?:' + body + ')\\b';
                const matcher = new RegExp(body, (caseSensitive ? '' : 'i') + (multiline ? 'g' : ''));
                return text => multiline
                    ? Array.from(text.matchAll(matcher), match => match.index)
                    : (matcher.test(text) ? [0] : []);
            }
        "#).context("Could not initialize regular expression compiler")?;
        let multiline = query.contains('\n');
        let matcher: rquickjs::Result<Function> = compile.call((query, options.regex, options.whole_word,
            options.case_sensitive, multiline));
        check_cancelled()?;
        let matcher = matcher.map_err(|error| {
                let exception = context.catch();
                anyhow::anyhow!("Invalid regular expression: {error}: {exception:?}")
            })?;
        let mut add = |index| {
            results.count += 1;
            if results.indices.len() < 500 {
                results.indices.push(index);
            }
        };
        let mut start = 0;
        while let Some(first) = lines.get(start) {
            check_cancelled()?;
            let mut end = start + 1;
            if multiline {
                while let Some(next) = lines.get(end) {
                    let previous = lines.get(end - 1).context("Missing previous search line")?;
                    if next.path != first.path || next.edit_block != first.edit_block
                        || next.kind != first.kind
                        || previous.line_number.checked_add(1) != Some(next.line_number)
                    {
                        break;
                    }

                    end += 1;
                }
            }

            if (first.kind == LineKind::Added && !options.added)
                || (first.kind == LineKind::Removed && !options.removed)
            {
                start = end;
                continue;
            }

            let mut content = String::new();
            let mut offsets = Vec::with_capacity(end - start);
            let mut offset = 0;
            for line in lines.get(start..end).context("Invalid search block")? {
                if !offsets.is_empty() {
                    content.push('\n');
                    offset += 1;
                }

                offsets.push(offset);
                let text = line.content.strip_suffix('\r').unwrap_or(&line.content);
                offset += text.encode_utf16().count();
                content.push_str(text);
            }

            let found: rquickjs::Result<Vec<usize>> = matcher.call((content,));
            check_cancelled()?;
            let found = found.context("Regular expression search failed")?;
            let mut previous_row = None;
            for position in found {
                let row = offsets.partition_point(|offset| *offset <= position).saturating_sub(1);
                if previous_row != Some(row) {
                    add(start + row);
                    previous_row = Some(row);
                }
            }

            start = end;
        }
        Ok(())
    })?;
    Ok(results)
}

struct Hunk {
    old_line: u32,
    new_line: u32,
    old_remaining: u32,
    new_remaining: u32,
    new_start: u32,
    body_start: usize,
}

pub fn parse_git_diff(diff: &str) -> Result<Vec<ChangedLine>> {
    parse_diff(diff, None)
}

pub(crate) fn parse_file_diff(diff: &str, file: &ChangedFile) -> Result<Vec<ChangedLine>> {
    let old_header = if file.change_type == ChangeKind::Added {
        "/dev/null".into()
    } else {
        format!("a/{}", file.renamed_from.as_deref().unwrap_or(&file.path))
    };
    let new_header = if file.change_type == ChangeKind::Deleted {
        "/dev/null".into()
    } else {
        format!("b/{}", file.path)
    };
    let header = format!("--- {old_header}\n+++ {new_header}\n");
    if let Some(start) = diff.find(&header) {
        // KK currently emits raw control characters in paths. The structured summary
        // identifies the complete header before we parse any hunk-looking filename text.
        return parse_diff(
            diff.get(start + header.len()..)
                .context("Missing diff body")?,
            Some(file),
        );
    }

    let mut lines = parse_git_diff(diff)?;
    for line in &mut lines {
        line.path = file.path.clone();
        line.old_path = (file.change_type != ChangeKind::Added)
            .then(|| file.renamed_from.as_ref().unwrap_or(&file.path).clone());
        line.new_path = (file.change_type != ChangeKind::Deleted).then(|| file.path.clone());
    }

    Ok(lines)
}

fn parse_diff(diff: &str, known_file: Option<&ChangedFile>) -> Result<Vec<ChangedLine>> {
    let header = regex::Regex::new(r"^@@ -([0-9]+)(?:,([0-9]+))? \+([0-9]+)(?:,([0-9]+))? @@")?;
    let lines: Vec<_> = diff.split('\n').collect();
    let mut changed = Vec::new();
    let mut old_path = known_file
        .filter(|file| file.change_type != ChangeKind::Added)
        .map(|file| file.renamed_from.as_ref().unwrap_or(&file.path).clone());
    let mut new_path = known_file
        .filter(|file| file.change_type != ChangeKind::Deleted)
        .map(|file| file.path.clone());
    let mut hunk: Option<Hunk> = None;
    let mut edit_block = 0;
    let mut in_edit_block = false;
    for (index, line) in lines.iter().enumerate() {
        if line.starts_with("diff --git ") {
            if let Some(hunk) = hunk.as_ref() {
                ensure!(
                    hunk.old_remaining == 0 && hunk.new_remaining == 0,
                    "Truncated diff hunk"
                );
            }

            old_path = None;
            new_path = None;
            hunk = None;
            in_edit_block = false;
            continue;
        }

        if hunk
            .as_ref()
            .is_some_and(|hunk| hunk.old_remaining == 0 && hunk.new_remaining == 0)
        {
            hunk = None;
        }

        if let Some(captures) = header.captures(line) {
            if let Some(hunk) = hunk.as_ref() {
                ensure!(
                    hunk.old_remaining == 0 && hunk.new_remaining == 0,
                    "Truncated diff hunk"
                );
            }

            let number = |index| -> Result<u32> {
                captures
                    .get(index)
                    .map(|value| value.as_str().parse())
                    .transpose()
                    .context("Invalid diff hunk line number")
                    .map(|number| number.unwrap_or(1))
            };
            hunk = Some(Hunk {
                old_line: number(1)?,
                old_remaining: number(2)?,
                new_line: number(3)?,
                new_remaining: number(4)?,
                new_start: number(3)?,
                body_start: index + 1,
            });
            in_edit_block = false;
            continue;
        }

        let Some(hunk) = hunk.as_mut() else {
            if let Some(path) = line.strip_prefix("--- ") {
                old_path = decode_path(path)?;
            }

            if let Some(path) = line.strip_prefix("+++ ") {
                new_path = decode_path(path)?;
            }

            continue;
        };
        let Some(prefix) = line.as_bytes().first().copied() else {
            continue;
        };
        if prefix == b'\\' {
            continue;
        }

        let kind = match prefix {
            b'+' => Some(LineKind::Added),
            b'-' => Some(LineKind::Removed),
            b' ' => None,
            _ => bail!("Invalid diff hunk body"),
        };
        if let Some(kind) = kind {
            if !in_edit_block {
                edit_block += 1;
                in_edit_block = true;
            }

            let path = new_path
                .as_ref()
                .or(old_path.as_ref())
                .context("Diff hunk has no file path")?;
            let context_line =
                |line: &&str| matches!(line.as_bytes().first(), Some(b' ' | b'+' | b'-' | b'\\'));
            let mut context_start = index;
            let mut context_end = index + 1;
            while context_start > hunk.body_start.max(index.saturating_sub(3))
                && lines.get(context_start - 1).is_some_and(context_line)
            {
                context_start -= 1;
            }

            while context_end < lines.len().min(index + 4)
                && lines.get(context_end).is_some_and(context_line)
            {
                context_end += 1;
            }

            changed.push(ChangedLine {
                path: path.clone(),
                old_path: old_path.clone(),
                new_path: new_path.clone(),
                kind,
                line_number: if kind == LineKind::Added {
                    hunk.new_line
                } else {
                    hunk.old_line
                },
                new_line_number: hunk.new_line,
                content: line.get(1..).context("Invalid diff content")?.to_owned(),
                hunk_new_start: hunk.new_start,
                edit_block,
                context: lines
                    .get(context_start..context_end)
                    .context("Invalid diff context")?
                    .join("\n"),
            });
        } else {
            in_edit_block = false;
        }

        if prefix != b'+' {
            hunk.old_remaining = hunk
                .old_remaining
                .checked_sub(1)
                .context("Diff exceeds old hunk length")?;
            hunk.old_line = hunk
                .old_line
                .checked_add(1)
                .context("Diff line number overflow")?;
        }

        if prefix != b'-' {
            hunk.new_remaining = hunk
                .new_remaining
                .checked_sub(1)
                .context("Diff exceeds new hunk length")?;
            hunk.new_line = hunk
                .new_line
                .checked_add(1)
                .context("Diff line number overflow")?;
        }
    }

    if let Some(hunk) = hunk {
        ensure!(
            hunk.old_remaining == 0 && hunk.new_remaining == 0,
            "Truncated diff hunk"
        );
    }

    Ok(changed)
}

fn decode_path(raw: &str) -> Result<Option<String>> {
    if raw == "/dev/null" {
        return Ok(None);
    }

    let path = if let Some(quoted) = raw.strip_prefix('"') {
        let quoted = quoted
            .strip_suffix('"')
            .context("Unterminated quoted diff path")?;
        let mut bytes = quoted.bytes().peekable();
        let mut decoded = Vec::new();
        while let Some(byte) = bytes.next() {
            if byte != b'\\' {
                decoded.push(byte);
                continue;
            }

            let escaped = bytes.next().context("Truncated diff path escape")?;
            decoded.push(match escaped {
                b'a' => 7,
                b'b' => 8,
                b't' => b'\t',
                b'n' => b'\n',
                b'v' => 11,
                b'f' => 12,
                b'r' => b'\r',
                b'\\' => b'\\',
                b'"' => b'"',
                b'0'..=b'7' => {
                    let mut value = u16::from(escaped - b'0');
                    for _ in 0..2 {
                        if let Some(digit @ b'0'..=b'7') = bytes.peek().copied() {
                            bytes.next();
                            value = value * 8 + u16::from(digit - b'0');
                        } else {
                            break;
                        }
                    }

                    u8::try_from(value).context("Invalid octal diff path escape")?
                }
                _ => bail!("Invalid diff path escape"),
            });
        }

        String::from_utf8(decoded).context("Diff path is not UTF-8")?
    } else {
        raw.to_owned()
    };
    Ok(Some(
        path.strip_prefix("a/")
            .or_else(|| path.strip_prefix("b/"))
            .unwrap_or(&path)
            .to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ecmascript_search_preserves_character_classes_utf16_and_cr() -> Result<()> {
        let lines = parse_git_diff(
            "diff --git a/f b/f\n--- /dev/null\n+++ b/f\n@@ -0,0 +1,5 @@\n+中文\n+１２３\n+😀\n+Ab\n+x\r\r\n",
        )?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let options = SearchOptions {
            regex: true,
            ..SearchOptions::default()
        };
        for (query, expected) in [
            (r"^\w+$", vec![3]),
            (r"^\d+$", vec![]),
            (r"^..$", vec![0, 2, 3]),
            (r"^.$", vec![]),
            (r"^\cA$", vec![]),
            (r"^x\r$", vec![4]),
        ] {
            assert_eq!(
                search_diff(&lines, query, options, &cancelled)?.indices,
                expected,
                "{query}"
            );
        }
        let lines = parse_git_diff(
            "diff --git a/f b/f\n--- /dev/null\n+++ b/f\n@@ -0,0 +1,3 @@\n+😀\n+second\n+third\n",
        )?;
        assert_eq!(
            search_diff(&lines, "(?=second)\n|second", options, &cancelled)?.indices,
            vec![1]
        );
        assert!(search_diff(&lines, "(?P<rust>second)", options, &cancelled).is_err());
        Ok(())
    }

    #[test]
    fn running_ecmascript_search_observes_cancellation() -> Result<()> {
        let mut line = parse_git_diff(PATCH)?.remove(1);
        line.content = format!("{}!", "a".repeat(100_000));
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = std::thread::spawn({
            let cancelled = cancelled.clone();
            move || {
                std::thread::sleep(Duration::from_millis(20));
                cancelled.store(true, Ordering::Relaxed);
            }
        });
        let result = search_diff(
            &[line],
            "^(a+)+$",
            SearchOptions {
                regex: true,
                ..SearchOptions::default()
            },
            &cancelled,
        );
        cancellation.join().expect("cancellation thread");
        assert!(
            result
                .expect_err("cancel running search")
                .to_string()
                .contains("cancelled")
        );
        Ok(())
    }

    #[test]
    fn pathological_ecmascript_expression_is_interrupted() -> Result<()> {
        let mut line = parse_git_diff(PATCH)?.remove(1);
        line.content = format!("{}!", "a".repeat(100_000));
        let cancelled = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let error = search_diff_with_timeout(
            &[line],
            "^(a+)+$",
            SearchOptions {
                regex: true,
                ..SearchOptions::default()
            },
            &cancelled,
            Duration::from_millis(20),
        )
        .expect_err("interrupt backtracking");
        assert!(error.to_string().contains("too long"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(1));
        Ok(())
    }

    const PATCH: &str = "diff --git a/old b/new\n--- \"a/old\\tname\"\n+++ \"b/\\346\\226\\260 name\"\n@@ -1,4 +1,5 @@\n-old\n+Alpha\n+Beta\n same\n-last\n+omega\n tail\n";

    #[test]
    fn diff_paths_line_numbers_and_multiline_blocks_are_preserved() -> Result<()> {
        let lines = parse_git_diff(PATCH)?;
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0].path, "新 name");
        assert_eq!(lines[0].old_path.as_deref(), Some("old\tname"));
        assert_eq!(lines[1].line_number, 1);
        assert_eq!(lines[3].line_number, 3);
        assert_eq!(lines[3].new_line_number, 4);
        assert_eq!(lines[0].edit_block, lines[2].edit_block);
        assert_ne!(lines[2].edit_block, lines[3].edit_block);
        assert!(lines[1].context.contains("-old\n+Alpha\n+Beta"));
        let cancelled = Arc::new(AtomicBool::new(false));
        let matches = search_diff(&lines, "alpha\nbeta", SearchOptions::default(), &cancelled)?;
        assert_eq!(matches.indices, vec![1]);
        assert_eq!(matches.count, 1);
        assert_eq!(
            search_diff(&lines, "Beta\nomega", SearchOptions::default(), &cancelled)?.count,
            0
        );
        assert!(parse_git_diff(&PATCH.replace("@@ -1,4 +1,5 @@", "@@ -1,8 +1,5 @@")).is_err());
        Ok(())
    }

    #[test]
    fn search_supports_regex_options_limits_and_cancellation() -> Result<()> {
        let lines = parse_git_diff(PATCH)?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let options = SearchOptions {
            regex: true,
            ..SearchOptions::default()
        };
        assert_eq!(
            search_diff(&lines, "(?<=Al)pha", options, &cancelled)?.indices,
            vec![1]
        );
        assert!(search_diff(&lines, "[", options, &cancelled).is_err());
        let options = SearchOptions {
            whole_word: true,
            ..SearchOptions::default()
        };
        assert_eq!(search_diff(&lines, "Al", options, &cancelled)?.count, 0);
        assert_eq!(search_diff(&lines, "ALPHA", options, &cancelled)?.count, 1);
        let options = SearchOptions {
            case_sensitive: true,
            ..options
        };
        assert_eq!(search_diff(&lines, "ALPHA", options, &cancelled)?.count, 0);
        let options = SearchOptions {
            added: false,
            ..SearchOptions::default()
        };
        assert_eq!(search_diff(&lines, "Alpha", options, &cancelled)?.count, 0);
        let many_lines = vec![lines[1].clone(); 501];
        let matches = search_diff(&many_lines, "Alpha", SearchOptions::default(), &cancelled)?;
        assert_eq!(matches.count, 501);
        assert_eq!(matches.indices.len(), 500);
        cancelled.store(true, Ordering::Relaxed);
        assert!(search_diff(&lines, "Alpha", SearchOptions::default(), &cancelled).is_err());
        Ok(())
    }
}
