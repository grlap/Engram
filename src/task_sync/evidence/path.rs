//! Carrier namespace shape only. Pack bodies remain with the evidence reader.
use crate::ObjectId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Kind {
    Directory,
    Pack,
}

pub(crate) fn pack(origin: &ObjectId, id: &ObjectId) -> String {
    format!(".engram/evidence/{origin}/{}/{id}.json", &id.as_str()[..2])
}

pub(crate) fn kind(path: &str) -> Option<Kind> {
    let parts = path.split('/').collect::<Vec<_>>();
    if parts.len() < 2 || parts[0] != ".engram" || parts[1] != "evidence" {
        return None;
    }
    if parts.len() > 2 && !hex(parts[2], 32) {
        return None;
    }
    if parts.len() > 3 && !hex(parts[3], 2) {
        return None;
    }
    match parts.len() {
        2..=4 => Some(Kind::Directory),
        5 => {
            let id = parts[4].strip_suffix(".json")?;
            (hex(id, 32) && id.starts_with(parts[3])).then_some(Kind::Pack)
        }
        _ => None,
    }
}

fn hex(value: &str, size: usize) -> bool {
    value.len() == size
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
