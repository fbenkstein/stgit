//! `stg import` implementation.

use std::{
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use bstr::{ByteSlice, ByteVec};
use clap::{Arg, ArgGroup};

use crate::{
    color::get_color_stdout,
    patchedit,
    patchname::PatchName,
    repo::RepositoryExtended,
    signature::{self, SignatureExtended},
    stack::{Stack, StackStateAccess},
    stupid::{Stupid, StupidContext},
};

use super::StGitCommand;

pub(super) fn get_command() -> (&'static str, StGitCommand) {
    ("import", StGitCommand { make, run })
}

fn make() -> clap::Command<'static> {
    let app = clap::Command::new("import")
        .about("Import patches to stack")
        .long_about(
            "Import patches from various sources to the stack.\n\
             \n\
             The simplest usage is to import a diff/patch file into the stack from a \
             local file. By default, the file name is used as the patch name, but this \
             can be overridden with --name. The patch can either be a normal file with \
             the description at the top, or it can have standard mail format. The \
             \"Subject\", \"From\", and \"Date\" headers will be used for the imported \
             patch's author details.\n\
             \n\
             Patches may also be imported from a mail file (-m/--mail), an mbox \
             (-M/--mbox), or a series (-s/--series). Furthermore, the -u/--url option \
             allows the patches source to be fetched from a url instead of from a \
             local file.\n\
             \n\
             If a patch does not apply cleanly, the failed diff is written to a \
             .stgit-failed.patch file and an empty patch is added to the stack.\n\
             \n\
             The patch description must be separated from the diff with a \"---\" line.",
        )
        .override_usage(if cfg!(feature = "import-url") {
            "stg import [OPTIONS] <diff-path>\n    \
             stg import [OPTIONS] -m [<mail-path>|<Maildir-path>]\n    \
             stg import [OPTIONS] -M [<mbox-path>]\n    \
             stg import [OPTIONS] -s [<series-path>]\n    \
             stg import [OPTIONS] -u <diff-url>\n    \
             stg import [OPTIONS] -u -m <mail-url>\n    \
             stg import [OPTIONS] -u -M <mbox-url>\n    \
             stg import [OPTIONS] -u -s <series-url>"
        } else {
            "stg import [OPTIONS] <diff-path>\n    \
             stg import [OPTIONS] -m [<mail-path>|<Maildir-path>]\n    \
             stg import [OPTIONS] -M [<mbox-path>]\n    \
             stg import [OPTIONS] -s [<series-path>]"
        })
        .arg(
            Arg::new("source")
                .help("Source of patches to import")
                .long_help(
                    "Source of patches to import. May be a path to a local file or a \
                     URL if the --url option is provided. The default is to read from \
                     stdin if no source argument is provided.",
                )
                .value_hint(clap::ValueHint::AnyPath),
        )
        .next_help_heading("SOURCE OPTIONS")
        .arg(
            Arg::new("mail")
                .long("mail")
                .short('m')
                .help("Import patch from an email file"),
        )
        .arg(
            Arg::new("mbox")
                .long("mbox")
                .short('M')
                .help("Import patch series from an mbox file"),
        )
        .arg(
            Arg::new("series")
                .long("series")
                .short('s')
                .help("Import patch series")
                .long_help("Import patch series from a series file are tar archive."),
        )
        .group(ArgGroup::new("whence").args(&["mail", "mbox", "series"]));

    let app = if cfg!(feature = "import-url") {
        app.arg(
            Arg::new("url")
                .long("url")
                .short('u')
                .help("Retrieve source from a url instead of local file")
                .requires("source"),
        )
    } else {
        app
    };

    let app = app
        .next_help_heading("IMPORT OPTIONS")
        .arg(
            Arg::new("name")
                .long("name")
                .short('n')
                .help("Use NAME as the patch name")
                .value_name("NAME"),
        )
        .arg(
            Arg::new("strip")
                .long("strip")
                .short('p')
                .help("Remove N leading components from diff paths (default 1)")
                .value_name("N")
                .validator(|s| {
                    s.parse::<usize>()
                        .map_err(|_| format!("'{s}' is not an unsigned integer"))
                }),
        )
        .arg(
            Arg::new("stripname")
                .long("stripname")
                .short('t')
                .help("Strip number and extension from patch name"),
        )
        .arg(
            Arg::new("context-lines")
                .short('C')
                .help("Ensure N lines of matching context for each change")
                .value_name("N")
                .validator(|s| {
                    s.parse::<usize>()
                        .map_err(|_| format!("'{s}' is not an unsigned integer"))
                }),
        )
        .arg(
            Arg::new("ignore")
                .long("ignore")
                .short('i')
                .help("Ignore the applied patches in the series"),
        )
        .arg(
            Arg::new("replace")
                .long("replace")
                .help("Replace the unapplied patches in the series"),
        )
        .arg(
            Arg::new("base")
                .long("base")
                .short('b')
                .help("Use BASE instead of HEAD for file importing")
                .value_name("COMMITTISH"),
        )
        .arg(
            Arg::new("reject")
                .long("reject")
                .help("Leave rejected hunks in \".rej\" files"),
        )
        .arg(
            Arg::new("keep-cr")
                .long("keep-cr")
                .help("Do not remove \"\\r\" from email lines ending with \"\\r\\n\""),
        )
        .arg(
            Arg::new("message-id")
                .long("message-id")
                .help("Create Message-Id trailer from Message-ID header")
                .long_help(
                    "Create Message-Id trailer in patch description based on the \
                    Message-ID email header. This option is applicable when importing \
                    with --mail or --mbox. This behavior may also be enabled via the \
                    \"stgit.import.message-id\" configuration option.",
                ),
        );
    patchedit::add_args(app, false, false)
}

fn run(matches: &clap::ArgMatches) -> Result<()> {
    let repo = git2::Repository::open_from_env()?;
    let stack = Stack::from_branch(&repo, None)?;

    let source_path = if matches.is_present("url") {
        None
    } else if let Some(path_str) = matches.value_of("source") {
        let path = Path::new(path_str);
        let abs_path = path.canonicalize()?;
        Some(abs_path)
    } else {
        None
    };

    std::env::set_current_dir(repo.workdir().unwrap())?;

    repo.check_index_and_worktree_clean()?;
    stack.check_head_top_mismatch()?;
    repo.stupid().update_index_refresh()?;

    if cfg!(feature = "import-url") && matches.is_present("url") {
        import_url(stack, matches)
    } else if matches.is_present("series") {
        import_series(stack, matches, source_path.as_deref())
    } else if matches.is_present("mail") || matches.is_present("mbox") {
        import_mail(stack, matches, source_path.as_deref())
    } else {
        import_file(stack, matches, source_path.as_deref(), None)?;
        Ok(())
    }
}

#[cfg(not(feature = "import-url"))]
fn import_url(_stack: Stack, _matches: &clap::ArgMatches) -> Result<()> {
    Err(anyhow!(
        "StGit not built with support for downloading imports"
    ))
}

#[cfg(feature = "import-url")]
fn import_url(stack: Stack, matches: &clap::ArgMatches) -> Result<()> {
    use std::io::Write;

    let url_str = matches
        .value_of("source")
        .expect("source url must be present");
    let mut handle = curl::easy::Easy::new();
    handle.url(url_str)?;
    let url_decoded = handle.url_decode(url_str);
    let filename = url_decoded
        .rsplit_str(b"/")
        .next()
        .and_then(|b| b.to_str().ok())
        .unwrap_or("patch");
    let download_dir = tempfile::tempdir()?;
    let download_path = download_dir.path().join(filename);
    let mut download_file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&download_path)?;
    let mut transfer = handle.transfer();
    transfer.write_function(|data| {
        download_file.write_all(data).unwrap();
        Ok(data.len())
    })?;

    match transfer.perform() {
        Ok(()) => {}
        Err(e) if e.is_url_malformed() && url_str.starts_with("file://") => {
            let source_path = Path::new(url_str.strip_prefix("file://").unwrap());
            std::fs::copy(source_path, &download_path)
                .with_context(|| format!("copying {url_str}"))?;
        }
        e @ Err(_) => e?,
    }

    if matches.is_present("series") {
        import_series(stack, matches, Some(download_path.as_path()))
    } else if matches.is_present("mail") || matches.is_present("mbox") {
        import_mail(stack, matches, Some(download_path.as_path()))
    } else {
        import_file(stack, matches, Some(download_path.as_path()), None)?;
        Ok(())
    }
}

#[cfg(feature = "import-compressed")]
fn import_tgz_series(stack: Stack, matches: &clap::ArgMatches, source_path: &Path) -> Result<()> {
    let source_file = std::fs::File::open(&source_path)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(source_file));
    let temp_dir = tempfile::tempdir()?;
    archive.unpack(temp_dir.path())?;
    let series_path = find_series_path(temp_dir.path())?;
    return import_series(stack, matches, Some(series_path.as_path()));
}

#[cfg(feature = "import-compressed")]
fn import_tbz2_series(stack: Stack, matches: &clap::ArgMatches, source_path: &Path) -> Result<()> {
    let source_file = std::fs::File::open(&source_path)?;
    let mut archive = tar::Archive::new(bzip2::read::BzDecoder::new(source_file));
    let temp_dir = tempfile::tempdir()?;
    archive.unpack(temp_dir.path())?;
    let series_path = find_series_path(temp_dir.path())?;
    return import_series(stack, matches, Some(series_path.as_path()));
}

#[cfg(feature = "import-compressed")]
fn import_tar_series(stack: Stack, matches: &clap::ArgMatches, source_path: &Path) -> Result<()> {
    let source_file = std::fs::File::open(&source_path)?;
    let mut archive = tar::Archive::new(source_file);
    let temp_dir = tempfile::tempdir()?;
    archive.unpack(temp_dir.path())?;
    let series_path = find_series_path(temp_dir.path())?;
    return import_series(stack, matches, Some(series_path.as_path()));
}

#[cfg(not(feature = "import-compressed"))]
fn import_tgz_series(_: Stack, _: &clap::ArgMatches, _: &Path) -> Result<()> {
    Err(anyhow!(
        "StGit not built with support for compressed series"
    ))
}

#[cfg(not(feature = "import-compressed"))]
fn import_tbz2_series(_: Stack, _: &clap::ArgMatches, _: &Path) -> Result<()> {
    Err(anyhow!(
        "StGit not built with support for compressed series"
    ))
}

#[cfg(not(feature = "import-compressed"))]
fn import_tar_series(_: Stack, _: &clap::ArgMatches, _: &Path) -> Result<()> {
    Err(anyhow!(
        "StGit not built with support for compressed series"
    ))
}

fn import_series(
    stack: Stack,
    matches: &clap::ArgMatches,
    source_path: Option<&Path>,
) -> Result<()> {
    let series = if let Some(source_path) = source_path {
        if let Some(filename) = source_path.file_name() {
            let filename = filename.to_string_lossy().to_ascii_lowercase();
            if filename.ends_with(".tar.gz") || filename.ends_with(".tgz") {
                return import_tgz_series(stack, matches, source_path);
            } else if filename.ends_with(".tar.bz2") {
                return import_tbz2_series(stack, matches, source_path);
            } else if filename.ends_with(".tar") {
                return import_tar_series(stack, matches, source_path);
            }
        }
        std::fs::read(source_path)?
    } else {
        let stdin = std::io::stdin();
        let mut stdin = stdin.lock();
        let mut buf = Vec::new();
        stdin.read_to_end(&mut buf)?;
        buf
    };

    let mut stack = stack;

    for line in series.lines() {
        let line = line
            .find_char('#')
            .map(|pos| &line[..pos])
            .unwrap_or(line)
            .trim();
        if line.is_empty() {
            continue;
        }

        let mut fields = line.fields();
        let raw_patchname = fields
            .next()
            .expect("non-empty line must have first field")
            .to_os_str()
            .context("converting patch name from series to file name")?;
        let patch_path = source_path
            .map(|p| p.with_file_name(raw_patchname))
            .unwrap_or_else(|| PathBuf::from(raw_patchname));

        let strip_level = if let Some(extra) = fields.next() {
            if extra.starts_with_str("-p") {
                if extra == b"-p0" {
                    Some(0)
                } else {
                    return Err(anyhow!(
                        "Patch `{}` has unsupported strip level \"{}\"",
                        raw_patchname.to_string_lossy(),
                        extra.to_str_lossy()
                    ));
                }
            } else {
                None
            }
        } else {
            None
        };

        stack = import_file(stack, matches, Some(patch_path.as_path()), strip_level)?;
    }
    Ok(())
}

#[cfg(feature = "import-compressed")]
fn find_series_path(base: &Path) -> Result<PathBuf> {
    for entry in base.read_dir()? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if let Ok(path) = find_series_path(&entry.path()) {
                return Ok(path);
            }
        } else if file_type.is_file() && entry.file_name() == std::ffi::OsStr::new("series") {
            return Ok(entry.path());
        }
    }
    Err(anyhow!("Series file not found"))
}

fn use_message_id(matches: &clap::ArgMatches, config: &git2::Config) -> bool {
    matches.is_present("message-id") || config.get_bool("stgit.import.message-id").unwrap_or(false)
}

fn import_mail(stack: Stack, matches: &clap::ArgMatches, source_path: Option<&Path>) -> Result<()> {
    let out_dir = tempfile::tempdir()?;
    let missing_from_ok = matches.is_present("mail");
    let keep_cr = matches.is_present("keep-cr");
    let config = stack.repo.config()?;
    let message_id = use_message_id(matches, &config);
    let stupid = stack.repo.stupid();
    let num_patches = stupid.mailsplit(source_path, out_dir.path(), keep_cr, missing_from_ok)?;
    let mut stack = stack;
    for i in 1..num_patches + 1 {
        let patch_path = out_dir.path().join(format!("{i:04}"));
        let patch_file = std::fs::File::open(&patch_path)?;
        let (mailinfo, message, diff) = stupid.mailinfo(Some(patch_file), message_id)?;
        let headers = Headers::parse_mailinfo(&mailinfo).unwrap_or_default();
        stack = create_patch(stack, matches, None, headers, &message, &diff, None)?;
    }
    Ok(())
}

#[cfg(feature = "import-compressed")]
fn get_gz_mailinfo(
    stupid: &StupidContext,
    source_file: std::fs::File,
    message_id: bool,
) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let stream = flate2::read::GzDecoder::new(source_file);
    stupid.mailinfo_stream(stream, message_id)
}

#[cfg(feature = "import-compressed")]
fn get_bz2_mailinfo(
    stupid: &StupidContext,
    source_file: std::fs::File,
    message_id: bool,
) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let stream = bzip2::read::BzDecoder::new(source_file);
    stupid.mailinfo_stream(stream, message_id)
}

#[cfg(not(feature = "import-compressed"))]
fn get_gz_mailinfo(
    _: &StupidContext,
    _: std::fs::File,
    _: bool,
) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    Err(anyhow!(
        "StGit not built with support for compressed patches"
    ))
}

#[cfg(not(feature = "import-compressed"))]
fn get_bz2_mailinfo(
    _: &StupidContext,
    _: std::fs::File,
    _: bool,
) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    Err(anyhow!(
        "StGit not built with support for compressed patches"
    ))
}

fn import_file<'repo>(
    stack: Stack<'repo>,
    matches: &clap::ArgMatches,
    source_path: Option<&Path>,
    strip_level: Option<usize>,
) -> Result<Stack<'repo>> {
    let config = stack.repo.config()?;
    let message_id = use_message_id(matches, &config);
    let stupid = stack.repo.stupid();

    let (mailinfo, message, diff) = if let Some(source_path) = source_path {
        let source_file = std::fs::File::open(source_path)?;
        match source_path.extension().and_then(|s| s.to_str()) {
            Some("gz") => get_gz_mailinfo(&stupid, source_file, message_id),
            Some("bz2") => get_bz2_mailinfo(&stupid, source_file, message_id),
            _ => stupid.mailinfo(Some(source_file), message_id),
        }
    } else {
        stupid.mailinfo(None, message_id)
    }
    .or_else(|e| {
        if e.chain()
            .last()
            .unwrap()
            .to_string()
            .contains("error: empty patch")
        {
            Ok((vec![], vec![], vec![]))
        } else {
            Err(e)
        }
    })?;

    let (headers, message) = if let Some(headers) = Headers::parse_mailinfo(&mailinfo) {
        (headers, message)
    } else {
        Headers::parse_message(&message)?
    };

    create_patch(
        stack,
        matches,
        source_path,
        headers,
        &message,
        &diff,
        strip_level,
    )
}

fn create_patch<'repo>(
    stack: Stack<'repo>,
    matches: &clap::ArgMatches,
    source_path: Option<&Path>,
    headers: Headers,
    message: &[u8],
    diff: &[u8],
    strip_level: Option<usize>,
) -> Result<Stack<'repo>> {
    let Headers {
        patchname,
        author_name,
        author_email,
        author_date,
        subject,
    } = headers;

    let message = if let Some(mut subject) = subject {
        subject.push_str("\n\n");
        subject.push_str(&message.to_str_lossy());
        subject
    } else {
        message.to_str_lossy().to_string()
    };

    let patchname = if patchname.is_some() {
        patchname.as_deref()
    } else if let Some(name) = matches.value_of("name") {
        Some(name)
    } else if let Some(source_path) = source_path {
        source_path.file_name().and_then(|name| name.to_str())
    } else {
        None
    };

    let patchname = if matches.is_present("stripname") {
        patchname.map(stripname)
    } else {
        patchname
    };

    let config = stack.repo.config()?;
    let name_len_limit = PatchName::get_length_limit(&config);

    let patchname = if let Some(patchname) = patchname {
        PatchName::make(patchname, false, name_len_limit)
    } else {
        PatchName::make(&message, true, name_len_limit)
    };

    let opt_ignore = matches.is_present("ignore");
    let opt_replace = matches.is_present("replace");

    let patchname = if !opt_ignore && !opt_replace {
        let disallow_patchnames: Vec<&PatchName> = stack.all_patches().collect();
        patchname.uniquify(&[], &disallow_patchnames)
    } else if opt_ignore && stack.applied().contains(&patchname) {
        eprintln!("info: ignoring already applied patch `{patchname}`");
        return Ok(stack);
    } else {
        patchname
    };

    let author_date = author_date.and_then(|date| crate::signature::parse_time(&date).ok());
    let author = if let (Some(name), Some(email), Some(time)) =
        (author_name.as_ref(), author_email.as_ref(), author_date)
    {
        git2::Signature::new(name, email, &time)?
    } else if let (Some(name), Some(email)) = (author_name.as_ref(), author_email.as_ref()) {
        git2::Signature::now(name, email)?
    } else if let Some(time) = author_date {
        let default = git2::Signature::default_author(Some(&config))?;
        let name = default.name().expect("default author name is UTF-8");
        let email = default.email().expect("default author email is UTF-8");
        git2::Signature::new(name, email, &time)?
    } else {
        git2::Signature::default_author(Some(&config))?
    };

    let strip_level = strip_level.or_else(|| {
        matches
            .value_of("strip")
            .map(|s| s.parse::<usize>().expect("clap already validated"))
    });

    let stupid = stack.repo.stupid();
    stupid.apply_to_worktree_and_index(
        diff,
        matches.is_present("reject"),
        strip_level,
        matches
            .value_of("context-lines")
            .map(|s| s.parse::<usize>().unwrap()),
    )?;

    let tree_id = stupid.write_tree()?;

    let (new_patchname, commit_id) = match crate::patchedit::EditBuilder::default()
        .original_patchname(Some(&patchname))
        .override_tree_id(tree_id)
        .override_parent_id(stack.branch_head.id())
        .default_author(author)
        .default_message(message)
        .allow_implicit_edit(false)
        .allow_diff_edit(true)
        .allow_template_save(false)
        .edit(&stack, stack.repo, matches)?
    {
        patchedit::EditOutcome::TemplateSaved(_) => unreachable!(),
        patchedit::EditOutcome::Committed {
            patchname,
            commit_id,
        } => (patchname, commit_id),
    };

    stack
        .setup_transaction()
        .with_output_stream(get_color_stdout(matches))
        .use_index_and_worktree(false)
        .allow_conflicts(false)
        .transact(|trans| {
            if opt_replace && trans.unapplied().contains(&new_patchname) {
                trans.delete_patches(|pn| pn == &new_patchname)?;
            }
            trans.new_applied(&new_patchname, commit_id)
        })
        .execute(&format!("import: {new_patchname}"))
}

fn stripname(name: &str) -> &str {
    name.trim_start_matches(|c: char| c.is_ascii_digit() || c == '-')
        .rsplit_once(".diff")
        .map_or_else(
            || name.rsplit_once(".patch").map_or(name, |(name, _)| name),
            |(name, _)| name,
        )
}

#[derive(Default, Debug)]
struct Headers {
    patchname: Option<String>,
    author_name: Option<String>,
    author_email: Option<String>,
    author_date: Option<String>,
    subject: Option<String>,
}

impl Headers {
    fn parse_mailinfo(headers: &[u8]) -> Option<Headers> {
        let mut author_name = None;
        let mut author_email = None;
        let mut author_date = None;
        let mut subject = None;

        for line in headers.lines().filter(|line| !line.is_empty()) {
            let mut parts = line.splitn_str(2, b": ");
            let header = parts.next().expect("mailinfo header line has header");
            let value = parts.next().expect("mailinfo header line has value");
            if let Ok(value) = value.to_str() {
                let value = Some(value.to_string());
                match header {
                    b"Author" => author_name = value,
                    b"Email" => author_email = value,
                    b"Date" => author_date = value,
                    b"Subject" => subject = value,
                    _ => panic!(
                        "unexpected mailinfo header \"{}\" with value \"{}\"",
                        header.to_str_lossy(),
                        value.unwrap()
                    ),
                }
            }
        }

        if author_name.is_some()
            || author_email.is_some()
            || author_date.is_some()
            || subject.is_some()
        {
            Some(Headers {
                patchname: None,
                author_name,
                author_email,
                author_date,
                subject,
            })
        } else {
            None
        }
    }

    fn parse_message(message: &[u8]) -> Result<(Headers, Vec<u8>)> {
        let mut headers = Headers::default();
        let mut dedent = "";
        let mut split_message = Vec::with_capacity(message.len());
        let mut lines = message.lines_with_terminator();

        while let Some(line) = lines.next().map(|line| line.trim()) {
            if line.is_empty() {
                continue;
            }

            let parts: Vec<_> = line.splitn_str(2, b":").collect();
            if parts.len() == 2 {
                let header = parts[0];
                let value = parts[1].trim_start();
                if header.eq_ignore_ascii_case(b"patch") && !value.is_empty() {
                    headers.patchname = Some(
                        value
                            .to_str()
                            .map_err(|_| anyhow!("Patch name is not UTF-8"))
                            .context("parsing Patch header")?
                            .to_string(),
                    );
                    continue;
                } else if header.eq_ignore_ascii_case(b"from")
                    || header.eq_ignore_ascii_case(b"author")
                {
                    let (name, email) = value
                        .to_str()
                        .map_err(|_| anyhow!("From/Author is not UTF-8"))
                        .and_then(|s| signature::parse_name_email(s).map_err(|e| e))
                        .context("parsing From/Author header")?;
                    headers.author_name = Some(name.to_string());
                    headers.author_email = Some(email.to_string());
                    continue;
                } else if header.eq_ignore_ascii_case(b"date") {
                    headers.author_date = value.to_str().map(|s| s.to_string()).ok();
                    continue;
                }
            }

            if headers.subject.is_some() {
                break;
            }

            if line
                .strip_prefix(b"commit ")
                .map(|rest| rest.iter().all(|c| c.is_ascii_hexdigit()))
                .unwrap_or(false)
            {
                // Looks like this patch came from `git show`. Remaining message lines
                // need to be stripped of indentation.
                dedent = "    ";
                continue;
            }

            // Once the subject is determined, more contiguous headers may be parsed.
            headers.subject = Some(
                line.to_str()
                    .map_err(|_| anyhow!("Message is not UTF-8"))
                    .context("parsing patch message")?
                    .to_string(),
            );
        }

        for line in lines {
            split_message.push_str(line.strip_prefix(dedent.as_bytes()).unwrap_or(line));
        }

        Ok((headers, split_message))
    }
}
