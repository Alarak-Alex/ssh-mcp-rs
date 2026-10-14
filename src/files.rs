// Remote file scripts and semantics ported from slepp/ssh-mcp (MIT).
use crate::core::timeout;
use crate::{Error, Result, core};
use regex::Regex;
use serde_json::{Value, json};

const VIEW_SCRIPT: &str = r#"
P=$1
MODE=$2
if [ ! -e "$P" ]; then exit 21; fi
if [ -d "$P" ]; then
  printf 'DIR\0'
  find "$P" -mindepth 1 -maxdepth 2 -name ".*" -prune -o -print 2>/dev/null | sort | while IFS= read -r entry; do
    if [ -d "$entry" ]; then
      printf 'd\t%s\n' "$entry"
    else
      printf 'f\t%s\n' "$entry"
    fi
  done
  exit 0
fi
if [ ! -f "$P" ]; then exit 22; fi
SIZE=$(wc -c < "$P" | tr -d '[:space:]')
LINES=$(wc -l < "$P" | tr -d '[:space:]')
printf 'FILE\t%s\t%s\0' "$SIZE" "$LINES"
case "$MODE" in
  range)
    START=$3
    END=$4
    if [ "$END" = "-1" ]; then
      sed -n "${START},\$p" "$P"
    else
      sed -n "${START},${END}p" "$P"
    fi
    ;;
  full) cat "$P" ;;
  *)
    MAXB=$5
    if [ "$SIZE" -gt "$MAXB" ]; then head -c "$MAXB" "$P"; else cat "$P"; fi
    ;;
esac
"#;
const CREATE_SCRIPT: &str = r#"
P=$1
if [ -e "$P" ]; then exit 24; fi
D=$(dirname "$P")
if [ ! -d "$D" ]; then exit 25; fi
cat > "$P"
"#;
const EXISTING_SCRIPT: &str = r#"
P=$1
MODE=$2
if [ ! -e "$P" ]; then exit 21; fi
if [ -d "$P" ]; then exit 23; fi
if [ ! -f "$P" ]; then exit 22; fi
if [ "$MODE" = "write" ]; then cat > "$P"; else cat "$P"; fi
"#;
const GREP_SCRIPT: &str = r#"
P=$1
shift
if [ ! -e "$P" ]; then exit 21; fi
GREP=grep
if command -v ggrep >/dev/null 2>&1; then GREP=ggrep; fi
if printf 'x' | "$GREP" -P 'x' >/dev/null 2>&1; then FLAG=-P; else FLAG=-E; fi
exec "$GREP" -r -I -H "$FLAG" --exclude-dir=.git --exclude-dir=.hg --exclude-dir=.svn "$@" "$P"
"#;
const GLOB_SCRIPT: &str = r#"
P=$1
if [ ! -e "$P" ]; then exit 21; fi
if [ ! -d "$P" ]; then exit 26; fi
cd "$P" || exit 26
find . -type d \( -name .git -o -name .hg -o -name .svn \) -prune -o -type f -print
"#;

fn shield(path: &str) -> String {
    if path.starts_with('-') {
        format!("./{path}")
    } else {
        path.to_owned()
    }
}
fn script_command(script: &str, args: &[String]) -> String {
    let mut command = format!("exec sh -c {} sh", core::quote(script));
    for arg in args {
        command.push(' ');
        command.push_str(&core::quote(arg));
    }
    command
}
fn run_script(
    connection: &core::Connection,
    script: &str,
    args: &[String],
    input: Option<&str>,
    timeout: Option<f64>,
) -> Result<Value> {
    let command = script_command(script, args);
    core::run(
        &connection.argv(Some(&command), false, false)?,
        input.map(str::as_bytes),
        timeout,
        false,
    )
}
fn stdout(result: &Value) -> &str {
    result["stdout"].as_str().unwrap_or("")
}
fn check_file(result: &Value, path: &str) -> Result<()> {
    if result["timed_out"] == true {
        return Err(Error::remote_file(format!(
            "Remote operation on '{path}' timed out."
        )));
    }
    let message = match result["exit_code"].as_i64() {
        Some(0) => return Ok(()),
        Some(21) => format!("Remote path does not exist: {path}"),
        Some(22) => format!("Remote path exists but is not a regular file: {path}"),
        Some(23) => format!("Remote path is a directory, not a file: {path}"),
        Some(24) => format!("Remote path already exists: {path}"),
        Some(25) => format!("Parent directory does not exist remotely for: {path}"),
        Some(26) => format!("Remote path exists but is not a directory: {path}"),
        code => {
            let stderr = result["stderr"].as_str().unwrap_or("").trim();
            let reason = if stderr.is_empty() {
                format!(
                    "remote command exited with code {}.",
                    code.map_or("None".into(), |n| n.to_string())
                )
            } else {
                stderr.to_owned()
            };
            format!("Remote operation on '{path}' failed: {reason}")
        }
    };
    Err(Error::remote_file(message))
}
// Non-trimming fields still share core's type, empty-string, and NUL validation.
fn raw_string<'a>(args: &'a Value, key: &str, required: bool) -> Result<Option<&'a str>> {
    if !required && args.get(key).is_none_or(Value::is_null) {
        return Ok(None);
    }
    core::raw_str(args, key).map(Some)
}
fn nonnegative(args: &Value, key: &str, default: i64) -> Result<i64> {
    let n = core::integer(args, key, default)?;
    if n < 0 {
        return Err(Error::validation(format!("'{key}' must be >= 0.")));
    }
    Ok(n)
}
fn positive(args: &Value, key: &str, default: i64) -> Result<i64> {
    let n = core::integer(args, key, default)?;
    if n <= 0 {
        return Err(Error::validation(format!("'{key}' must be > 1.")));
    }
    Ok(n)
}
fn head_limit(args: &Value) -> Result<Option<usize>> {
    if args.get("head_limit").is_none_or(Value::is_null) {
        return Ok(None);
    }
    Ok(Some(positive(args, "head_limit", 1)? as usize))
}
fn truncate<T>(items: &mut Vec<T>, limit: Option<usize>) -> bool {
    if let Some(limit) = limit
        && items.len() > limit
    {
        items.truncate(limit);
        return true;
    }
    false
}
fn view_range(args: &Value) -> Result<Option<(i64, i64)>> {
    let Some(value) = args.get("view_range").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let array = value.as_array().filter(|a| a.len() == 2).ok_or_else(|| {
        Error::validation("'view_range' must be a two-element array, e.g. [start, end].")
    })?;
    let pair = json!({"start": array[0], "end": array[1]});
    let start = core::integer(&pair, "start", 0)
        .map_err(|_| Error::validation("'view_range' elements must be integers."))?;
    let end = core::integer(&pair, "end", 0)
        .map_err(|_| Error::validation("'view_range' elements must be integers."))?;
    if array.iter().any(Value::is_null) {
        return Err(Error::validation("'view_range' elements must be integers."));
    }
    if start < 1 {
        return Err(Error::validation(
            "'view_range[0]' (start line) must be >= 1.",
        ));
    }
    if end != -1 && end < start {
        return Err(Error::validation(
            "'view_range[1]' (end line) must be -1 (end of file) or >= start line.",
        ));
    }
    Ok(Some((start, end)))
}
fn view_response(
    result: &Value,
    target: &str,
    path: &str,
    mode: &str,
    max_bytes: i64,
    range: Option<(i64, i64)>,
) -> Result<Value> {
    check_file(result, path)?;
    let (header, payload) = stdout(result)
        .split_once('\0')
        .unwrap_or((stdout(result), ""));
    if header == "DIR" {
        let entries: Vec<Value> = payload
            .split('\n')
            .filter(|l| !l.is_empty())
            .map(|line| {
                let (kind, entry) = line.split_once('\t').unwrap_or((line, ""));
                json!({"path":entry,"type":if kind == "d" {"directory"} else {"file"}})
            })
            .collect();
        return Ok(
            json!({"ok":true,"target":target,"path":path,"is_directory":true,"entries":entries}),
        );
    }
    let parts: Vec<_> = header.split('\t').collect();
    let bad_response =
        || Error::remote_file(format!("Unexpected response reading remote file: {path}"));
    if parts.len() != 3 || parts[0] != "FILE" {
        return Err(bad_response());
    }
    let size = parts[1].parse::<u64>().map_err(|_| bad_response())?;
    let lines = parts[2].parse::<u64>().map_err(|_| bad_response())?;
    let mut response = json!({"ok":true,"target":target,"path":path,"is_directory":false,"content":payload,"size_bytes":size,"total_lines":lines,"truncated":mode == "auto" && size > max_bytes as u64});
    if let Some((start, end)) = range {
        response["start_line"] = json!(start);
        response["end_line"] = json!(end);
    }
    Ok(response)
}
fn view(connection: &core::Connection, args: &Value) -> Result<Value> {
    let path = core::req_str(args, "path")?;
    let force = core::boolean(args, "force_read_large_files", false)?;
    let max_bytes = positive(args, "max_bytes", 20_480)?;
    let timeout = timeout(args)?;
    let range = view_range(args)?;
    let mode = if range.is_some() {
        "range"
    } else if force {
        "full"
    } else {
        "auto"
    };
    let (start, end) = range.map_or((String::new(), String::new()), |(s, e)| {
        (s.to_string(), e.to_string())
    });
    let result = run_script(
        connection,
        VIEW_SCRIPT,
        &[
            shield(path),
            mode.into(),
            start,
            end,
            if mode == "auto" {
                max_bytes.to_string()
            } else {
                String::new()
            },
        ],
        None,
        timeout,
    )?;
    view_response(&result, &connection.target, path, mode, max_bytes, range)
}
fn content<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    let content = args
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::validation(format!("'{key}' is required and must be a string.")))?;
    if content.contains('\0') {
        return Err(Error::validation(format!(
            "'{key}' cannot contain NUL bytes."
        )));
    }
    Ok(content)
}
fn create(connection: &core::Connection, args: &Value) -> Result<Value> {
    let path = core::req_str(args, "path")?;
    let text = content(args, "content")?;
    let result = run_script(
        connection,
        CREATE_SCRIPT,
        &[shield(path)],
        Some(text),
        timeout(args)?,
    )?;
    check_file(&result, path)?;
    Ok(json!({"ok":true,"target":connection.target,"path":path,"bytes_written":text.len()}))
}
fn edits(args: &Value) -> Result<Vec<(&str, &str)>> {
    let array = args
        .get("edits")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| {
            Error::validation(
                "'edits' is required and must be a non-empty array of {old_str, new_str} objects.",
            )
        })?;
    array
        .iter()
        .enumerate()
        .map(|(i, item)| {
            if !item.is_object() {
                return Err(Error::validation(format!(
                    "'edits[{i}]' must be an object with 'old_str' and 'new_str'."
                )));
            }
            let old = item
                .get("old_str")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    Error::validation(format!(
                        "'edits[{i}].old_str' is required and must be a non-empty string."
                    ))
                })?;
            if old.contains('\0') {
                return Err(Error::validation(format!(
                    "'edits[{i}].old_str' cannot contain NUL bytes."
                )));
            }
            let new = item.get("new_str").and_then(Value::as_str).ok_or_else(|| {
                Error::validation(format!(
                    "'edits[{i}].new_str' is required and must be a string."
                ))
            })?;
            if new.contains('\0') {
                return Err(Error::validation(format!(
                    "'edits[{i}].new_str' cannot contain NUL bytes."
                )));
            }
            Ok((old, new))
        })
        .collect()
}
fn apply_edits(content: &str, edits: &[(&str, &str)]) -> Result<String> {
    let mut updated = content.to_owned();
    for (index, (old, new)) in edits.iter().enumerate() {
        let mut locations = updated.match_indices(old);
        let first = locations.next().map(|(offset, _)| offset);
        let count = usize::from(first.is_some()) + locations.count();
        if count == 0 {
            return Err(Error::remote_file(format!(
                "'edits[{index}].old_str' was not found in the file (checked after applying any earlier edits in this call)."
            )));
        }
        if count > 1 {
            return Err(Error::remote_file(format!(
                "'edits[{index}].old_str' matches {count} locations in the file; it must match exactly one. Include more surrounding context to make it unique."
            )));
        }
        let offset = first.unwrap();
        updated.replace_range(offset..offset + old.len(), new);
    }
    Ok(updated)
}
fn edit(connection: &core::Connection, args: &Value) -> Result<Value> {
    let path = core::req_str(args, "path")?;
    let edits = edits(args)?;
    let timeout = timeout(args)?;
    let shielded = shield(path);
    let result = run_script(
        connection,
        EXISTING_SCRIPT,
        &[shielded.clone(), "read".into()],
        None,
        timeout,
    )?;
    check_file(&result, path)?;
    // Finish the complete batch before opening the remote file for writing.
    let updated = apply_edits(stdout(&result), &edits)?;
    let result = run_script(
        connection,
        EXISTING_SCRIPT,
        &[shielded, "write".into()],
        Some(&updated),
        timeout,
    )?;
    check_file(&result, path)?;
    Ok(
        json!({"ok":true,"target":connection.target,"path":path,"edits_applied":edits.len(),"bytes_written":updated.len()}),
    )
}

fn expand_braces(pattern: &str) -> Vec<String> {
    let mut opening = None;
    for (i, c) in pattern.char_indices() {
        if c == '{' {
            opening = Some(i);
        }
        if c == '}'
            && let Some(start) = opening
        {
            let mut expanded = Vec::new();
            for option in pattern[start + 1..i].split(',') {
                expanded.extend(expand_braces(&format!(
                    "{}{option}{}",
                    &pattern[..start],
                    &pattern[i + 1..]
                )));
            }
            return expanded;
        }
    }
    vec![pattern.to_owned()]
}
fn segment_regex(segment: &str) -> Result<Regex> {
    let chars: Vec<char> = segment.chars().collect();
    let mut regex = String::from("(?s)\\A(?:");
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => regex.push_str("[^/]*"),
            '?' => regex.push_str("[^/]"),
            '[' => {
                let mut end = i + 1;
                if end < chars.len() && matches!(chars[end], '!' | '^') {
                    end += 1;
                }
                if end < chars.len() && chars[end] == ']' {
                    end += 1;
                }
                while end < chars.len() && chars[end] != ']' {
                    end += 1;
                }
                if end == chars.len() {
                    regex.push_str("\\[");
                } else {
                    regex.push('[');
                    let mut start = i + 1;
                    if matches!(chars[start], '!' | '^') {
                        regex.push('^');
                        start += 1;
                    }
                    for &c in &chars[start..end] {
                        // Python's re character classes have no set-operation syntax.
                        if matches!(c, '\\' | '[' | ']' | '&' | '~' | '^') {
                            regex.push('\\');
                        }
                        regex.push(c);
                    }
                    regex.push(']');
                    i = end;
                }
            }
            c => regex.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    regex.push_str(")\\z");
    Regex::new(&regex).map_err(|e| Error::internal(e.to_string()))
}
struct Segment {
    regex: Option<Regex>,
    dot: bool,
}
struct Glob {
    variants: Vec<Vec<Segment>>,
    memo: std::collections::HashMap<(usize, usize), bool>,
}
impl Glob {
    fn new(pattern: &str) -> Result<Self> {
        let variants = expand_braces(pattern)
            .iter()
            .map(|variant| {
                variant
                    .split('/')
                    .map(|s| {
                        Ok(Segment {
                            regex: if s == "**" {
                                None
                            } else {
                                Some(segment_regex(s)?)
                            },
                            dot: s.starts_with('.'),
                        })
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            variants,
            memo: std::collections::HashMap::new(),
        })
    }
    fn matches(&mut self, candidate: &str) -> bool {
        for segments in &self.variants {
            self.memo.clear();
            if Self::match_from(segments, candidate, 0, 0, &mut self.memo) {
                return true;
            }
        }
        false
    }
    fn match_from(
        segments: &[Segment],
        candidate: &str,
        index: usize,
        offset: usize,
        memo: &mut std::collections::HashMap<(usize, usize), bool>,
    ) -> bool {
        if let Some(&value) = memo.get(&(index, offset)) {
            return value;
        }
        let rest = &candidate[offset..];
        let segment = &segments[index];
        let last = index + 1 == segments.len();
        let found = if let Some(regex) = &segment.regex {
            let (part, tail) = rest
                .split_once('/')
                .map_or((rest, None), |(a, b)| (a, Some(b)));
            (segment.dot || !part.starts_with('.'))
                && regex.is_match(part)
                && if last {
                    tail.is_none()
                } else if let Some(tail) = tail {
                    Self::match_from(
                        segments,
                        candidate,
                        index + 1,
                        candidate.len() - tail.len(),
                        memo,
                    )
                } else {
                    false
                }
        } else if last {
            rest.is_empty()
                || rest
                    .split('/')
                    .all(|s| !s.is_empty() && !s.starts_with('.'))
        } else {
            let mut cursor = offset;
            let mut matched = Self::match_from(segments, candidate, index + 1, cursor, memo);
            while !matched {
                let Some((part, tail)) = candidate[cursor..].split_once('/') else {
                    break;
                };
                if part.is_empty() || part.starts_with('.') {
                    break;
                }
                cursor = candidate.len() - tail.len();
                matched = Self::match_from(segments, candidate, index + 1, cursor, memo);
            }
            matched
        };
        memo.insert((index, offset), found);
        found
    }
}
fn grep_line(line: &str) -> Option<Value> {
    // Find the first :digits: or -digits-; the two separators must agree.
    for (i, sep) in line
        .bytes()
        .enumerate()
        .filter(|(_, b)| matches!(b, b':' | b'-'))
    {
        let bytes = line.as_bytes();
        let mut end = i + 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end > i + 1 && bytes.get(end) == Some(&sep) {
            let number = line[i + 1..end].parse::<u64>().ok()?;
            let mut entry = json!({"path":&line[..i],"line_number":number,"line":&line[end+1..]});
            if sep == b'-' {
                entry["is_context"] = json!(true);
            }
            return Some(entry);
        }
    }
    None
}
fn grep(connection: &core::Connection, args: &Value) -> Result<Value> {
    let pattern = raw_string(args, "pattern", true)?.unwrap();
    let path = raw_string(args, "path", false)?.unwrap_or(".");
    let filter = raw_string(args, "glob", false)?;
    let insensitive = core::boolean(args, "case_insensitive", false)?;
    let mode = core::opt_str(args, "output_mode")?.unwrap_or("files_with_matches");
    if !matches!(mode, "content" | "count" | "files_with_matches") {
        return Err(Error::validation(
            "'output_mode' must be one of: content, count, files_with_matches.",
        ));
    }
    let context = if args.get("context").is_none_or(Value::is_null) {
        None
    } else {
        Some(nonnegative(args, "context", 0)?)
    };
    let mut before = nonnegative(args, "context_before", 0)?;
    let mut after = nonnegative(args, "context_after", 0)?;
    if let Some(context) = context {
        before = context;
        after = context;
    }
    if (before != 0 || after != 0) && mode != "content" {
        return Err(Error::validation(
            "'context'/'context_before'/'context_after' only apply when output_mode is 'content'.",
        ));
    }
    let limit = head_limit(args)?;
    let timeout = timeout(args)?;
    let mut script_args = vec![shield(path)];
    script_args.push(
        match mode {
            "content" => "-n",
            "count" => "-c",
            _ => "-l",
        }
        .into(),
    );
    if mode == "content" {
        if before != 0 && before == after {
            script_args.extend(["-C".into(), before.to_string()]);
        } else {
            if before != 0 {
                script_args.extend(["-B".into(), before.to_string()]);
            }
            if after != 0 {
                script_args.extend(["-A".into(), after.to_string()]);
            }
        }
    }
    if insensitive {
        script_args.push("-i".into());
    }
    if let Some(filter) = filter {
        for variant in expand_braces(filter) {
            script_args.extend(["--include".into(), variant]);
        }
    }
    script_args.extend(["--".into(), pattern.into()]);
    let result = run_script(connection, GREP_SCRIPT, &script_args, None, timeout)?;
    if result["timed_out"] == true {
        return Err(Error::remote_file(format!(
            "Remote search under '{path}' timed out."
        )));
    }
    if result["exit_code"] == 21 {
        return Err(Error::remote_file(format!(
            "Remote path does not exist: {path}"
        )));
    }
    if let Some(code) = result["exit_code"].as_i64().filter(|n| *n > 1) {
        let stderr = result["stderr"].as_str().unwrap_or("").trim();
        let reason = if stderr.is_empty() {
            format!("grep exited with code {code}.")
        } else {
            stderr.into()
        };
        return Err(Error::remote_file(format!(
            "Remote search under '{path}' failed: {reason}"
        )));
    }
    let mut lines: Vec<_> = stdout(&result).split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let mut matches: Vec<Value> = match mode {
        "files_with_matches" => lines.into_iter().map(|s| json!(s)).collect(),
        "count" => lines
            .into_iter()
            .filter_map(|line| {
                let (path, count) = line.rsplit_once(':')?;
                let count = count.trim().parse::<i64>().ok()?;
                if path.is_empty() || count <= 0 {
                    return None;
                }
                Some(json!({"path":path,"count":count}))
            })
            .collect(),
        _ => lines.into_iter().filter_map(grep_line).collect(),
    };
    let truncated = truncate(&mut matches, limit);
    Ok(
        json!({"ok":true,"target":connection.target,"path":path,"pattern":pattern,"output_mode":mode,"matches":matches,"truncated":truncated}),
    )
}
fn glob(connection: &core::Connection, args: &Value) -> Result<Value> {
    let pattern = raw_string(args, "pattern", true)?.unwrap();
    let path = raw_string(args, "path", false)?.unwrap_or(".");
    let limit = head_limit(args)?;
    let result = run_script(
        connection,
        GLOB_SCRIPT,
        &[shield(path)],
        None,
        timeout(args)?,
    )?;
    check_file(&result, path)?;
    let mut glob = Glob::new(pattern)?;
    let mut matches: Vec<&str> = stdout(&result)
        .split('\n')
        .filter(|s| !s.is_empty())
        .map(|s| s.strip_prefix("./").unwrap_or(s))
        .filter(|s| glob.matches(s))
        .collect();
    matches.sort_unstable();
    let truncated = truncate(&mut matches, limit);
    Ok(
        json!({"ok":true,"target":connection.target,"path":path,"pattern":pattern,"matches":matches,"truncated":truncated}),
    )
}
pub fn call(tool: &str, args: &Value) -> Result<Value> {
    let connection = core::Connection::from_args(args)?;
    match tool {
        "ssh_view" => view(&connection, args),
        "ssh_create" => create(&connection, args),
        "ssh_edit" => edit(&connection, args),
        "ssh_grep" => grep(&connection, args),
        "ssh_glob" => glob(&connection, args),
        _ => Err(Error::validation(format!("Unknown file tool: {tool}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("ssh-mcp-files-{}", uuid::Uuid::new_v4().simple()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self, name: &str) -> String {
            self.0.join(name).to_str().unwrap().to_owned()
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn local(script: &str, args: &[&str], input: Option<&str>) -> Value {
        let args: Vec<_> = args.iter().map(|s| (*s).to_owned()).collect();
        core::run(
            &["/bin/sh".into(), "-c".into(), script_command(script, &args)],
            input.map(str::as_bytes),
            Some(5.0),
            false,
        )
        .unwrap()
    }
    #[test]
    fn edits_are_sequential_transactional_and_preserve_newlines() {
        let original = "alpha\r\nbeta\rgamma\n";
        assert_eq!(
            apply_edits(original, &[("alpha", "δ"), ("δ", "finished")]).unwrap(),
            "finished\r\nbeta\rgamma\n"
        );
        assert_eq!(apply_edits("aaa", &[("aa", "x")]).unwrap(), "xa");
        assert!(
            apply_edits(original, &[("alpha", "changed"), ("missing", "bad")])
                .unwrap_err()
                .message
                .contains("edits[1]")
        );
        assert_eq!(original, "alpha\r\nbeta\rgamma\n");
        let error = apply_edits("one two", &[("one", "two"), ("two", "bad")]).unwrap_err();
        assert!(error.message.contains("matches 2 locations"));
        assert_eq!(error.kind, "remote_file_error");
        assert_eq!(apply_edits("delete", &[("delete", "")]).unwrap(), "");
        for args in [
            json!({}),
            json!({"edits":[]}),
            json!({"edits":[null]}),
            json!({"edits":[{"old_str":"","new_str":"x"}]}),
            json!({"edits":[{"old_str":"x","new_str":"\0"}]}),
            json!({"edits":[{"old_str":"\0","new_str":"x"}]}),
        ] {
            assert_eq!(edits(&args).unwrap_err().kind, "validation_error");
        }
        assert_eq!(
            edits(&json!({"edits":[{"old_str":" ","new_str":""}]})).unwrap(),
            vec![(" ", "")]
        );
    }
    #[test]
    fn scripts_preserve_bytes_page_and_refuse_destructive_inputs() {
        let dir = Directory::new();
        let path = dir.path("quoted ' ; $(echo unsafe).txt");
        let text = "α\r\nline2\r\nlast\r";
        check_file(&local(CREATE_SCRIPT, &[&path], Some(text)), &path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), text.as_bytes());
        let existing = local(CREATE_SCRIPT, &[&path], Some("clobber"));
        assert_eq!(existing["exit_code"], 24);
        assert!(
            check_file(&existing, &path)
                .unwrap_err()
                .message
                .contains("already exists")
        );
        assert_eq!(fs::read(&path).unwrap(), text.as_bytes());
        let missing = dir.path("missing/file");
        assert_eq!(
            local(CREATE_SCRIPT, &[&missing], Some("x"))["exit_code"],
            25
        );
        let empty = dir.path("empty");
        assert_eq!(local(CREATE_SCRIPT, &[&empty], Some(""))["exit_code"], 0);
        assert!(fs::read(&empty).unwrap().is_empty());
        let full = local(VIEW_SCRIPT, &[&path, "full", "", "", ""], None);
        let response = view_response(&full, "host", &path, "full", 1, None).unwrap();
        assert_eq!(response["content"], text);
        assert_eq!(response["size_bytes"], text.len());
        assert_eq!(response["total_lines"], 2);
        assert_eq!(response["truncated"], false);
        let page = local(VIEW_SCRIPT, &[&path, "range", "2", "-1", ""], None);
        assert_eq!(
            view_response(&page, "host", &path, "range", 1, Some((2, -1))).unwrap()["content"],
            "line2\r\nlast\r"
        );
        let page = local(VIEW_SCRIPT, &[&path, "range", "2", "2", ""], None);
        assert_eq!(
            view_response(&page, "host", &path, "range", 1, Some((2, 2))).unwrap()["content"],
            "line2\r\n"
        );
        let clipped = local(VIEW_SCRIPT, &[&path, "auto", "", "", "1"], None);
        let response = view_response(&clipped, "host", &path, "auto", 1, None).unwrap();
        assert_eq!(response["content"], "�");
        assert_eq!(response["truncated"], true);
        let fetched = local(EXISTING_SCRIPT, &[&path, "read"], None);
        assert!(apply_edits(stdout(&fetched), &[("line2", "good"), ("absent", "bad")]).is_err());
        assert_eq!(fs::read(&path).unwrap(), text.as_bytes());
        let updated = apply_edits(stdout(&fetched), &[("line2", "changed")]).unwrap();
        check_file(
            &local(EXISTING_SCRIPT, &[&path, "write"], Some(&updated)),
            &path,
        )
        .unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            "α\r\nchanged\r\nlast\r".as_bytes()
        );
        assert_eq!(
            local(
                EXISTING_SCRIPT,
                &[dir.0.to_str().unwrap(), "write"],
                Some("bad")
            )["exit_code"],
            23
        );
        assert_eq!(
            local(EXISTING_SCRIPT, &[&missing, "write"], Some("bad"))["exit_code"],
            21
        );
        assert!(!std::path::Path::new(&missing).exists());
    }
    #[test]
    fn directory_depth_hidden_pruning_and_glob_enumeration() {
        let dir = Directory::new();
        for name in ["visible/deeper", ".config", ".git"] {
            fs::create_dir_all(dir.path(name)).unwrap();
        }
        for name in [
            "top.txt",
            "visible/child.txt",
            "visible/deeper/too-deep.txt",
            ".secret",
            ".config/settings",
            ".git/config",
        ] {
            fs::write(dir.path(name), "").unwrap();
        }
        let path = dir.0.to_str().unwrap();
        let result = local(VIEW_SCRIPT, &[path, "full", "", "", ""], None);
        let response = view_response(&result, "host", path, "full", 1, None).unwrap();
        let entries = response["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 4);
        assert!(
            entries
                .iter()
                .any(|v| v["path"] == dir.path("visible/deeper") && v["type"] == "directory")
        );
        assert!(
            !entries
                .iter()
                .any(|v| v["path"].as_str().unwrap().contains(".secret")
                    || v["path"].as_str().unwrap().contains(".config"))
        );
        let result = local(GLOB_SCRIPT, &[path], None);
        assert_eq!(result["exit_code"], 0);
        assert!(stdout(&result).contains("./.config/settings\n"));
        assert!(!stdout(&result).contains(".git"));
        assert!(stdout(&result).contains("./visible/deeper/too-deep.txt\n"));
        assert_eq!(
            local(GLOB_SCRIPT, &[&dir.path("top.txt")], None)["exit_code"],
            26
        );
        assert_eq!(
            local(GLOB_SCRIPT, &[&dir.path("missing")], None)["exit_code"],
            21
        );
    }
    #[test]
    fn glob_segment_dot_brace_and_regex_semantics() {
        for (pattern, candidates) in [
            ("*.py", vec!["foo.py"]),
            (".*", vec![".env"]),
            ("a?c.txt", vec!["abc.txt"]),
            ("**/*.py", vec!["foo.py", "sub/foo.py", "a/b/c.py"]),
            ("src/**/*.ts", vec!["src/foo.ts", "src/a/b/foo.ts"]),
            ("src/**", vec!["src/foo.txt", "src/a/b.txt", "src/"]),
            ("**", vec!["foo.txt", "a/b/c.txt", ""]),
            ("**/foo/**", vec!["a/foo/b/c.txt", "foo/b.txt"]),
            ("*.{ts,tsx}", vec!["foo.ts", "foo.tsx"]),
            ("{src,lib}/*.ts", vec!["src/a.ts", "lib/a.ts"]),
            ("[abc].txt", vec!["a.txt"]),
            ("[!abc].txt", vec!["d.txt"]),
            ("[^abc].txt", vec!["d.txt"]),
            ("a.b.txt", vec!["a.b.txt"]),
            ("[a&&b]", vec!["a", "b", "&"]),
            ("[[]", vec!["["]),
            ("[]]", vec!["]"]),
            ("[!]]", vec!["a"]),
            ("file[", vec!["file["]),
            (
                "**/.config/*",
                vec![".config/settings", "sub/.config/settings"],
            ),
            ("**/**/a", vec!["a", "b/c/a"]),
            ("?.txt", vec!["α.txt"]),
        ] {
            let mut glob = Glob::new(pattern).unwrap();
            for candidate in candidates {
                assert!(
                    glob.matches(candidate),
                    "{pattern:?} must match {candidate:?}"
                );
            }
        }
        for (pattern, candidate) in [
            ("*.py", "sub/foo.py"),
            ("*", ".env"),
            ("*.py", ".hidden.py"),
            ("a?c.txt", "ac.txt"),
            ("a?c.txt", "abbc.txt"),
            ("**/*.py", ".hidden/foo.py"),
            ("**/*.py", "sub/.foo.py"),
            ("src/**/*.ts", "src/.git/config.ts"),
            ("src/**", "src"),
            ("src/**", "other/foo.txt"),
            ("**", ".git/config"),
            ("**/foo/**", "foo"),
            ("*.{ts,tsx}", "foo.js"),
            ("[abc].txt", "d.txt"),
            ("[!abc].txt", "a.txt"),
            ("a.b.txt", "aXb.txt"),
            ("[.]env", ".env"),
            ("**/**", "a/.hidden"),
        ] {
            assert!(
                !Glob::new(pattern).unwrap().matches(candidate),
                "{pattern:?} must not match {candidate:?}"
            );
        }
        assert_eq!(
            expand_braces("{a,{b,c}}.{x,y}"),
            vec!["a.x", "a.y", "b.x", "b.y", "a.x", "a.y", "c.x", "c.y"]
        );
    }
    #[test]
    fn grep_regex_context_binary_and_vcs_behavior() {
        let dir = Directory::new();
        fs::create_dir(dir.path(".git")).unwrap();
        fs::write(dir.path("a:b-c.txt"), "before\r\n-Needle 123\r\nafter\r\n").unwrap();
        fs::write(dir.path("zero.txt"), "no match\n").unwrap();
        fs::write(dir.path("binary.txt"), b"-Needle 123\0data\n").unwrap();
        fs::write(dir.path(".git/config"), "-Needle 123\n").unwrap();
        let path = dir.0.to_str().unwrap();
        let result = local(
            GREP_SCRIPT,
            &[
                path,
                "-n",
                "-i",
                "-C",
                "1",
                "--include",
                "*.txt",
                "--",
                "-needle [0-9]+",
            ],
            None,
        );
        assert_eq!(result["exit_code"], 0, "{result}");
        let entries: Vec<_> = stdout(&result).split('\n').filter_map(grep_line).collect();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0]["is_context"], true);
        assert_eq!(entries[1]["line_number"], 2);
        assert_eq!(entries[1]["line"], "-Needle 123\r");
        assert!(entries[1].get("is_context").is_none());
        let none = local(GREP_SCRIPT, &[path, "-l", "--", "missing-pattern"], None);
        assert_eq!(none["exit_code"], 1);
        assert_eq!(none["stdout"], "");
        assert_eq!(
            local(GREP_SCRIPT, &[path, "-l", "--", "["], None)["exit_code"],
            2
        );
        assert_eq!(
            local(GREP_SCRIPT, &[&dir.path("missing"), "-l", "--", "x"], None)["exit_code"],
            21
        );
        assert_eq!(grep_line("path-3:x:12:match").unwrap()["path"], "path-3:x");
        assert_eq!(
            grep_line("path:3-x-12-context").unwrap()["path"],
            "path:3-x"
        );
        assert!(grep_line("--").is_none());
        let files = local(GREP_SCRIPT, &[path, "-l", "-i", "--", "needle"], None);
        assert_eq!(stdout(&files), format!("{}\n", dir.path("a:b-c.txt")));
        let counts = local(GREP_SCRIPT, &[path, "-c", "-i", "--", "needle"], None);
        assert!(stdout(&counts).contains(&format!("{}:1\n", dir.path("a:b-c.txt"))));
        assert!(stdout(&counts).contains(&format!("{}:0\n", dir.path("zero.txt"))));
        assert!(!stdout(&counts).contains(".git/config"));
    }
    #[test]
    fn validation_error_mapping_and_quoting() {
        assert_eq!(shield("-file"), "./-file");
        for args in [
            json!({"view_range":[0,1]}),
            json!({"view_range":[2,1]}),
            json!({"view_range":[true,2]}),
            json!({"view_range":[null,2]}),
            json!({"view_range":[1]}),
            json!({"view_range":"1,2"}),
        ] {
            assert_eq!(view_range(&args).unwrap_err().kind, "validation_error");
        }
        assert_eq!(
            view_range(&json!({"view_range":[1,-1]})).unwrap(),
            Some((1, -1))
        );
        assert!(timeout(&json!({"timeout":0})).is_err());
        assert!(head_limit(&json!({"head_limit":0})).is_err());
        assert!(content(&json!({"content":"\0"}), "content").is_err());
        for (code, message) in [
            (21, "does not exist"),
            (22, "not a regular file"),
            (23, "is a directory"),
            (24, "already exists"),
            (25, "Parent directory"),
            (26, "not a directory"),
        ] {
            let error = check_file(
                &json!({"exit_code":code,"timed_out":false,"stderr":""}),
                "p",
            )
            .unwrap_err();
            assert_eq!(error.kind, "remote_file_error");
            assert!(error.message.contains(message));
        }
        assert!(
            check_file(&json!({"exit_code":0,"timed_out":true}), "p")
                .unwrap_err()
                .message
                .contains("timed out")
        );
        assert!(
            check_file(&json!({"exit_code":255,"stderr":"  ssh failure\n"}), "p")
                .unwrap_err()
                .message
                .ends_with("ssh failure")
        );
        assert!(
            view_response(
                &json!({"exit_code":0,"stdout":"bad"}),
                "h",
                "p",
                "auto",
                1,
                None
            )
            .is_err()
        );
        let result = local(
            "printf '%s\\0' \"$1\"",
            &["a' ; $(touch should-not-exist)\n ", "-flag"],
            None,
        );
        assert_eq!(result["exit_code"], 0);
        assert_eq!(result["stdout"], "a' ; $(touch should-not-exist)\n \0");
    }

    #[test]
    fn dash_paths_and_nonregular_files_keep_script_exit_boundaries() {
        let dir = Directory::new();
        let directory = dir.0.to_str().unwrap();
        let create = format!("cd \"$1\"\nshift\n{CREATE_SCRIPT}");
        assert_eq!(
            local(&create, &[directory, &shield("-file")], Some("one\ntwo\n"))["exit_code"],
            0
        );
        let view = format!("cd \"$1\"\nshift\n{VIEW_SCRIPT}");
        let result = local(
            &view,
            &[directory, &shield("-file"), "range", "2", "2", ""],
            None,
        );
        assert_eq!(
            view_response(&result, "h", "-file", "range", 1, Some((2, 2))).unwrap()["content"],
            "two\n"
        );
        let fifo = dir.path("fifo");
        let c_path = std::ffi::CString::new(fifo.as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        assert_eq!(
            local(VIEW_SCRIPT, &[&fifo, "full", "", "", ""], None)["exit_code"],
            22
        );
        assert_eq!(
            local(EXISTING_SCRIPT, &[&fifo, "read"], None)["exit_code"],
            22
        );
        assert_eq!(
            local(EXISTING_SCRIPT, &[&fifo, "write"], Some("never write"))["exit_code"],
            22
        );
    }
}
