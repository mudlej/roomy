//! Installation images roomy can fetch for the user: the distributions it knows, how to find each
//! edition's current image in what its project publishes, and downloading one into the user's
//! Downloads folder.
//!
//! No version is written down here. Each edition names where its project announces the current
//! image and its checksum, and that is read when the user looks. An image is only ever kept once
//! its checksum matches.

mod distros;
pub mod download;
pub mod sources;

pub use distros::DISTROS;

use crate::env::Env;
use crate::process;
use crate::{Result, err};
use sources::{checksum_list, file_name_of, folder_of, newest, newest_folder, newest_link, version_in};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

/// How distributions are grouped in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    FirstChoices,
    Popular,
    Tinkerers,
    Servers,
    Special,
}

impl Group {
    pub const ALL: [Group; 5] = [Group::FirstChoices, Group::Popular, Group::Tinkerers, Group::Servers, Group::Special];

    pub fn title(self) -> &'static str {
        match self {
            Group::FirstChoices => "Good first choices",
            Group::Popular => "Popular",
            Group::Tinkerers => "For tinkerers",
            Group::Servers => "Servers and work",
            Group::Special => "Special purpose",
        }
    }
}

#[derive(Debug)]
pub struct Distro {
    /// Used on the command line and for the logo and screenshot files.
    pub id: &'static str,
    pub name: &'static str,
    pub group: Group,
    /// One line under the name.
    pub tagline: &'static str,
    /// A few sentences on what it is like.
    pub about: &'static str,
    pub desktop: &'static str,
    /// The project's website.
    pub site: &'static str,
    pub editions: &'static [Edition],
}

impl Distro {
    /// The edition with this short name (as used on the command line).
    pub fn edition(&self, slug: &str) -> Option<&'static Edition> {
        self.editions.iter().find(|edition| edition.slug == slug)
    }
}

#[derive(Debug)]
pub struct Edition {
    /// The heading it is listed under, such as "Spins".
    pub section: &'static str,
    pub name: &'static str,
    /// Its short name on the command line.
    pub slug: &'static str,
    /// A few words on what sets it apart; may be empty.
    pub note: &'static str,
    /// The guest system (a [`crate::catalog::SYSTEMS`] id) a VM installed from it should be marked as.
    pub guest: &'static str,
    pub source: Source,
}

impl Edition {
    /// The page to get it from, when roomy cannot download it itself.
    pub fn website_only(&self) -> Option<&'static str> {
        match self.source {
            Source::Website { page } => Some(page),
            _ => None,
        }
    }
}

/// Where a project announces an edition's current image.
#[derive(Debug, Clone, Copy)]
pub enum Source {
    /// A checksum list: the newest file in it whose name fits `pattern` (see [`sources::fits`]),
    /// downloaded from next to the list, or from `fixed` when the project names its current
    /// image by a link that never changes.
    Sums { list: &'static str, pattern: &'static str, fixed: Option<&'static str> },
    /// The highest-numbered folder linked from `index`, holding the checksum list `list`.
    NewestFolder { index: &'static str, list: &'static str, pattern: &'static str },
    /// Ubuntu and its flavours: `list` with `{series}` standing for the current LTS release's code name.
    UbuntuLts { list: &'static str, pattern: &'static str },
    /// `fedoraproject.org/releases.json`. `link` must appear in the image's address, for editions
    /// that offer more than one image.
    Fedora { variant: &'static str, subvariant: &'static str, link: &'static str },
    /// System76's build list for a Pop!_OS release.
    Pop { release: &'static str, channel: &'static str },
    /// The newest GitHub release of `repo`, with an asset whose name fits `pattern`.
    GitHub { repo: &'static str, pattern: &'static str },
    /// Alpine's `latest-releases.yaml`.
    Alpine { flavor: &'static str },
    /// Gentoo's `latest-iso.txt`; the image's file name starts with `prefix`.
    Gentoo { prefix: &'static str },
    /// The newest stable NixOS channel; `kind` is `graphical` or `minimal`.
    NixOs { kind: &'static str },
    /// The newest link on the project's download page that fits `pattern`, with its checksum in
    /// a file next to it ending `.sha256`.
    PageLink { page: &'static str, pattern: &'static str },
    /// None that roomy can use: the image is only offered on this page.
    Website { page: &'static str },
}

/// A file's expected checksum, as lower-case hexadecimal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checksum {
    Sha256(String),
    Sha512(String),
}

impl Checksum {
    /// A SHA-256 or SHA-512 checksum, told apart by length.
    pub fn from_hex(hex: &str) -> Option<Checksum> {
        let hex = hex.trim().to_ascii_lowercase();
        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        match hex.len() {
            64 => Some(Checksum::Sha256(hex)),
            128 => Some(Checksum::Sha512(hex)),
            _ => None,
        }
    }

    pub fn hex(&self) -> &str {
        match self {
            Checksum::Sha256(hex) | Checksum::Sha512(hex) => hex,
        }
    }

    /// The program that computes this kind of checksum.
    pub(crate) fn program(&self) -> &'static str {
        match self {
            Checksum::Sha256(_) => "sha256sum",
            Checksum::Sha512(_) => "sha512sum",
        }
    }
}

/// An edition's current installation image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// As the project writes it; may be empty for rolling releases.
    pub version: String,
    /// What the file is called once downloaded.
    pub file_name: String,
    pub url: String,
    pub size: Option<u64>,
    pub checksum: Checksum,
}

/// The distribution with this id.
pub fn distro(id: &str) -> Option<&'static Distro> {
    DISTROS.iter().find(|distro| distro.id == id)
}

/// Where the pictures of each distribution's desktop are published, as `<id>.jpg`. They are
/// kept with roomy's source rather than built into it, so they can be refreshed between releases.
const SCREENSHOTS: &str = "https://raw.githubusercontent.com/mudlej/roomy/main/catalog/screenshots/";

/// A picture of the distribution's desktop, kept in roomy's cache folder and fetched again once
/// it is a month old. An old picture is still better than none when it cannot be fetched.
pub fn screenshot(curl: &Curl, distro_id: &str) -> Result<PathBuf> {
    const MONTH: Duration = Duration::from_secs(30 * 24 * 3600);
    let dir = crate::env::cache_dir()?.join("screenshots");
    std::fs::create_dir_all(&dir).map_err(|e| err!("cannot create {}: {e}", dir.display()))?;
    let file = dir.join(format!("{distro_id}.jpg"));
    let age = std::fs::metadata(&file).and_then(|m| m.modified()).ok().and_then(|at| at.elapsed().ok());
    if age.is_some_and(|age| age < MONTH) {
        return Ok(file);
    }
    match curl.save(&format!("{SCREENSHOTS}{distro_id}.jpg"), &file) {
        Ok(()) => Ok(file),
        Err(_) if age.is_some() => Ok(file),
        Err(e) => Err(e),
    }
}

/// A file size as people read it: `2.9 GB`, `750 MB`.
pub fn size_words(bytes: u64) -> String {
    let bytes = bytes as f64;
    if bytes >= 1e9 { format!("{:.1} GB", bytes / 1e9) } else { format!("{} MB", (bytes / 1e6).round().max(1.0)) }
}

/// Remembers every text it was given, so that editions announced in the same file (all of
/// Fedora's, say) read it once.
pub struct Remembering<F> {
    inner: F,
    texts: Mutex<HashMap<String, String>>,
}

impl<F: Fetch> Remembering<F> {
    pub fn new(inner: F) -> Remembering<F> {
        Remembering { inner, texts: Mutex::default() }
    }

    pub fn inner(&self) -> &F {
        &self.inner
    }
}

impl<F: Fetch> Fetch for Remembering<F> {
    fn text(&self, url: &str) -> Result<String> {
        if let Some(text) = self.texts.lock().unwrap().get(url) {
            return Ok(text.clone());
        }
        let text = self.inner.text(url)?;
        self.texts.lock().unwrap().insert(url.to_owned(), text.clone());
        Ok(text)
    }

    fn size(&self, url: &str) -> Option<u64> {
        self.inner.size(url)
    }
}

/// Reads what projects publish. [`Curl`] reads it from the web; tests read saved copies.
pub trait Fetch {
    /// A small text file (an index, a checksum list, a web page).
    fn text(&self, url: &str) -> Result<String>;
    /// The size of the file at `url`, when the server says.
    fn size(&self, url: &str) -> Option<u64>;
}

/// Reaches the web through the `curl` program, which every distribution has and which handles
/// redirects to mirrors, proxies and resuming far better than anything roomy could carry.
pub struct Curl {
    program: PathBuf,
}

/// Arguments that keep curl on encrypted connections, including where a server redirects to.
const HTTPS_ONLY: [&str; 4] = ["--proto", "=https", "--proto-redir", "=https"];

impl Curl {
    pub fn new(env: &Env) -> Result<Curl> {
        Ok(Curl { program: env.require("curl")? })
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Saves a small file (up to 16 MB) at `dest`, replacing it only once all of it has arrived.
    pub fn save(&self, url: &str, dest: &Path) -> Result<()> {
        let mut partial = dest.as_os_str().to_owned();
        partial.push(".partial");
        let partial = PathBuf::from(partial);
        let outcome = self
            .fetch_to(url, &partial)
            .and_then(|()| std::fs::rename(&partial, dest).map_err(|e| err!("cannot save {}: {e}", dest.display())));
        let _ = std::fs::remove_file(&partial);
        outcome
    }

    fn fetch_to(&self, url: &str, dest: &Path) -> Result<()> {
        let dest = dest.to_string_lossy();
        let mut args = vec!["--fail", "--silent", "--show-error", "--location", "--max-time", "60"];
        args.extend(HTTPS_ONLY);
        args.extend(["--max-filesize", "16777216", "--output", &dest, url]);
        let ran = process::run_capture(&self.program, &args, Duration::from_secs(70))
            .map_err(|e| err!("cannot run curl: {e}"))?;
        if ran.status.success() {
            Ok(())
        } else {
            Err(err!("cannot read {url}: {}", ran.stderr.trim().trim_start_matches("curl: ")))
        }
    }
}

impl Fetch for Curl {
    fn text(&self, url: &str) -> Result<String> {
        // Written to a file rather than read from curl's output: pages can be larger than what
        // `run_capture` can take in while waiting.
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let file = std::env::temp_dir().join(format!(
            "roomy-fetch-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let outcome = self.fetch_to(url, &file).and_then(|()| {
            std::fs::read(&file)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .map_err(|e| err!("cannot read what {url} sent: {e}"))
        });
        let _ = std::fs::remove_file(&file);
        outcome
    }

    fn size(&self, url: &str) -> Option<u64> {
        let mut args = vec!["--silent", "--head", "--location", "--max-time", "20"];
        args.extend(HTTPS_ONLY);
        args.push(url);
        let ran = process::run_capture(&self.program, &args, Duration::from_secs(30)).ok()?;
        ran.status.success().then(|| content_length(&ran.stdout)).flatten()
    }
}

/// The size the last of a chain of responses gives (redirects come first), if it was a success.
fn content_length(headers: &str) -> Option<u64> {
    let (mut ok, mut length) = (false, None);
    for line in headers.lines() {
        let lower = line.trim().to_ascii_lowercase();
        if lower.starts_with("http/") {
            ok = lower.split_whitespace().nth(1) == Some("200");
            length = None;
        } else if let Some(value) = lower.strip_prefix("content-length:") {
            length = value.trim().parse().ok();
        }
    }
    length.filter(|&size| ok && size > 0)
}

const UBUNTU_RELEASES: &str = "https://changelogs.ubuntu.com/meta-release-lts";
const FEDORA_RELEASES: &str = "https://fedoraproject.org/releases.json";
const ALPINE_RELEASES: &str =
    "https://dl-cdn.alpinelinux.org/alpine/latest-stable/releases/x86_64/latest-releases.yaml";
const GENTOO_RELEASES: &str = "https://distfiles.gentoo.org/releases/amd64/autobuilds/latest-iso.txt";

/// Finds an edition's current image.
pub fn resolve(fetch: &dyn Fetch, source: &Source) -> Result<Image> {
    let mut image = match *source {
        Source::Sums { list, pattern, fixed } => from_sums(fetch, list, pattern, fixed)?,
        Source::NewestFolder { index, list, pattern } => {
            let folder = newest_folder(&fetch.text(index)?)
                .ok_or_else(|| err!("{} lists no release folders any more", host(index)))?;
            from_sums(fetch, &format!("{index}{folder}/{list}"), pattern, None)?
        }
        Source::UbuntuLts { list, pattern } => {
            let (series, version) = sources::ubuntu_lts(&fetch.text(UBUNTU_RELEASES)?)
                .ok_or_else(|| err!("Ubuntu's list of releases names no supported LTS release"))?;
            let list = list.replace("{series}", &series);
            let mut image = from_sums(fetch, &list, pattern, None)
                .map_err(|_| err!("there is no build of this for Ubuntu {version}"))?;
            // A flavour can lag behind the release's latest point update.
            image.version = format!("{} LTS", image.version);
            image
        }
        Source::Fedora { variant, subvariant, link } => {
            let releases = fetch.text(FEDORA_RELEASES)?;
            sources::fedora(&releases, variant, subvariant, link)?
        }
        Source::Pop { release, channel } => {
            sources::pop(&fetch.text(&format!("https://api.pop-os.org/builds/{release}/{channel}"))?)?
        }
        Source::GitHub { repo, pattern } => {
            let latest = fetch.text(&format!("https://api.github.com/repos/{repo}/releases/latest"))?;
            let (mut image, sums) = sources::github_release(&latest, pattern)?;
            image.checksum = checksum_for(fetch, &sums, &image.file_name)?;
            image
        }
        Source::Alpine { flavor } => {
            let mut image = sources::alpine(&fetch.text(ALPINE_RELEASES)?, flavor)?;
            image.url = format!("{}{}", folder_of(ALPINE_RELEASES), image.file_name);
            image
        }
        Source::Gentoo { prefix } => {
            let (path, size) = sources::gentoo(&fetch.text(GENTOO_RELEASES)?, prefix)?;
            let url = format!("{}{path}", folder_of(GENTOO_RELEASES));
            let file_name = file_name_of(&url)?;
            let checksum = checksum_for(fetch, &format!("{url}.DIGESTS"), &file_name)?;
            let version = path.split('T').next().unwrap_or_default().to_owned();
            Image { version, file_name, url, size, checksum }
        }
        Source::NixOs { kind } => nixos(fetch, kind)?,
        Source::PageLink { page, pattern } => {
            let url = newest_link(&fetch.text(page)?, pattern)
                .ok_or_else(|| err!("{} no longer links to a current image", host(page)))?;
            let file_name = file_name_of(&url)?;
            let checksum = checksum_for(fetch, &format!("{url}.sha256"), &file_name)?;
            Image { version: version_in(&file_name), file_name, url, size: None, checksum }
        }
        Source::Website { page } => return Err(err!("roomy cannot download this one itself; it is offered on {page}")),
    };
    if image.size.is_none() {
        image.size = fetch.size(&image.url);
    }
    checked(image)
}

/// Finds the current image of every edition, each answer in the edition's place. The first is
/// looked up alone so that the others find what they share with it (all of Fedora's editions
/// are in one file) already read.
pub fn resolve_all<'a>(fetch: &(impl Fetch + Sync), editions: &'a [Edition]) -> Vec<(&'a Edition, Result<Image>)> {
    let Some((first, rest)) = editions.split_first() else { return Vec::new() };
    let mut found = vec![(first, resolve(fetch, &first.source))];
    std::thread::scope(|scope| {
        let running: Vec<_> =
            rest.iter().map(|edition| (edition, scope.spawn(move || resolve(fetch, &edition.source)))).collect();
        for (edition, thread) in running {
            found.push((edition, thread.join().unwrap_or_else(|_| Err(err!("the lookup failed")))));
        }
    });
    found
}

fn from_sums(fetch: &dyn Fetch, list: &str, pattern: &str, fixed: Option<&str>) -> Result<Image> {
    let listed = checksum_list(&fetch.text(list)?);
    let entry = newest(&listed, pattern)
        .ok_or_else(|| err!("{} no longer lists an image named like {pattern}", host(list)))?
        .clone();
    let url = fixed.map_or_else(|| format!("{}{}", folder_of(list), entry.name), str::to_owned);
    Ok(Image {
        version: version_in(&entry.name),
        file_name: entry.name,
        url,
        size: entry.size,
        checksum: entry.checksum,
    })
}

/// The checksum of `file_name` in the list at `list`. A list holding a single file is taken to
/// be about the image whatever it calls it, as projects that publish one checksum per image do.
fn checksum_for(fetch: &dyn Fetch, list: &str, file_name: &str) -> Result<Checksum> {
    let listed = checksum_list(&fetch.text(list)?);
    let entry = match listed.as_slice() {
        [only] => Some(only),
        all => all.iter().find(|entry| entry.name == file_name),
    };
    entry.map(|entry| entry.checksum.clone()).ok_or_else(|| err!("{} has no checksum for {file_name}", host(list)))
}

/// The newest stable NixOS channel that has the image: releases come out in May and November,
/// named `<year>.05` and `<year>.11`.
fn nixos(fetch: &dyn Fetch, kind: &str) -> Result<Image> {
    let now = crate::time::utc_timestamp(SystemTime::now());
    let year: u32 = now[2..4].parse().unwrap_or(26);
    let candidates = [format!("{year}.11"), format!("{year}.05"), format!("{}.11", year - 1)];
    for channel in candidates {
        let image = format!("https://channels.nixos.org/nixos-{channel}/latest-nixos-{kind}-x86_64-linux.iso");
        let Ok(list) = fetch.text(&format!("{image}.sha256")) else { continue };
        if let [entry] = checksum_list(&list).as_slice() {
            return Ok(Image {
                version: channel,
                file_name: entry.name.clone(),
                url: image,
                size: None,
                checksum: entry.checksum.clone(),
            });
        }
    }
    Err(err!("no current NixOS release offers the {kind} image"))
}

/// Refuses anything a project's list could name that would be unsafe to fetch or to save.
fn checked(image: Image) -> Result<Image> {
    let name = &image.file_name;
    let plain_name = !name.is_empty()
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "._-+~".contains(c));
    if !plain_name || !name.ends_with(".iso") {
        return Err(err!("the project lists an image with an unexpected name ({name})"));
    }
    if !image.url.starts_with("https://") {
        return Err(err!("the project offers {name} only over an unencrypted connection"));
    }
    Ok(image)
}

/// The host name in a URL, for messages.
fn host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split('/').next().unwrap_or(rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Serves saved copies of what projects publish.
    struct Saved(HashMap<&'static str, String>);

    impl Fetch for Saved {
        fn text(&self, url: &str) -> Result<String> {
            self.0.get(url).cloned().ok_or_else(|| err!("not found: {url}"))
        }
        fn size(&self, url: &str) -> Option<u64> {
            url.ends_with(".iso").then_some(1234)
        }
    }

    const SHA: &str = "a081ab202cfda17f6924128dbd2de8b63518ac0531bcfe3f1a1b88097c459bd4";

    #[test]
    fn images_are_found_through_each_kind_of_source() {
        let saved = Saved(HashMap::from([
            (
                "https://mirror.example.org/mint/stable/",
                r#"<a href="22.2/">22.2/</a> <a href="22.3/">22.3/</a>"#.to_owned(),
            ),
            (
                "https://mirror.example.org/mint/stable/22.3/sha256sum.txt",
                format!("{SHA} *linuxmint-22.3-cinnamon-64bit.iso\n{SHA} *linuxmint-22.3-xfce-64bit.iso\n"),
            ),
            (
                "https://download.example.org/tw/Tumbleweed-DVD-x86_64-Current.iso.sha256",
                format!("{SHA}  Tumbleweed-DVD-x86_64-Snapshot20261117-Media.iso\n"),
            ),
        ]));
        let mint = resolve(
            &saved,
            &Source::NewestFolder {
                index: "https://mirror.example.org/mint/stable/",
                list: "sha256sum.txt",
                pattern: "linuxmint-*-cinnamon-64bit.iso",
            },
        )
        .unwrap();
        assert_eq!(mint.url, "https://mirror.example.org/mint/stable/22.3/linuxmint-22.3-cinnamon-64bit.iso");
        assert_eq!((mint.version.as_str(), mint.size), ("22.3", Some(1234)));

        let tumbleweed = resolve(
            &saved,
            &Source::Sums {
                list: "https://download.example.org/tw/Tumbleweed-DVD-x86_64-Current.iso.sha256",
                pattern: "Tumbleweed-DVD-x86_64-*-Media.iso",
                fixed: Some("https://download.example.org/tw/Tumbleweed-DVD-x86_64-Current.iso"),
            },
        )
        .unwrap();
        assert_eq!(tumbleweed.file_name, "Tumbleweed-DVD-x86_64-Snapshot20261117-Media.iso");
        assert_eq!(tumbleweed.url, "https://download.example.org/tw/Tumbleweed-DVD-x86_64-Current.iso");
    }

    #[test]
    fn unsafe_names_and_addresses_are_refused() {
        let image = |name: &str, url: &str| Image {
            version: String::new(),
            file_name: name.to_owned(),
            url: url.to_owned(),
            size: None,
            checksum: Checksum::Sha256(SHA.to_owned()),
        };
        assert!(checked(image("debian-13.7.0-amd64-netinst.iso", "https://example.org/d.iso")).is_ok());
        assert!(checked(image("../../.bashrc.iso", "https://example.org/d.iso")).is_err());
        assert!(checked(image(".hidden.iso", "https://example.org/d.iso")).is_err());
        assert!(checked(image("notes.txt", "https://example.org/d.iso")).is_err());
        assert!(checked(image("debian.iso", "http://example.org/d.iso")).is_err());
    }

    #[test]
    fn sizes_come_from_the_last_response_after_redirects() {
        let headers = "HTTP/2 302\r\nlocation: https://mirror/x.iso\r\ncontent-length: 0\r\n\r\nHTTP/2 200\r\ncontent-length: 3892314112\r\n";
        assert_eq!(content_length(headers), Some(3892314112));
        assert_eq!(content_length("HTTP/2 404\r\ncontent-length: 120\r\n"), None);
    }

    #[test]
    fn every_edition_is_listed_once_and_can_be_named() {
        let mut ids = std::collections::HashSet::new();
        for distro in DISTROS {
            assert!(ids.insert(distro.id), "{} is listed twice", distro.id);
            assert!(distro.site.starts_with("https://"), "{}", distro.id);
            assert!(!distro.editions.is_empty(), "{} has no editions", distro.id);
            let mut slugs = std::collections::HashSet::new();
            for edition in distro.editions {
                assert!(slugs.insert(edition.slug), "{} {} is listed twice", distro.id, edition.slug);
                assert!(
                    crate::catalog::SYSTEMS.iter().any(|s| s.id == edition.guest),
                    "{} {}",
                    distro.id,
                    edition.slug
                );
            }
        }
    }
}
