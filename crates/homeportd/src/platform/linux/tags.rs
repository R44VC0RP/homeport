//! Finder tags ↔ freedesktop tags. Finder stores tags as a binary plist
//! array of strings (`"Name"` or `"Name\n<color index>"`) in
//! `com.apple.metadata:_kMDItemUserTags`; KDE/Baloo reads a comma-separated
//! `user.xdg.tags`. Only that one shape is parsed and written.

/// `user.xdg.tags` value for a Finder tags plist (names only, colors dropped).
pub fn xdg_from_plist(plist: &[u8]) -> Option<Vec<u8>> {
    let names = parse_string_array(plist)?;
    let names: Vec<String> = names
        .into_iter()
        .map(|t| t.split('\n').next().unwrap_or_default().replace(',', " "))
        .filter(|t| !t.is_empty())
        .collect();
    Some(names.join(",").into_bytes())
}

/// A Finder tags plist for a `user.xdg.tags` value (tags without color).
pub fn plist_from_xdg(xdg: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(xdg).ok()?;
    let names: Vec<&str> = text.split(',').map(str::trim).filter(|t| !t.is_empty()).collect();
    if names.is_empty() {
        return None;
    }
    Some(write_string_array(&names))
}

fn be(b: &[u8]) -> Option<u64> {
    if b.len() > 8 {
        return None;
    }
    Some(b.iter().fold(0u64, |a, &x| (a << 8) | u64::from(x)))
}

fn parse_string_array(p: &[u8]) -> Option<Vec<String>> {
    if p.len() < 40 || !p.starts_with(b"bplist00") {
        return None;
    }
    let t = &p[p.len() - 32..];
    let off_size = usize::from(t[6]);
    let ref_size = usize::from(t[7]);
    let count = usize::try_from(be(&t[8..16])?).ok()?;
    let top = usize::try_from(be(&t[16..24])?).ok()?;
    let table = usize::try_from(be(&t[24..32])?).ok()?;
    if off_size == 0 || ref_size == 0 || top >= count {
        return None;
    }
    let offset = |i: usize| -> Option<usize> {
        let s = table.checked_add(i.checked_mul(off_size)?)?;
        usize::try_from(be(p.get(s..s + off_size)?)?).ok()
    };
    // Length that follows a marker: low nibble, or 0xF and an int object.
    let length = |at: usize| -> Option<(usize, usize)> {
        let m = *p.get(at)?;
        if m & 0x0F != 0x0F {
            return Some((usize::from(m & 0x0F), at + 1));
        }
        let im = *p.get(at + 1)?;
        if im & 0xF0 != 0x10 {
            return None;
        }
        let n = 1usize << (im & 0x0F);
        let v = usize::try_from(be(p.get(at + 2..at + 2 + n)?)?).ok()?;
        Some((v, at + 2 + n))
    };
    let at = offset(top)?;
    if p.get(at)? & 0xF0 != 0xA0 {
        return None;
    }
    let (n, mut pos) = length(at)?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let r = usize::try_from(be(p.get(pos..pos + ref_size)?)?).ok()?;
        pos += ref_size;
        let o = offset(r)?;
        let (len, start) = length(o)?;
        let s = match p.get(o)? & 0xF0 {
            0x50 => String::from_utf8_lossy(p.get(start..start + len)?).into_owned(),
            0x60 => {
                let b = p.get(start..start + len * 2)?;
                let units: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect();
                String::from_utf16_lossy(&units)
            }
            _ => return None,
        };
        out.push(s);
    }
    Some(out)
}

fn push_len(out: &mut Vec<u8>, marker: u8, n: usize) {
    if n < 15 {
        out.push(marker | n as u8);
    } else {
        out.push(marker | 0x0F);
        if let Ok(v) = u8::try_from(n) {
            out.extend_from_slice(&[0x10, v]);
        } else if let Ok(v) = u16::try_from(n) {
            out.push(0x11);
            out.extend_from_slice(&v.to_be_bytes());
        } else {
            out.push(0x12);
            out.extend_from_slice(&(n as u32).to_be_bytes());
        }
    }
}

fn write_string_array(names: &[&str]) -> Vec<u8> {
    let names = &names[..names.len().min(250)];
    let mut out = b"bplist00".to_vec();
    let mut offsets = vec![out.len()];
    push_len(&mut out, 0xA0, names.len());
    for i in 0..names.len() {
        out.push((i + 1) as u8);
    }
    for n in names {
        offsets.push(out.len());
        if n.is_ascii() {
            push_len(&mut out, 0x50, n.len());
            out.extend_from_slice(n.as_bytes());
        } else {
            let units: Vec<u16> = n.encode_utf16().collect();
            push_len(&mut out, 0x60, units.len());
            for u in units {
                out.extend_from_slice(&u.to_be_bytes());
            }
        }
    }
    let table = out.len();
    let off_size = if table > 0xFFFF {
        4u8
    } else if table > 0xFF {
        2
    } else {
        1
    };
    for o in &offsets {
        out.extend_from_slice(&(*o as u32).to_be_bytes()[4 - usize::from(off_size)..]);
    }
    out.extend_from_slice(&[0; 6]);
    out.push(off_size);
    out.push(1);
    out.extend_from_slice(&(offsets.len() as u64).to_be_bytes());
    out.extend_from_slice(&0u64.to_be_bytes());
    out.extend_from_slice(&(table as u64).to_be_bytes());
    out
}
