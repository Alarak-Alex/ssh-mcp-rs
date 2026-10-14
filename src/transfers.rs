// Adapted from slepp/ssh-mcp (MIT); see LICENSE for original attribution.
use crate::{Error, Result, core};
use serde_json::{Value, json};
use std::path::Path;

fn local_path(raw: &str) -> String {
    let expanded = core::expand(raw);
    // A colon or leading dash in a local filename must not become a remote path or flag.
    if Path::new(&expanded).is_absolute()
        || expanded.starts_with("./")
        || expanded.starts_with("../")
    {
        expanded
    } else {
        format!("./{expanded}")
    }
}
fn source(raw: &str, recursive: bool) -> Result<String> {
    let expanded = local_path(raw);
    let path = Path::new(&expanded);
    if !path.exists() {
        return Err(Error::validation(format!(
            "Local source path does not exist: {expanded}"
        )));
    }
    if path.is_dir() && !recursive {
        return Err(Error::validation(format!(
            "Local source path is a directory and requires 'recursive=true': {expanded}"
        )));
    }
    Ok(expanded)
}
fn destination(raw: &str, require_directory: bool) -> Result<String> {
    let expanded = local_path(raw);
    let path = Path::new(&expanded);
    if require_directory {
        if !path.is_dir() {
            return Err(Error::validation(format!(
                "Local destination must be an existing directory when copying multiple sources: {expanded}"
            )));
        }
    } else {
        let parent = if path.is_dir() {
            path
        } else {
            path.parent().unwrap_or(Path::new("."))
        };
        if !parent.is_dir() {
            return Err(Error::validation(format!(
                "Parent directory for local destination does not exist: {}",
                parent.display()
            )));
        }
    }
    Ok(expanded)
}
fn rsync_remote(target: &str, path: &str) -> String {
    format!("{target}:{}", core::quote(path))
}
fn scp_remote(target: &str, path: &str) -> String {
    // Contemporary OpenSSH SCP uses SFTP: quoting here would become literal filename bytes.
    format!("{target}:{path}")
}
fn direction(args: &Value) -> Result<&str> {
    let dir = core::req_str(args, "direction")?;
    if !["upload", "download"].contains(&dir) {
        return Err(Error::validation(
            "'direction' must be one of: upload, download.",
        ));
    }
    Ok(dir)
}
fn check_rsync_args(args: &[String]) -> Result<()> {
    let mut need_operand = false;
    for arg in args {
        if need_operand {
            need_operand = false;
            continue;
        }
        let flag = arg.split('=').next().unwrap_or(arg).to_ascii_lowercase();
        if (flag.len() > 2
            && ["--rsh", "--rsync-path"]
                .iter()
                .any(|blocked| blocked.starts_with(&flag)))
            || (arg.starts_with('-') && !arg.starts_with("--") && arg[1..].contains('e'))
        {
            return Err(Error::validation(format!(
                "Blocked rsync transport/command flag '{arg}' in extra_rsync_args."
            )));
        }
        if arg == "--" || !arg.starts_with('-') {
            return Err(Error::validation(
                "extra_rsync_args must contain options, not source/destination operands.",
            ));
        }
        // These standard rsync options accept a separate value; other values can use --flag=value.
        need_operand = [
            "--exclude",
            "--include",
            "--exclude-from",
            "--include-from",
            "--filter",
            "--files-from",
            "--bwlimit",
            "--timeout",
            "--contimeout",
            "--max-size",
            "--min-size",
            "--max-delete",
            "--chmod",
            "--log-file",
            "--log-file-format",
            "--out-format",
            "--backup-dir",
            "--suffix",
            "--temp-dir",
            "--partial-dir",
            "--compare-dest",
            "--copy-dest",
            "--link-dest",
            "--block-size",
            "--protocol",
            "--iconv",
            "--address",
            "--port",
            "-f",
            "-B",
            "-T",
        ]
        .contains(&arg.as_str());
    }
    if need_operand {
        return Err(Error::validation(
            "A value is missing from extra_rsync_args.",
        ));
    }
    Ok(())
}
fn scp_argv(args: &Value, connection: &core::Connection) -> Result<Vec<String>> {
    let dir = direction(args)?;
    let sources = core::strings(args, "sources")?;
    if sources.is_empty() {
        return Err(Error::validation(
            "'sources' must contain at least one item.",
        ));
    }
    let dest = core::raw_str(args, "destination")?;
    let recursive = core::boolean(args, "recursive", false)?;
    let preserve_times = core::boolean(args, "preserve_times", false)?;
    let mut argv = connection.transport(&core::binary("SSH_MCP_SCP_BIN", "scp")?, "-P", None);
    if recursive {
        argv.push("-r".to_owned());
    }
    if preserve_times {
        argv.push("-p".to_owned());
    }
    argv.push("--".to_owned());
    if dir == "upload" {
        for s in &sources {
            argv.push(source(s, recursive)?);
        }
        argv.push(scp_remote(&connection.target, dest));
    } else {
        for s in &sources {
            argv.push(scp_remote(&connection.target, s));
        }
        argv.push(destination(dest, sources.len() > 1)?);
    }
    Ok(argv)
}
fn sync_argv(args: &Value, connection: &core::Connection) -> Result<Vec<String>> {
    let dir = direction(args)?;
    let src = core::raw_str(args, "source")?;
    let dest = core::raw_str(args, "destination")?;
    let delete = core::boolean(args, "delete", false)?;
    let compress = core::boolean(args, "compress", true)?;
    let dry_run = core::boolean(args, "dry_run", false)?;
    let excludes = core::strings(args, "exclude")?;
    let extra = core::strings(args, "extra_rsync_args")?;
    check_rsync_args(&extra)?;
    let transport = connection.transport(&core::binary("SSH_MCP_SSH_BIN", "ssh")?, "-p", None);
    let mut argv = vec![core::binary("SSH_MCP_RSYNC_BIN", "rsync")?, "-a".to_owned()];
    if compress {
        argv.push("-z".to_owned());
    }
    if delete {
        argv.push("--delete".to_owned());
    }
    if dry_run {
        argv.push("--dry-run".to_owned());
    }
    for pattern in excludes {
        argv.extend(["--exclude".to_owned(), pattern]);
    }
    argv.extend(extra);
    argv.extend([
        "-e".to_owned(),
        transport
            .iter()
            .map(|s| core::quote(s))
            .collect::<Vec<_>>()
            .join(" "),
        "--".to_owned(),
    ]);
    if dir == "upload" {
        argv.extend([source(src, true)?, rsync_remote(&connection.target, dest)]);
    } else {
        argv.extend([
            rsync_remote(&connection.target, src),
            destination(dest, false)?,
        ]);
    }
    Ok(argv)
}
pub fn call(tool: &str, args: &Value) -> Result<Value> {
    let connection = core::Connection::from_args(args)?;
    let timeout = core::timeout(args)?;
    let argv = match tool {
        "ssh_scp" => scp_argv(args, &connection)?,
        "ssh_sync" => sync_argv(args, &connection)?,
        _ => return Err(Error::validation(format!("Unknown transfer tool: {tool}"))),
    };
    let mut result = core::run(&argv, None, timeout, false)?;
    result["target"] = json!(connection.target);
    result["direction"] = json!(direction(args)?);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transfer_argument_security_and_paths() {
        for args in [
            vec!["--rsh=touch /tmp/injected"],
            vec!["-e", "evil"],
            vec!["-avevil"],
            vec!["--rsync-path=evil"],
            vec!["--", "-evil"],
            vec!["other-host:path"],
        ] {
            assert!(
                check_rsync_args(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_err()
            );
        }
        assert!(
            check_rsync_args(&[
                "--exclude".to_owned(),
                "cache".to_owned(),
                "--size-only".to_owned()
            ])
            .is_ok()
        );
        assert_eq!(local_path("-evil:source/"), "./-evil:source/");
        assert_eq!(
            scp_remote("user@host", "copy space's;$(echo bad).txt"),
            "user@host:copy space's;$(echo bad).txt"
        );
        assert!(check_rsync_args(&["--rsync-pa=evil".to_owned()]).is_err());
        assert_eq!(
            rsync_remote("user@host", "odd';$(echo bad)/file"),
            "user@host:'odd'\"'\"';$(echo bad)/file'"
        );
        assert!(source("/a/nonexistent/ssh-mcp-path", false).is_err());
        assert!(destination("/a/nonexistent/ssh-mcp-path", false).is_err());
        assert!(direction(&json!({"direction":"sideways"})).is_err());
        assert!(direction(&json!({"direction":"upload"})).is_ok());
    }
}
