use std::io;
use std::fs;
use std::env;
use std::path::{Path, PathBuf};
use std::fs::File;
use std::mem;
use std::ffi::OsStr;
use std::collections::HashSet;

use clap::{App, Arg, ArgMatches};
use hyper::method::Method;
use mime;
use multipart::client::Multipart;
use serde_json;
use walkdir::{WalkDir, Iter as WalkDirIter};
use zip;

use CliResult;
use utils::{TempFile, get_org_and_project, get_sha1_checksum};
use macho::is_macho_file;
use commands::Config;

const BATCH_SIZE : usize = 15;


#[derive(Debug, Deserialize)]
struct DSymFile {
    uuid: String,
    #[serde(rename="objectName")]
    object_name: String,
    #[serde(rename="cpuName")]
    cpu_name: String,
}

#[derive(Deserialize)]
struct MissingChecksumsResponse {
    missing: HashSet<String>,
}

struct LocalFile {
    path: PathBuf,
    arc_name: String,
    checksum: String,
}

struct BatchIter {
    path: PathBuf,
    wd_iter: WalkDirIter,
    batch: Vec<LocalFile>,
}

impl BatchIter {
    pub fn new<P: AsRef<Path>>(path: P) -> BatchIter {
        BatchIter {
            path: path.as_ref().to_path_buf(),
            wd_iter: WalkDir::new(&path).into_iter(),
            batch: vec![],
        }
    }
}

impl Iterator for BatchIter {
    type Item = CliResult<Vec<LocalFile>>;

    fn next(&mut self) -> Option<CliResult<Vec<LocalFile>>> {
        loop {
            if let Some(dent_res) = self.wd_iter.next() {
                let dent = iter_try!(dent_res);
                let md = iter_try!(dent.metadata());
                if md.is_file() && is_macho_file(dent.path()) {
                    let name = Path::new("DebugSymbols")
                        .join(dent.path().strip_prefix(&self.path).unwrap());
                    println!("  {}", name.display());
                    self.batch.push(LocalFile {
                        path: dent.path().to_path_buf(),
                        arc_name: name.to_string_lossy().into_owned(),
                        checksum: iter_try!(get_sha1_checksum(dent.path())),
                    });
                    if self.batch.len() > BATCH_SIZE {
                        break;
                    }
                }
            } else {
                break;
            }
        }
        if self.batch.len() == 0 {
            None
        } else {
            Some(Ok(mem::replace(&mut self.batch, vec![])))
        }
    }
}

fn find_missing_files(config: &Config, files: Vec<LocalFile>, api_path: &str)
    -> CliResult<Vec<LocalFile>>
{
    let mut url = format!("{}unknown/?", api_path);
    for (idx, ref file) in files.iter().enumerate() {
        if idx > 0 {
            url.push('&');
        }
        url.push_str("checksums=");
        url.push_str(&file.checksum);
    }
    let mut resp = config.api_request(Method::Get, &url)?;

    // This happens if the sentry installation we're connecting to does not
    // have that endpoint.  In that case just continue.  Any other HTTP
    // failure is ignored here too which is okay since we will try to upload
    // next step anyways.
    if !resp.status.is_success() {
        return Ok(files);
    }

    let state : MissingChecksumsResponse = serde_json::from_reader(&mut resp)?;
    let mut rv = vec![];
    for file in files.into_iter() {
        if state.missing.contains(&file.checksum) {
            rv.push(file)
        }
    }
    Ok(rv)
}

fn zip_up(files: &[LocalFile]) -> CliResult<TempFile> {
    println!("  Uploading a batch of missing files ...");
    let tf = TempFile::new()?;
    let mut zip = zip::ZipWriter::new(tf.open());
    for ref file in files {
        println!("    {}", file.arc_name);
        zip.start_file(file.arc_name.clone(),
            zip::CompressionMethod::Deflated)?;
        io::copy(&mut File::open(file.path.clone())?, &mut zip)?;
    }
    Ok(tf)
}

fn upload_dsyms(files: &[LocalFile], config: &Config,
                api_path: &str) -> CliResult<Vec<DSymFile>> {
    let tf = zip_up(files)?;
    let req = config.prepare_api_request(Method::Post, api_path)?;
    let mut mp = Multipart::from_request_sized(req)?;
    try!(mp.write_stream("file", &mut tf.open(), Some("archive.zip"),
        "application/zip".parse::<mime::Mime>().ok()));
    let mut resp = mp.send()?;
    if !resp.status.is_success() {
        fail!(resp);
    }
    Ok(serde_json::from_reader(&mut resp)?)
}

fn get_paths_from_env() -> CliResult<Vec<PathBuf>> {
    let mut rv = vec![];
    if let Some(base_path) = env::var_os("DWARF_DSYM_FOLDER_PATH") {
        for entry in fs::read_dir(base_path)? {
            let entry = entry?;
            if entry.path().extension() == Some(OsStr::new("dSYM")) &&
                fs::metadata(entry.path())?.is_dir() {
                rv.push(entry.path().to_path_buf());
            }
        }
    }
    Ok(rv)
}


pub fn make_app<'a, 'b: 'a>(app: App<'a, 'b>) -> App<'a, 'b>
{
    app
        .about("uploads debug symbols to a project")
        .arg(Arg::with_name("org")
             .value_name("ORG")
             .long("org")
             .short("o")
             .help("The organization slug"))
        .arg(Arg::with_name("project")
             .value_name("PROJECT")
             .long("project")
             .short("p")
             .help("The project slug"))
        .arg(Arg::with_name("global")
             .long("global")
             .short("g")
             .help("Uploads the dsyms globally. This can only be done \
                    with super admin access for the Sentry installation"))
        .arg(Arg::with_name("paths")
             .value_name("PATH")
             .help("The path to the debug symbols")
             .multiple(true)
             .index(1))
}

pub fn execute<'a>(matches: &ArgMatches<'a>, config: &Config) -> CliResult<()> {
    let paths = match matches.values_of("paths") {
        Some(paths) => paths.map(|x| PathBuf::from(x)).collect(),
        None => get_paths_from_env()?,
    };
    let api_path = if matches.is_present("global") {
        "/system/global-dsyms/".to_owned()
    } else {
        let (org, project) = get_org_and_project(matches)?;
        format!("/projects/{}/{}/files/dsyms/", org, project)
    };

    println!("Uploading symbols");
    if paths.len() == 0 {
        println!("Warning: no paths were provided.");
    }

    for path in paths {
        println!("Finding symbols in {}...", path.display());
        for batch_res in BatchIter::new(path) {
            let missing = find_missing_files(config, batch_res?, &api_path)?;
            if missing.len() == 0 {
                continue;
            }
            println!("Detected missing files");
            let rv = upload_dsyms(&missing, config, &api_path)?;
            if rv.len() > 0 {
                println!("  Accepted debug symbols:");
                for df in rv {
                    println!("    {} ({}; {})", df.uuid, df.object_name, df.cpu_name);
                }
            }
        }
    }

    Ok(())
}