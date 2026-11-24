//! Decode Apple's DER entitlement envelope into the plist representation.
//!
//! Profiles in the iOS provisioning database may contain entitlement bytes
//! rather than the dictionary present in downloaded mobileprovision files.

use plist::{Dictionary, Value};

const MAX_DEPTH: usize = 24;

fn tlv<'a>(input: &mut &'a [u8]) -> Result<(u8, &'a [u8]), String> {
    let original = *input;
    if original.len() < 2 || original[0] & 0x1f == 0x1f {
        return Err("invalid DER tag".into());
    }
    let tag = original[0];
    let first = original[1];
    let (len, header) = if first & 0x80 == 0 {
        (first as usize, 2)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > std::mem::size_of::<usize>() || original.len() < 2 + n || original[2] == 0 {
            return Err("invalid DER length".into());
        }
        let mut len = 0usize;
        for byte in &original[2..2+n] {
            len = len.checked_mul(256).and_then(|v| v.checked_add(*byte as usize)).ok_or("DER length overflow")?;
        }
        if len < 128 { return Err("non-canonical DER length".into()); }
        (len, 2 + n)
    };
    let end = header.checked_add(len).ok_or("DER length overflow")?;
    if end > original.len() { return Err("truncated DER value".into()); }
    *input = &original[end..];
    Ok((tag, &original[header..end]))
}

fn single<'a>(bytes: &'a [u8], expected: u8) -> Result<&'a [u8], String> {
    let mut remaining = bytes;
    let (tag, value) = tlv(&mut remaining)?;
    if tag != expected || !remaining.is_empty() { return Err("unexpected DER structure".into()); }
    Ok(value)
}

fn dict(mut body: &[u8], depth: usize) -> Result<Dictionary, String> {
    if depth > MAX_DEPTH { return Err("DER nesting too deep".into()); }
    let mut result = Dictionary::new();
    while !body.is_empty() {
        let (tag, mut pair) = tlv(&mut body)?;
        if tag != 0x30 { return Err("invalid DER dictionary entry".into()); }
        let (key_tag, key) = tlv(&mut pair)?;
        if key_tag != 0x0c { return Err("invalid DER dictionary key".into()); }
        let key = std::str::from_utf8(key).map_err(|_| "invalid DER UTF-8 key")?;
        let value = parse_value(&mut pair, depth + 1)?;
        if !pair.is_empty() || result.insert(key.to_owned(), value).is_some() {
            return Err("invalid or duplicate DER dictionary entry".into());
        }
    }
    Ok(result)
}

fn parse_value(input: &mut &[u8], depth: usize) -> Result<Value, String> {
    if depth > MAX_DEPTH { return Err("DER nesting too deep".into()); }
    let (tag, body) = tlv(input)?;
    match tag {
        0x01 if body == [0] => Ok(Value::Boolean(false)),
        0x01 if body == [0xff] => Ok(Value::Boolean(true)),
        0x02 => {
            if body.is_empty() || body.len() > 9 || (body.len() > 1 && ((body[0] == 0 && body[1] & 0x80 == 0) || (body[0] == 0xff && body[1] & 0x80 != 0))) {
                return Err("invalid DER integer".into());
            }
            let negative = body[0] & 0x80 != 0;
            if negative {
                if body.len() > 8 { return Err("DER integer too large".into()); }
                let mut bytes = [0xffu8; 8];
                bytes[8-body.len()..].copy_from_slice(body);
                Ok(Value::Integer(i64::from_be_bytes(bytes).into()))
            } else {
                let body = if body.len() == 9 && body[0] == 0 { &body[1..] } else { body };
                if body.len() > 8 { return Err("DER integer too large".into()); }
                let mut bytes = [0u8; 8];
                bytes[8-body.len()..].copy_from_slice(body);
                Ok(Value::Integer(u64::from_be_bytes(bytes).into()))
            }
        }
        0x04 => Ok(Value::Data(body.to_vec())),
        0x0c => Ok(Value::String(std::str::from_utf8(body).map_err(|_| "invalid DER UTF-8 value")?.to_owned())),
        0x30 => {
            let mut remaining = body;
            let mut values = Vec::new();
            while !remaining.is_empty() { values.push(parse_value(&mut remaining, depth + 1)?); }
            Ok(Value::Array(values))
        }
        0x31 => Ok(Value::Dictionary(dict(body, depth + 1)?)),
        _ => Err("unsupported DER entitlement type".into()),
    }
}

pub fn entitlements(bytes: &[u8]) -> Result<Dictionary, String> {
    let mut envelope = single(bytes, 0x70)?;
    let (tag, version) = tlv(&mut envelope)?;
    if tag != 0x02 || version != [1] { return Err("unsupported DER entitlement version".into()); }
    let body = single(envelope, 0xb0)?;
    dict(body, 0)
}

pub fn normalize_profile(xml: &[u8]) -> Result<Vec<u8>, String> {
    let mut profile: Dictionary = plist::from_bytes(xml).map_err(|_| "invalid profile plist")?;
    if let Some(Value::Data(data)) = profile.get("Entitlements") {
        let decoded = entitlements(data)?;
        profile.insert("Entitlements".into(), Value::Dictionary(decoded));
    }
    let mut output = Vec::new();
    plist::to_writer_xml(&mut output, &profile).map_err(|_| "cannot normalize profile plist")?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn wrap(tag: u8, body: &[u8]) -> Vec<u8> {
        assert!(body.len() < 128);
        let mut out = vec![tag, body.len() as u8]; out.extend_from_slice(body); out
    }
    fn entry(key: &str, value: Vec<u8>) -> Vec<u8> {
        let mut body = wrap(0x0c, key.as_bytes()); body.extend(value); wrap(0x30, &body)
    }
    fn envelope(entries: &[u8]) -> Vec<u8> {
        let mut b = wrap(0x02, &[1]); b.extend(wrap(0xb0, entries)); wrap(0x70, &b)
    }
    #[test]
    fn decodes_profile_entitlements() {
        let mut body = entry("application-identifier", wrap(0x0c, b"TEAM.example.app"));
        body.extend(entry("get-task-allow", wrap(0x01, &[0xff])));
        body.extend(entry("groups", wrap(0x30, &wrap(0x0c, b"TEAM.group"))));
        let decoded = entitlements(&envelope(&body)).unwrap();
        assert_eq!(decoded.get("application-identifier"), Some(&Value::String("TEAM.example.app".into())));
        assert_eq!(decoded.get("get-task-allow"), Some(&Value::Boolean(true)));
        assert_eq!(decoded.get("groups"), Some(&Value::Array(vec![Value::String("TEAM.group".into())])));
    }
    #[test]
    fn rejects_truncated_and_duplicate_values() {
        assert!(entitlements(&[0x70, 0x82, 1]).is_err());
        let mut body = entry("same", wrap(0x01, &[0]));
        body.extend(entry("same", wrap(0x01, &[0xff])));
        assert!(entitlements(&envelope(&body)).is_err());
        assert!(entitlements(&envelope(&entry("bad", wrap(0x01, &[1])))).is_err());
    }
    // Optional device-derived regression check: fixture files remain outside the source tree.
    #[test]
    fn normalizes_ios_db_fixture_when_available() {
        let (Ok(db), Ok(decoded)) = (
            std::env::var("REBOX_DB_PROFILE_FIXTURE"),
            std::env::var("REBOX_IOS_PROFILE_FIXTURE"),
        ) else { return; };
        let input = std::fs::read(db).unwrap();
        let expected: Dictionary = plist::from_file(decoded).unwrap();
        let actual: Dictionary = plist::from_bytes(&normalize_profile(&input).unwrap()).unwrap();
        assert_eq!(actual.get("Entitlements"), expected.get("Entitlements"));
    }
    #[test]
    fn normalizes_binary_entitlements_in_profile() {
        let mut profile = Dictionary::new();
        profile.insert("Entitlements".into(), Value::Data(envelope(&entry("key", wrap(0x0c, b"value")))));
        let mut xml = Vec::new(); plist::to_writer_xml(&mut xml, &profile).unwrap();
        let normalized: Dictionary = plist::from_bytes(&normalize_profile(&xml).unwrap()).unwrap();
        assert_eq!(normalized["Entitlements"].as_dictionary().unwrap()["key"], Value::String("value".into()));
    }
}
