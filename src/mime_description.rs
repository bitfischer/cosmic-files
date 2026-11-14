// SPDX-License-Identifier: GPL-3.0-only

//! Human readable descriptions of MIME types ("PNG image"), read from the
//! shared-mime-info database and localized with the user's locale.

use mime_guess::Mime;
use rustc_hash::FxHashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

static CACHE: LazyLock<Mutex<FxHashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(FxHashMap::default()));

/// Description of `mime`, falling back to the MIME type itself.
pub fn describe(mime: &Mime) -> String {
    let key = mime.essence_str();
    if let Ok(cache) = CACHE.lock()
        && let Some(text) = cache.get(key)
    {
        return text.clone();
    }
    let text = lookup(key).unwrap_or_else(|| key.to_string());
    if let Ok(mut cache) = CACHE.lock() {
        cache.insert(key.to_string(), text.clone());
    }
    text
}

fn data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        dirs.push(PathBuf::from(home));
    } else if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share"));
    }
    match std::env::var("XDG_DATA_DIRS") {
        Ok(v) if !v.is_empty() => dirs.extend(v.split(':').map(PathBuf::from)),
        _ => {
            dirs.push(PathBuf::from("/usr/local/share"));
            dirs.push(PathBuf::from("/usr/share"));
        }
    }
    dirs
}

/// Locales in order of preference, e.g. `["de-DE", "de"]`.
fn locales() -> Vec<String> {
    let raw = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
        .unwrap_or_default();
    // Strip encoding and modifier: de_DE.UTF-8@euro
    let base = raw.split(['.', '@']).next().unwrap_or("").replace('_', "-");
    if base.is_empty() || base == "C" || base == "POSIX" {
        return Vec::new();
    }
    let mut out = vec![base.clone()];
    if let Some((lang, _)) = base.split_once('-') {
        out.push(lang.to_string());
    }
    out
}

fn lookup(essence: &str) -> Option<String> {
    // Reject anything that could escape the mime directory
    if essence.contains("..") || essence.matches('/').count() != 1 {
        return None;
    }
    let locales = locales();
    data_dirs().into_iter().find_map(|dir| {
        let xml = std::fs::read_to_string(dir.join("mime").join(format!("{essence}.xml"))).ok()?;
        pick_comment(&xml, &locales)
    })
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Pick the `<comment>` best matching `locales`, else the untranslated one.
fn pick_comment(xml: &str, locales: &[String]) -> Option<String> {
    let mut default = None;
    let mut by_lang: Vec<(String, String)> = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<comment") {
        rest = &rest[start + "<comment".len()..];
        // Skip other elements starting with "comment"
        if !rest.starts_with([' ', '>']) {
            continue;
        }
        let Some(tag_end) = rest.find('>') else { break };
        let attrs = &rest[..tag_end];
        let Some(text_len) = rest[tag_end + 1..].find("</comment>") else {
            break;
        };
        let text = unescape(rest[tag_end + 1..tag_end + 1 + text_len].trim());
        match attrs
            .split_once("xml:lang=\"")
            .and_then(|(_, v)| v.split_once('"'))
        {
            Some((lang, _)) => by_lang.push((lang.replace('_', "-"), text)),
            None => {
                default.get_or_insert(text);
            }
        }
        rest = &rest[tag_end + 1 + text_len..];
    }
    for locale in locales {
        if let Some((_, text)) = by_lang
            .iter()
            .find(|(lang, _)| lang.eq_ignore_ascii_case(locale))
        {
            return Some(text.clone());
        }
    }
    default.filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<?xml version="1.0"?>
<mime-type type="image/png">
  <comment>PNG image</comment>
  <comment xml:lang="de">PNG-Bild</comment>
  <comment xml:lang="pt_BR">Imagem PNG</comment>
  <comment xml:lang="en-GB">PNG &amp; co</comment>
  <acronym>PNG</acronym>
</mime-type>"#;

    fn loc(l: &[&str]) -> Vec<String> {
        l.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn picks_locale_then_language_then_default() {
        assert_eq!(
            pick_comment(XML, &loc(&["de-DE", "de"])).unwrap(),
            "PNG-Bild"
        );
        assert_eq!(
            pick_comment(XML, &loc(&["pt-BR", "pt"])).unwrap(),
            "Imagem PNG"
        );
        assert_eq!(
            pick_comment(XML, &loc(&["en-GB", "en"])).unwrap(),
            "PNG & co"
        );
        assert_eq!(pick_comment(XML, &loc(&["fr"])).unwrap(), "PNG image");
        assert_eq!(pick_comment(XML, &[]).unwrap(), "PNG image");
    }

    #[test]
    fn missing_comment_and_bad_types() {
        assert_eq!(pick_comment("<mime-type/>", &[]), None);
        assert_eq!(lookup("../../etc/passwd"), None);
        assert_eq!(lookup("nonsense"), None);
    }
}
