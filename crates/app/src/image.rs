//! `roomy image`: installation images roomy finds and downloads for the user.

use crate::{EXIT_PROBLEMS, usage_error};
use roomy_core::env::{self, Env};
use roomy_core::images::download::{Download, Progress};
use roomy_core::images::{self, Curl, DISTROS, Distro, Group, Image, Remembering, resolve_all, size_words};
use roomy_core::{Result, err};
use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: roomy image list [<system>] | roomy image get <system> [<edition>]";

pub fn run(args: &[String]) -> ExitCode {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["list"] => {
            list();
            ExitCode::SUCCESS
        }
        ["list", system] => show(system),
        ["get", system] => get(system, None),
        ["get", system, edition] => get(system, Some(edition)),
        ["check"] => check(),
        _ => usage_error(USAGE),
    }
}

fn fail(message: impl std::fmt::Display) -> ExitCode {
    eprintln!("roomy: {message}");
    ExitCode::from(EXIT_PROBLEMS)
}

fn find(system: &str) -> Result<&'static Distro> {
    images::distro(system).ok_or_else(|| {
        err!("roomy does not know a system called '{system}' (see: {} image list)", env::roomy_command())
    })
}

fn fetcher() -> Result<Remembering<Curl>> {
    Ok(Remembering::new(Curl::new(&Env::system())?))
}

/// How an image is described in one line: version, size and file name.
fn describe(image: &Image) -> String {
    let size = image.size.map(size_words).unwrap_or_else(|| "size unknown".into());
    let version = if image.version.is_empty() { String::new() } else { format!("{}, ", image.version) };
    format!("{version}{size}, {}", image.file_name)
}

fn list() {
    for group in Group::ALL {
        println!("{}:", group.title());
        for distro in DISTROS.iter().filter(|distro| distro.group == group) {
            println!("  {:<13} {}: {}", distro.id, distro.name, distro.tagline);
        }
        println!();
    }
    println!("'{} image list <system>' shows a system's editions and their current versions.", env::roomy_command());
}

fn show(system: &str) -> ExitCode {
    let (distro, fetch) = match find(system).and_then(|distro| Ok((distro, fetcher()?))) {
        Ok(found) => found,
        Err(e) => return fail(e),
    };
    println!("{}: {}\n{}\n", distro.name, distro.tagline, distro.site);
    let mut section = "";
    for (edition, found) in resolve_all(&fetch, distro.editions) {
        if edition.section != section {
            section = edition.section;
            println!("{section}:");
        }
        let about = match (edition.website_only(), found) {
            (Some(page), _) => format!("download it from {page}"),
            (None, Ok(image)) => describe(&image),
            (None, Err(e)) => format!("not available right now: {e}"),
        };
        println!("  {:<17} {}: {about}", edition.slug, edition.name);
    }
    ExitCode::SUCCESS
}

fn get(system: &str, edition: Option<&str>) -> ExitCode {
    let outcome = (|| {
        let distro = find(system)?;
        let edition = match edition {
            Some(slug) => distro.edition(slug).ok_or_else(|| {
                err!(
                    "{} has no edition called '{slug}' (see: {} image list {system})",
                    distro.name,
                    env::roomy_command()
                )
            })?,
            None => &distro.editions[0],
        };
        if let Some(page) = edition.website_only() {
            return Err(err!("roomy cannot download {} {} itself; get it from {page}", distro.name, edition.name));
        }
        let image = images::resolve(&fetcher()?, &edition.source)?;
        let dir = env::downloads_dir()?;
        println!("Downloading {} {} ({}) to {}", distro.name, edition.name, describe(&image), dir.display());
        Download::start(&Env::system(), &image, &dir)
    })();
    let download = match outcome {
        Ok(download) => download,
        Err(e) => return fail(e),
    };
    let live = std::io::stdout().is_terminal();
    let mut shown = Instant::now() - Duration::from_secs(1);
    loop {
        let progress = download.progress();
        if live && shown.elapsed() >= Duration::from_millis(500) {
            shown = Instant::now();
            let line = match &progress {
                Progress::Fetching { done, total: Some(total) } if *total > 0 => {
                    format!("{} of {} ({}%)", size_words(*done), size_words(*total), done * 100 / total)
                }
                Progress::Fetching { done, .. } => size_words(*done),
                Progress::Checking => "Checking…".into(),
                _ => String::new(),
            };
            print!("\r\x1b[2K{line}");
            let _ = std::io::stdout().flush();
        }
        if progress.finished() {
            if live {
                print!("\r\x1b[2K");
            }
            return match progress {
                Progress::Done(path) => {
                    println!("Saved and checked: {}", path.display());
                    ExitCode::SUCCESS
                }
                Progress::Failed(why) => fail(why),
                _ => fail("the download was stopped; run the same command again to carry on"),
            };
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Looks up every edition and asks for each image, to notice early when a project changes how
/// it publishes. For roomy's own regular checks.
fn check() -> ExitCode {
    let fetch = match fetcher() {
        Ok(fetch) => fetch,
        Err(e) => return fail(e),
    };
    let mut broken = 0;
    for distro in DISTROS {
        for (edition, found) in resolve_all(&fetch, distro.editions) {
            let name = format!("{}/{}", distro.id, edition.slug);
            match (edition.website_only(), found) {
                (Some(page), _) => println!("site  {name:<28} {page}"),
                (None, Ok(image)) if image.size.is_some() => println!("ok    {name:<28} {}", describe(&image)),
                (None, Ok(image)) => {
                    broken += 1;
                    println!("FAIL  {name:<28} {} does not answer", image.url);
                }
                (None, Err(e)) => {
                    broken += 1;
                    println!("FAIL  {name:<28} {e}");
                }
            }
        }
    }
    if broken > 0 { fail(format!("{broken} editions cannot be found")) } else { ExitCode::SUCCESS }
}
