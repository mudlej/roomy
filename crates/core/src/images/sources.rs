//! Finding the current image of an edition in what its project publishes.
//!
//! Every function here reads text a project published (a checksum list, a JSON index, a web page)
//! and is free of any network access, so each format is tested against a saved copy.

use super::{Checksum, Image};
use crate::{Result, err};
use std::cmp::Ordering;
use std::collections::HashMap;

/// One file named in a checksum list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub checksum: Checksum,
    /// Some lists also give each file's size.
    pub size: Option<u64>,
}

/// The files in a checksum list. Reads the common layouts: `<hash>  <name>` (also with `*` before
/// the name), `SHA256 (<name>) = <hash>`, and Gentoo's lists, where a `# SHA512 HASH` line says
/// which kind of hash the next lines hold. Sizes come from comment lines such as
/// `# <name>: <n> bytes`. Anything else, such as a signature around the list, is skipped.
pub fn checksum_list(text: &str) -> Vec<Listed> {
    let mut sizes = HashMap::new();
    let mut wanted_kind = true;
    let mut listed = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(comment) = line.strip_prefix('#') {
            let comment = comment.trim();
            if comment.ends_with("HASH") {
                wanted_kind = comment.starts_with("SHA256") || comment.starts_with("SHA512");
            } else if let Some((name, size)) = comment.split_once(": ")
                && let Some(bytes) = size.strip_suffix(" bytes").and_then(|n| n.parse::<u64>().ok())
            {
                sizes.insert(name.to_owned(), bytes);
            }
            continue;
        }
        if !wanted_kind {
            continue;
        }
        let entry = if let Some(rest) = line.strip_prefix("SHA256 (").or_else(|| line.strip_prefix("SHA512 (")) {
            rest.split_once(") = ").map(|(name, hash)| (hash, name))
        } else {
            line.split_once(char::is_whitespace).map(|(hash, name)| (hash, name.trim_start().trim_start_matches('*')))
        };
        if let Some((hash, name)) = entry
            && let Some(checksum) = Checksum::from_hex(hash)
            && !name.is_empty()
        {
            let name = name.trim_start_matches("./").to_owned();
            listed.push(Listed { name, checksum, size: None });
        }
    }
    for entry in &mut listed {
        entry.size = sizes.get(&entry.name).copied();
    }
    listed
}

/// Whether `name` fits `pattern`, where each `*` stands for one or more characters other than
/// `-` and `/`. Keeping `-` out lets `debian-*-amd64-netinst.iso` match Debian's own installer
/// but not `debian-edu-13.7.0-amd64-netinst.iso`.
pub fn fits(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((before, after)) => {
            let Some(rest) = name.strip_prefix(before) else { return false };
            let run = rest.find(['-', '/']).unwrap_or(rest.len());
            (1..=run).filter(|&taken| rest.is_char_boundary(taken)).any(|taken| fits(after, &rest[taken..]))
        }
    }
}

/// Compares names the way people read version numbers: `26.04.1` after `26.04`, `22.10` after `22.9`.
pub fn version_order(a: &str, b: &str) -> Ordering {
    fn runs(text: &str) -> Vec<&str> {
        let mut runs = Vec::new();
        let mut start = 0;
        for (i, c) in text.char_indices().skip(1) {
            let previous = text[..i].chars().next_back().unwrap_or(c);
            if previous.is_ascii_digit() != c.is_ascii_digit() {
                runs.push(&text[start..i]);
                start = i;
            }
        }
        runs.push(&text[start..]);
        runs
    }
    for (x, y) in runs(a).into_iter().zip(runs(b)) {
        let order = if x.starts_with(|c: char| c.is_ascii_digit()) && y.starts_with(|c: char| c.is_ascii_digit()) {
            let (x, y) = (x.trim_start_matches('0'), y.trim_start_matches('0'));
            x.len().cmp(&y.len()).then_with(|| x.cmp(y))
        } else {
            x.cmp(y)
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    a.len().cmp(&b.len())
}

/// The newest file in a checksum list whose name fits `pattern`.
pub fn newest<'a>(listed: &'a [Listed], pattern: &str) -> Option<&'a Listed> {
    listed.iter().filter(|entry| fits(pattern, &entry.name)).max_by(|a, b| version_order(&a.name, &b.name))
}

/// The version in a file name such as `ubuntu-26.04.1-desktop-amd64.iso`: the first part between
/// dashes that starts with a digit. Empty when there is none, as in a rolling release's name.
pub fn version_in(file_name: &str) -> String {
    let stem = file_name.strip_suffix(".iso").unwrap_or(file_name);
    stem.split('-').find(|part| part.starts_with(|c: char| c.is_ascii_digit())).unwrap_or_default().to_owned()
}

/// The highest version among the folders an index page links to, such as `22.3/` on Linux Mint's
/// mirror. Only names made of digits and dots count.
pub fn newest_folder(index_html: &str) -> Option<String> {
    links(index_html)
        .into_iter()
        .filter_map(|link| link.trim_start_matches("./").strip_suffix('/').map(str::to_owned))
        .filter(|name| !name.is_empty() && name.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .max_by(|a, b| version_order(a, b))
}

/// Every `href="…"` on a web page.
fn links(html: &str) -> Vec<&str> {
    html.split("href=\"").skip(1).filter_map(|rest| rest.split_once('"').map(|(link, _)| link)).collect()
}

/// The newest link on a web page that fits `pattern` (see [`fits`]), for projects that announce
/// their current image only on their download page.
pub fn newest_link(html: &str, pattern: &str) -> Option<String> {
    let mut found: Vec<&str> = Vec::new();
    // Pages built by script frameworks carry their links inside quoted data as well as in hrefs.
    for piece in html.split(['"', '\'', ' ', '<', '>', '[', ']', ',']) {
        let piece = piece.trim_end_matches("&quot;").trim_start_matches("&quot;");
        if piece.starts_with("https://") && fits(pattern, piece) {
            found.push(piece);
        }
    }
    found.into_iter().max_by(|a, b| version_order(a, b)).map(str::to_owned)
}

/// The current long-term support release in Ubuntu's `meta-release-lts`: its code name and its
/// version as people say it (`resolute`, `26.04.1 LTS`).
pub fn ubuntu_lts(meta_release: &str) -> Option<(String, String)> {
    let mut newest = None;
    for block in meta_release.split("\n\n") {
        let field =
            |name: &str| block.lines().find_map(|line| line.strip_prefix(name)?.strip_prefix(": ").map(str::trim));
        if field("Supported") == Some("1")
            && let (Some(dist), Some(version)) = (field("Dist"), field("Version"))
            && version.contains("LTS")
        {
            newest = Some((dist.to_owned(), version.to_owned()));
        }
    }
    newest
}

/// The image of one Fedora edition in `fedoraproject.org/releases.json`: of the final releases
/// (not betas) the highest version, x86-64, the installation image whose address contains `link`.
pub fn fedora(releases_json: &str, variant: &str, subvariant: &str, link: &str) -> Result<Image> {
    let releases: Vec<serde_json::Value> =
        serde_json::from_str(releases_json).map_err(|e| err!("Fedora's list of releases cannot be read: {e}"))?;
    let text = |release: &serde_json::Value, key: &str| release[key].as_str().unwrap_or_default().to_owned();
    let best = releases
        .iter()
        .filter(|r| {
            text(r, "arch") == "x86_64"
                && text(r, "variant") == variant
                && text(r, "subvariant") == subvariant
                && text(r, "link").ends_with(".iso")
                && text(r, "link").contains(link)
                && text(r, "version").chars().all(|c| c.is_ascii_digit())
        })
        .max_by(|a, b| version_order(&text(a, "version"), &text(b, "version")))
        .ok_or_else(|| err!("Fedora no longer lists this edition"))?;
    let url = text(best, "link");
    Ok(Image {
        version: text(best, "version"),
        file_name: file_name_of(&url)?,
        checksum: Checksum::from_hex(&text(best, "sha256"))
            .ok_or_else(|| err!("Fedora lists no checksum for {url}"))?,
        size: text(best, "size").parse().ok(),
        url,
    })
}

/// The image of a Pop!_OS build channel, from `api.pop-os.org/builds/<release>/<channel>`.
pub fn pop(build_json: &str) -> Result<Image> {
    let build: serde_json::Value =
        serde_json::from_str(build_json).map_err(|e| err!("Pop!_OS's build list cannot be read: {e}"))?;
    let url = build["url"].as_str().ok_or_else(|| err!("Pop!_OS lists no image"))?.to_owned();
    Ok(Image {
        version: build["version"].as_str().unwrap_or_default().to_owned(),
        file_name: file_name_of(&url)?,
        checksum: build["sha_sum"]
            .as_str()
            .and_then(Checksum::from_hex)
            .ok_or_else(|| err!("Pop!_OS lists no checksum for {url}"))?,
        size: build["size"].as_u64(),
        url,
    })
}

/// The image of the newest release on GitHub whose asset name fits `pattern`, with its checksum
/// in the asset of the same name ending `.sha256.txt` instead of `.iso` (Vanilla OS's layout).
/// Returns the image without its checksum and where to read the checksum.
pub fn github_release(release_json: &str, pattern: &str) -> Result<(Image, String)> {
    let release: serde_json::Value =
        serde_json::from_str(release_json).map_err(|e| err!("the release list cannot be read: {e}"))?;
    let assets = release["assets"].as_array().cloned().unwrap_or_default();
    let named = |name: &str| assets.iter().find(|a| a["name"].as_str() == Some(name)).cloned();
    let image = assets
        .iter()
        .filter_map(|a| a["name"].as_str())
        .filter(|name| fits(pattern, name))
        .max_by(|a, b| version_order(a, b))
        .ok_or_else(|| err!("the newest release has no image"))?;
    let sums_name = format!("{}.sha256.txt", image.strip_suffix(".iso").unwrap_or(image));
    let asset = named(image).unwrap_or_default();
    let sums = named(&sums_name).ok_or_else(|| err!("the newest release has no checksum for {image}"))?;
    let url_of = |asset: &serde_json::Value| asset["browser_download_url"].as_str().unwrap_or_default().to_owned();
    Ok((
        Image {
            version: release["tag_name"].as_str().unwrap_or_default().to_owned(),
            file_name: image.to_owned(),
            url: url_of(&asset),
            size: asset["size"].as_u64(),
            checksum: Checksum::Sha256(String::new()),
        },
        url_of(&sums),
    ))
}

/// The image of one Alpine flavour (`alpine-virt`, `alpine-standard`, ...) in the
/// `latest-releases.yaml` next to the images.
pub fn alpine(releases_yaml: &str, flavor: &str) -> Result<Image> {
    for block in releases_yaml.split("\n-") {
        let field = |name: &str| {
            block.lines().find_map(|line| line.trim().strip_prefix(name)?.strip_prefix(':').map(str::trim))
        };
        if field("flavor") == Some(flavor)
            && let Some(file) = field("iso").or(field("file")).filter(|file| file.ends_with(".iso"))
        {
            return Ok(Image {
                version: field("version").unwrap_or_default().to_owned(),
                file_name: file.to_owned(),
                url: String::new(),
                size: field("size").and_then(|size| size.parse().ok()),
                checksum: field("sha256")
                    .and_then(Checksum::from_hex)
                    .ok_or_else(|| err!("Alpine lists no checksum for {file}"))?,
            });
        }
    }
    Err(err!("Alpine no longer lists the {flavor} image"))
}

/// The path (relative to the list) and size of the image whose file name starts with `prefix`,
/// in Gentoo's `latest-iso.txt`.
pub fn gentoo(latest: &str, prefix: &str) -> Result<(String, Option<u64>)> {
    latest
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split_once(' '))
        .find(|(path, _)| file_name_of(path).is_ok_and(|name| name.starts_with(prefix) && name.ends_with(".iso")))
        .map(|(path, size)| (path.to_owned(), size.trim().parse().ok()))
        .ok_or_else(|| err!("Gentoo no longer lists this image"))
}

/// The last part of a URL or path.
pub fn file_name_of(url: &str) -> Result<String> {
    url.rsplit('/')
        .next()
        .filter(|name| !name.is_empty() && *name != ".." && *name != ".")
        .map(str::to_owned)
        .ok_or_else(|| err!("{url} does not name a file"))
}

/// The folder a URL points into, with its trailing `/`.
pub fn folder_of(url: &str) -> &str {
    url.rfind('/').map_or(url, |slash| &url[..=slash])
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "601e30fbf5d97759367c632e2c33630665039b7e2158fd068403da3ccf1bda1f";

    #[test]
    fn checksum_lists_are_read_in_every_layout_projects_use() {
        let gnu = format!("{SHA} *ubuntu-26.04.1-desktop-amd64.iso\n{SHA}  debian-13.7.0-amd64-netinst.iso\n");
        let names: Vec<_> = checksum_list(&gnu).into_iter().map(|l| l.name).collect();
        assert_eq!(names, ["ubuntu-26.04.1-desktop-amd64.iso", "debian-13.7.0-amd64-netinst.iso"]);

        let bsd = format!(
            "-----BEGIN PGP SIGNED MESSAGE-----\nHash: SHA256\n\n# Rocky-10.3-x86_64-boot.iso: 955719680 bytes\nSHA256 (Rocky-10.3-x86_64-boot.iso) = {SHA}\n"
        );
        assert_eq!(
            checksum_list(&bsd),
            [Listed {
                name: "Rocky-10.3-x86_64-boot.iso".into(),
                checksum: Checksum::Sha256(SHA.into()),
                size: Some(955719680)
            }]
        );

        let blake = "a".repeat(128);
        let sha512 = "b".repeat(128);
        let gentoo = format!("# BLAKE2B HASH\n{blake}  install.iso\n# SHA512 HASH\n{sha512}  install.iso\n");
        assert_eq!(checksum_list(&gentoo)[0].checksum, Checksum::Sha512(sha512));
        assert_eq!(checksum_list(&gentoo).len(), 1);
    }

    #[test]
    fn patterns_do_not_reach_across_dashes() {
        assert!(fits("debian-*-amd64-netinst.iso", "debian-13.7.0-amd64-netinst.iso"));
        assert!(!fits("debian-*-amd64-netinst.iso", "debian-edu-13.7.0-amd64-netinst.iso"));
        assert!(fits("void-live-x86_64-*-base.iso", "void-live-x86_64-20250202-base.iso"));
        assert!(!fits("void-live-x86_64-*-base.iso", "void-live-x86_64-musl-20250202-base.iso"));
        assert!(fits("archlinux-*-x86_64.iso", "archlinux-2026.11.01-x86_64.iso"));
        assert!(!fits("archlinux-*-x86_64.iso", "archlinux-x86_64.iso"));
        assert!(fits(
            "https://iso.example.org/desktop/*/x-*.iso",
            "https://iso.example.org/desktop/260809/x-260809.iso"
        ));
    }

    #[test]
    fn newer_versions_sort_later() {
        assert_eq!(version_order("ubuntu-26.04.1-desktop", "ubuntu-26.04-desktop"), Ordering::Greater);
        assert_eq!(version_order("22.10", "22.9"), Ordering::Greater);
        assert_eq!(version_order("lmde-7-cinnamon", "lmde-6-cinnamon"), Ordering::Greater);
        let list = format!("{SHA}  ubuntu-26.04-desktop-amd64.iso\n{SHA}  ubuntu-26.04.1-desktop-amd64.iso\n");
        assert_eq!(
            newest(&checksum_list(&list), "ubuntu-*-desktop-amd64.iso").unwrap().name,
            "ubuntu-26.04.1-desktop-amd64.iso"
        );
    }

    #[test]
    fn versions_are_read_from_file_names() {
        assert_eq!(version_in("ubuntu-26.04.1-desktop-amd64.iso"), "26.04.1");
        assert_eq!(version_in("debian-live-13.7.0-amd64-gnome.iso"), "13.7.0");
        assert_eq!(version_in("void-live-x86_64-20250202-base.iso"), "20250202");
        assert_eq!(version_in("openSUSE-Tumbleweed-DVD-x86_64-Snapshot20261117-Media.iso"), "");
    }

    #[test]
    fn index_pages_give_their_newest_numbered_folder() {
        let html = r#"<a href="../">..</a><a href="21.3/">21.3/</a><a href="22.3/">22.3/</a><a href="22/">22/</a><a href="./9.9/">x</a><a href="beta/">"#;
        assert_eq!(newest_folder(html).as_deref(), Some("22.3"));
    }

    #[test]
    fn download_pages_give_their_newest_matching_link() {
        let html = r#"<a href="https://download.example.org/kde/25.0.3/m-kde-25.0.3.iso">old</a>
            {&quot;url&quot;:[0,&quot;https://download.example.org/kde/26.1.0/m-kde-26.1.0.iso&quot;]}"#;
        assert_eq!(
            newest_link(html, "https://download.example.org/kde/*/m-kde-*.iso").as_deref(),
            Some("https://download.example.org/kde/26.1.0/m-kde-26.1.0.iso")
        );
    }

    #[test]
    fn the_current_ubuntu_lts_is_the_last_supported_one() {
        let meta = "Dist: noble\nVersion: 24.04.5 LTS\nSupported: 1\n\nDist: questing\nVersion: 25.10\nSupported: 1\n\n\
                    Dist: resolute\nVersion: 26.04.1 LTS\nSupported: 1\n\nDist: stonking\nVersion: 28.04 LTS\nSupported: 0\n";
        assert_eq!(ubuntu_lts(meta), Some(("resolute".into(), "26.04.1 LTS".into())));
    }

    #[test]
    fn fedora_editions_come_from_the_newest_final_release() {
        let entry = |version: &str, link: &str| {
            format!(
                r#"{{"version":"{version}","arch":"x86_64","link":"{link}","variant":"Workstation","subvariant":"Workstation","sha256":"{SHA}","size":"2930000000"}}"#
            )
        };
        let json = format!(
            "[{},{},{}]",
            entry("44", "https://dl.example.org/44/Fedora-Workstation-Live-44-1.7.x86_64.iso"),
            entry("45", "https://dl.example.org/45/Fedora-Workstation-Live-45-1.4.x86_64.iso"),
            entry("46 Beta", "https://dl.example.org/46/Fedora-Workstation-Live-46_Beta.x86_64.iso"),
        );
        let image = fedora(&json, "Workstation", "Workstation", "").unwrap();
        assert_eq!(image.version, "45");
        assert_eq!(image.file_name, "Fedora-Workstation-Live-45-1.4.x86_64.iso");
        assert_eq!(image.size, Some(2930000000));
        assert!(fedora(&json, "Workstation", "Workstation", "-netinst-").is_err());
        assert!(fedora(&json, "KDE", "KDE", "").is_err());
    }

    #[test]
    fn pop_builds_alpine_and_gentoo_lists_are_read() {
        let pop_json = format!(
            r#"{{"version":"24.04","url":"https://iso.pop-os.org/24.04/amd64/intel/20/pop-os_24.04_amd64_intel_20.iso","size":2955067392,"sha_sum":"{SHA}"}}"#
        );
        assert_eq!(pop(&pop_json).unwrap().file_name, "pop-os_24.04_amd64_intel_20.iso");

        let yaml = format!(
            "---\n-\n  title: \"Mini root filesystem\"\n  flavor: alpine-minirootfs\n  file: alpine-minirootfs-3.24.4-x86_64.tar.gz\n\
             -\n  title: \"Virtual\"\n  version: 3.24.4\n  flavor: alpine-virt\n  file: alpine-virt-3.24.4-x86_64.iso\n  iso: alpine-virt-3.24.4-x86_64.iso\n  size: 72351744\n  sha256: {SHA}\n"
        );
        let image = alpine(&yaml, "alpine-virt").unwrap();
        assert_eq!(
            (image.file_name.as_str(), image.version.as_str(), image.size),
            ("alpine-virt-3.24.4-x86_64.iso", "3.24.4", Some(72351744))
        );
        assert!(alpine(&yaml, "alpine-standard").is_err());

        let latest = "# Latest as of today\n20261115T170104Z/install-amd64-minimal-20261115T170104Z.iso 1051310080\n\
                      20260913T163055Z/livegui-amd64-20260913T163055Z.iso 4319342592\n";
        assert_eq!(
            gentoo(latest, "livegui-amd64").unwrap(),
            ("20260913T163055Z/livegui-amd64-20260913T163055Z.iso".into(), Some(4319342592))
        );
    }

    #[test]
    fn github_releases_give_the_image_and_where_its_checksum_is() {
        let json = r#"{"tag_name":"3.0","assets":[
            {"name":"Vanilla-OS-3-stable-amd64.20261106.iso","size":1756446720,"browser_download_url":"https://github.com/v/iso/Vanilla-OS-3-stable-amd64.20261106.iso"},
            {"name":"Vanilla-OS-3-stable-amd64.20261106.sha256.txt","size":105,"browser_download_url":"https://github.com/v/iso/Vanilla-OS-3-stable-amd64.20261106.sha256.txt"},
            {"name":"Vanilla-OS-3-stable-arm64.20261106.iso","size":1671327744,"browser_download_url":"https://github.com/v/iso/arm.iso"}]}"#;
        let (image, sums) = github_release(json, "Vanilla-OS-*-stable-amd64.*.iso").unwrap();
        assert_eq!(image.size, Some(1756446720));
        assert_eq!(sums, "https://github.com/v/iso/Vanilla-OS-3-stable-amd64.20261106.sha256.txt");
    }
}
