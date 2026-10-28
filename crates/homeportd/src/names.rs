//! Raw names on disk ↔ names shown to clients. Platform-independent: inputs
//! are plain bytes / UTF-16 / strings, so every platform (and any host)
//! uses the same rules.
//!
//! - **Unrepresentable names.** Bytes that are not UTF-8 (Linux) become
//!   U+F780–U+F7FF (byte `0x80 + k` → U+F780 + k); unpaired UTF-16 surrogates
//!   (Windows) become U+F0000 + (unit − 0xD800). Both are private-use, so the
//!   mapping is reversible. Such items get `NAME_UNREPRESENTABLE`.
//! - **Windows-illegal characters.** The SFM / Samba `vfs_catia` private-use
//!   mapping the macOS SMB client uses: `"`→U+F020, `*`→F021, `:`→F022,
//!   `<`→F023, `>`→F024, `?`→F025, `\`→F026, `|`→F027, a trailing space →
//!   F028, a trailing dot → F029, 0x01–0x1F → U+F001–F01F. Displayed names
//!   map back, so a file shows the same name over SMB and Homeport.
//! - **Collisions.** The client (File Provider) folds case and Unicode
//!   normalization. In a directory on a sensitive filesystem, names with the
//!   same [`fold_key`] keep their raw names, but all but the oldest item get
//!   a display alias `name (2).ext` and `NAME_ALIASED`.

// The UTF-16 half is only used by the Windows platform.
#![cfg_attr(not(windows), allow(dead_code))]

use std::collections::{HashMap, HashSet};

use homeport_proto::item_flags;
use unicode_normalization::UnicodeNormalization;

use crate::platform::NamePolicy;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NameError {
    #[error("name is empty, `.`/`..`, or contains `/` or NUL")]
    Invalid,
    #[error("name is reserved on this server ({0})")]
    Reserved(String),
    #[error("name is too long")]
    TooLong,
}

const ESC_BYTE: u32 = 0xF780 - 0x80;
const ESC_SURROGATE: u32 = 0xF0000;

/// Display form of raw Unix bytes that are not valid UTF-8.
pub fn display_bytes(raw: &[u8]) -> (String, u32) {
    let mut out = String::with_capacity(raw.len() + 8);
    let mut flags = 0;
    for chunk in raw.utf8_chunks() {
        out.push_str(chunk.valid());
        for &b in chunk.invalid() {
            flags |= item_flags::NAME_UNREPRESENTABLE;
            out.push(char::from_u32(ESC_BYTE + u32::from(b)).unwrap_or('\u{FFFD}'));
        }
    }
    (out, flags)
}

/// Raw bytes for a client's name.
pub fn bytes_from_display(name: &str, policy: NamePolicy) -> Result<Vec<u8>, NameError> {
    if !homeport_proto::valid_name(name) {
        return Err(NameError::Invalid);
    }
    let normalized: String = match policy {
        NamePolicy::Apple => return Ok(name.as_bytes().to_vec()),
        _ => name.nfc().collect(),
    };
    let mut out = Vec::with_capacity(normalized.len());
    let mut buf = [0u8; 4];
    for c in normalized.chars() {
        match u32::from(c) {
            u @ 0xF780..=0xF7FF => out.push((u - ESC_BYTE) as u8),
            _ => out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes()),
        }
    }
    if out.len() > 255 {
        return Err(NameError::TooLong);
    }
    Ok(out)
}

fn sfm_to_ascii(u: u32) -> Option<char> {
    Some(match u {
        0xF001..=0xF01F => char::from_u32(u - 0xF000)?,
        0xF020 => '"',
        0xF021 => '*',
        0xF022 => ':',
        0xF023 => '<',
        0xF024 => '>',
        0xF025 => '?',
        0xF026 => '\\',
        0xF027 => '|',
        0xF028 => ' ',
        0xF029 => '.',
        _ => return None,
    })
}

fn ascii_to_sfm(c: char) -> Option<u16> {
    Some(match c {
        '\u{1}'..='\u{1f}' => 0xF000 + c as u16,
        '"' => 0xF020,
        '*' => 0xF021,
        ':' => 0xF022,
        '<' => 0xF023,
        '>' => 0xF024,
        '?' => 0xF025,
        '\\' => 0xF026,
        '|' => 0xF027,
        _ => return None,
    })
}

/// Display form of a raw Windows (UTF-16) name.
pub fn display_wide(raw: &[u16]) -> (String, u32) {
    let mut out = String::with_capacity(raw.len());
    let mut flags = 0;
    for r in char::decode_utf16(raw.iter().copied()) {
        match r {
            Ok(c) => match sfm_to_ascii(u32::from(c)) {
                Some(a) => out.push(a),
                None => out.push(c),
            },
            Err(e) => {
                flags |= item_flags::NAME_UNREPRESENTABLE;
                let u = u32::from(e.unpaired_surrogate());
                out.push(char::from_u32(ESC_SURROGATE + (u - 0xD800)).unwrap_or('\u{FFFD}'));
            }
        }
    }
    (out, flags)
}

/// Whether `name` is a DOS device name (`CON`, `com1.txt`, `LPT¹`, ...).
fn reserved_on_windows(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ').to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") {
        return true;
    }
    let mut chars = stem.chars();
    let prefix: String = chars.by_ref().take(3).collect();
    let rest: String = chars.collect();
    (prefix == "COM" || prefix == "LPT")
        && matches!(rest.as_str(), "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
}

/// Raw UTF-16 for a client's name on Windows.
pub fn wide_from_display(name: &str) -> Result<Vec<u16>, NameError> {
    if !homeport_proto::valid_name(name) {
        return Err(NameError::Invalid);
    }
    if reserved_on_windows(name) {
        return Err(NameError::Reserved(name.to_owned()));
    }
    let normalized: Vec<char> = name.nfc().collect();
    let mut out: Vec<u16> = Vec::with_capacity(normalized.len());
    let last = normalized.len().saturating_sub(1);
    let mut buf = [0u16; 2];
    for (i, &c) in normalized.iter().enumerate() {
        let u = u32::from(c);
        if (ESC_SURROGATE..ESC_SURROGATE + 0x800).contains(&u) {
            out.push((0xD800 + (u - ESC_SURROGATE)) as u16);
        } else if let Some(m) = ascii_to_sfm(c) {
            out.push(m);
        } else if i == last && c == ' ' {
            out.push(0xF028);
        } else if i == last && c == '.' {
            out.push(0xF029);
        } else {
            out.extend_from_slice(c.encode_utf16(&mut buf));
        }
    }
    if out.len() > 255 {
        return Err(NameError::TooLong);
    }
    Ok(out)
}

/// The key File Provider compares names by: case- and normalization-folded.
pub fn fold_key(name: &str) -> String {
    if name.is_ascii() {
        return name.to_ascii_lowercase();
    }
    name.nfd().flat_map(char::to_lowercase).collect()
}

/// `a.txt` → `a (2).txt`.
pub fn alias_name(name: &str, n: u32) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 => format!("{} ({n}){}", &name[..i], &name[i..]),
        _ => format!("{name} ({n})"),
    }
}

/// One directory entry for [`assign_aliases`].
pub struct Named<'a> {
    /// Older items sort first and keep the plain name (the item id).
    pub order: u64,
    pub name: &'a mut String,
    pub flags: &'a mut u32,
}

/// Gives every entry whose name folds to another's a unique alias. Returns
/// how many were aliased.
pub fn assign_aliases(entries: &mut [Named<'_>]) -> usize {
    let keys: Vec<String> = entries.iter().map(|e| fold_key(e.name)).collect();
    let mut groups: HashMap<&str, Vec<usize>> = HashMap::with_capacity(keys.len());
    for (i, k) in keys.iter().enumerate() {
        groups.entry(k.as_str()).or_default().push(i);
    }
    if groups.len() == keys.len() {
        return 0;
    }
    let mut taken: HashSet<String> = keys.iter().cloned().collect();
    let mut dups: Vec<Vec<usize>> = groups.into_values().filter(|g| g.len() > 1).collect();
    dups.sort_by_key(|g| g.iter().map(|&i| entries[i].order).min());
    let mut n_aliased = 0;
    for mut g in dups {
        g.sort_by_key(|&i| entries[i].order);
        for &i in &g[1..] {
            let base = entries[i].name.clone();
            let mut n = 2;
            let alias = loop {
                let a = alias_name(&base, n);
                if taken.insert(fold_key(&a)) {
                    break a;
                }
                n += 1;
            };
            *entries[i].name = alias;
            *entries[i].flags |= item_flags::NAME_ALIASED;
            n_aliased += 1;
        }
    }
    n_aliased
}
