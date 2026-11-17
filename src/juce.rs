//! JUCE's two serialised forms of a `ValueTree`, as plugins built on JUCE
//! store them: the binary stream `ValueTree::writeToStream` writes (Odin 2's
//! `.odin` patches) and the XML block `AudioProcessor::copyXmlToBinary` makes
//! (their CLAP state). Converting the first into the second lets a patch file
//! load through the plugin's state, along the same path a host restore takes.

use anyhow::{anyhow, bail, ensure, Result};
use std::fmt::Write;

/// `AudioProcessor::copyXmlToBinary`'s magic number ("VC2!").
const XML_MAGIC: u32 = 0x2132_4356;

/// A binary `ValueTree` stream as the plugin state `setStateInformation` reads.
pub fn value_tree_to_state(stream: &[u8]) -> Result<Vec<u8>> {
    let mut r = Reader { data: stream, pos: 0 };
    let mut xml = String::from(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    write_tree(&mut r, &mut xml, 0)?;
    ensure!(r.pos == stream.len(), "{} bytes after the value tree", stream.len() - r.pos);
    Ok(xml_to_state(&xml))
}

/// `copyXmlToBinary`: magic, the text length including its NUL, the text, NUL.
pub fn xml_to_state(xml: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(xml.len() + 9);
    out.extend_from_slice(&XML_MAGIC.to_le_bytes());
    out.extend_from_slice(&(xml.len() as u32 + 1).to_le_bytes());
    out.extend_from_slice(xml.as_bytes());
    out.push(0);
    out
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.data.len()).ok_or_else(|| anyhow!("value tree ends early"))?;
        let bytes = &self.data[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// `OutputStream::writeString`: UTF-8 up to a NUL.
    fn string(&mut self) -> Result<&str> {
        let len = self.data[self.pos..].iter().position(|&b| b == 0).ok_or_else(|| anyhow!("string without NUL"))?;
        let start = self.pos;
        self.pos += len + 1;
        Ok(std::str::from_utf8(&self.data[start..start + len])?)
    }

    /// `OutputStream::writeCompressedInt`: a byte holding the sign (0x80) and
    /// the count of little-endian magnitude bytes that follow.
    fn compressed_int(&mut self) -> Result<i64> {
        let head = self.byte()?;
        let n = (head & 0x7f) as usize;
        ensure!(n <= 4, "compressed int of {n} bytes");
        let magnitude = self.take(n)?.iter().rev().fold(0i64, |v, &b| (v << 8) | b as i64);
        Ok(if head & 0x80 != 0 { -magnitude } else { magnitude })
    }

    fn count(&mut self) -> Result<usize> {
        usize::try_from(self.compressed_int()?).map_err(|_| anyhow!("negative count"))
    }
}

/// One `var` in `var::writeToStream`'s form, as `var::toString` prints it.
fn read_var(r: &mut Reader) -> Result<String> {
    let size = r.count()?;
    if size == 0 {
        return Ok(String::new());
    }
    let body = r.take(size)?;
    let (marker, data) = (body[0], &body[1..]);
    let le8 = || -> Result<[u8; 8]> { data.try_into().map_err(|_| anyhow!("8-byte var of {} bytes", data.len())) };
    Ok(match marker {
        1 => i32::from_le_bytes(data.try_into().map_err(|_| anyhow!("int var of {} bytes", data.len()))?).to_string(),
        2 => "1".into(),
        3 => "0".into(),
        4 => f64::from_le_bytes(le8()?).to_string(),
        5 => std::str::from_utf8(data.strip_suffix(&[0]).unwrap_or(data))?.to_string(),
        6 => i64::from_le_bytes(le8()?).to_string(),
        other => bail!("var type {other} has no XML attribute form"),
    })
}

/// `ValueTree::writeToStream`: type, properties, children; written as
/// `ValueTree::createXml` would (tag = type, one attribute per property).
fn write_tree(r: &mut Reader, xml: &mut String, depth: usize) -> Result<()> {
    ensure!(depth < 64, "value tree nested deeper than 64");
    let tag = r.string()?.to_string();
    ensure!(!tag.is_empty() && tag.chars().all(|c| c.is_alphanumeric() || "_-.:".contains(c)), "bad tree type '{tag}'");
    xml.push('<');
    xml.push_str(&tag);
    for _ in 0..r.count()? {
        let name = r.string()?.to_string();
        let value = read_var(r)?;
        let _ = write!(xml, " {name}=\"{}\"", escape(&value));
    }
    let children = r.count()?;
    if children == 0 {
        xml.push_str("/>");
        return Ok(());
    }
    xml.push('>');
    for _ in 0..children {
        write_tree(r, xml, depth + 1)?;
    }
    let _ = write!(xml, "</{tag}>");
    Ok(())
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "&#{};", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compressed(v: u32) -> Vec<u8> {
        let bytes: Vec<u8> = v.to_le_bytes().into_iter().rev().skip_while(|&b| b == 0).collect::<Vec<_>>().into_iter().rev().collect();
        std::iter::once(bytes.len() as u8).chain(bytes).collect()
    }

    fn cstr(s: &str) -> Vec<u8> {
        s.bytes().chain(std::iter::once(0)).collect()
    }

    fn var(marker: u8, data: &[u8]) -> Vec<u8> {
        let mut v = compressed(data.len() as u32 + 1);
        v.push(marker);
        v.extend_from_slice(data);
        v
    }

    #[test]
    fn a_value_tree_becomes_the_xml_state_juce_reads() {
        // Odin { PARAM id="amp_gain" value=-6.5 } { misc name="A & B" n=3 on=true }
        let mut t = cstr("Odin");
        t.extend(compressed(0));
        t.extend(compressed(2));
        t.extend(cstr("PARAM"));
        t.extend(compressed(2));
        t.extend(cstr("id"));
        t.extend(var(5, &cstr("amp_gain")));
        t.extend(cstr("value"));
        t.extend(var(4, &(-6.5f64).to_le_bytes()));
        t.extend(compressed(0));
        t.extend(cstr("misc"));
        t.extend(compressed(3));
        t.extend(cstr("name"));
        t.extend(var(5, &cstr("A & \"B\"")));
        t.extend(cstr("n"));
        t.extend(var(1, &3i32.to_le_bytes()));
        t.extend(cstr("on"));
        t.extend(var(2, &[]));
        t.extend(compressed(0));

        let state = value_tree_to_state(&t).unwrap();
        assert_eq!(&state[..4], b"VC2!");
        let len = u32::from_le_bytes(state[4..8].try_into().unwrap()) as usize;
        assert_eq!(len, state.len() - 8, "length counts the text and its NUL");
        assert_eq!(*state.last().unwrap(), 0);
        let xml = std::str::from_utf8(&state[8..state.len() - 1]).unwrap();
        assert_eq!(
            xml,
            r#"<?xml version="1.0" encoding="UTF-8"?><Odin><PARAM id="amp_gain" value="-6.5"/><misc name="A &amp; &quot;B&quot;" n="3" on="1"/></Odin>"#
        );
    }

    #[test]
    fn truncated_or_padded_streams_are_refused() {
        let mut t = cstr("Odin");
        t.extend(compressed(0));
        t.extend(compressed(0));
        assert!(value_tree_to_state(&t).is_ok());
        assert!(value_tree_to_state(&t[..t.len() - 1]).is_err(), "missing child count");
        t.push(7);
        assert!(value_tree_to_state(&t).is_err(), "trailing bytes");
    }
}
