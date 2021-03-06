//! Batch of Frames

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{self, Write};
use std::ops::Range;

use uuid::UUID;

use crate::{scan_for_float, scan_for_integer, scan_for_string, Frame};

/// An iterator over frames.
#[derive(Clone, Debug)]
pub struct Batch<'a> {
    body: Cow<'a, str>,
    next: Option<Range<usize>>,
}

impl<'a> Batch<'a> {
    /// Crates a new batch from text encoded frames `s`.
    pub fn parse<S>(s: S) -> Batch<'a>
    where
        S: Into<Cow<'a, str>>,
    {
        let b = s.into();
        let mut n = Batch::scan(&b[..]);

        if let Some(rgn) = n.clone() {
            if rgn.start == 0 {
                n = Batch::scan(&b[rgn.end..])
                    .map(|x| (x.start + rgn.end)..(x.end + rgn.end));
            }
        }

        Batch { body: b, next: n }
    }

    /// Indexes all frames. Returns map from object UUID to a pair of type and frames refering to
    /// the object.
    pub fn index(self) -> Option<HashMap<UUID, (UUID, Vec<Frame<'a>>)>> {
        use crate::Op;

        let mut index = HashMap::<UUID, (UUID, Vec<Frame<'a>>)>::default();

        for frm in self {
            match frm.peek().cloned() {
                Some(Op { ty, object, .. }) => {
                    let ent = index
                        .entry(object)
                        .or_insert_with(|| (ty.clone(), Default::default()));

                    if ent.0 == ty {
                        ent.1.push(frm);
                    } else {
                        error!(
                            "miss matched type/object pair: {} vs. {} for object {}",
                            ent.0, ty, object
                        );
                        return None;
                    }
                }
                None => {}
            }
        }

        Some(index)
    }

    /// Reduces all frames found in `self` and outputs the final status frames.
    pub fn reduce_all<W>(self, mut out: W) -> io::Result<()>
    where
        W: Write,
    {
        use crate::{Set, CRDT, LWW};
        use std::io::{Error, ErrorKind};
        use std::str::FromStr;

        let index = self
            .index()
            .ok_or(Error::new(ErrorKind::Other, "indexing failed"))?;
        let lww = UUID::from_str("lww").unwrap();
        let set = UUID::from_str("set").unwrap();

        for (_, (ty, mut frames)) in index {
            match frames.len() {
                0 => {}
                1 => {
                    out.write_all(frames[0].body().as_bytes())?;
                }
                _ => {
                    let s = frames.pop().unwrap();
                    let state = if ty == lww {
                        LWW::reduce(s, frames)
                    } else if ty == set {
                        Set::reduce(s, frames)
                    } else {
                        warn!("unknown type {}", ty);

                        out.write_all(s.body().as_bytes())?;
                        for frm in frames {
                            out.write_all(frm.body().as_bytes())?;
                        }
                        continue;
                    };

                    match state {
                        Some(state) => {
                            out.write_all(state.body().as_bytes())?;
                        }
                        None => {}
                    }
                }
            }
        }

        Ok(())
    }

    fn scan(s: &str) -> Option<Range<usize>> {
        #[derive(PartialEq, Eq, Clone, Copy, Debug)]
        enum Scan {
            Initial,
            SawType,
            SawObject,
            SawEvent,
            SawLoc,
        };
        let mut pos = 0usize;

        loop {
            let start = pos;
            let mut state = Scan::Initial;

            // spec
            loop {
                match (state, s.get(pos..pos + 1)) {
                    (Scan::Initial, Some("*")) => {
                        state = Scan::SawType;
                        pos += Self::scan_uuid(&s[pos + 1..]) + 1;
                    }
                    (Scan::Initial, Some("#")) | (Scan::SawType, Some("#")) => {
                        state = Scan::SawObject;
                        pos += Self::scan_uuid(&s[pos + 1..]) + 1;
                    }
                    (Scan::Initial, Some("@"))
                    | (Scan::SawType, Some("@"))
                    | (Scan::SawObject, Some("@")) => {
                        state = Scan::SawEvent;
                        pos += Self::scan_uuid(&s[pos + 1..]) + 1;
                    }
                    (Scan::Initial, Some(":"))
                    | (Scan::SawType, Some(":"))
                    | (Scan::SawObject, Some(":"))
                    | (Scan::SawEvent, Some(":")) => {
                        state = Scan::SawLoc;
                        pos += Self::scan_uuid(&s[pos + 1..]) + 1;
                    }
                    (_, Some(x)) => {
                        if x.chars().next().unwrap().is_whitespace() {
                            pos += 1;
                        } else {
                            break;
                        }
                    }
                    _ => {
                        break;
                    }
                }
            }

            if state == Scan::Initial {
                return None;
            }

            // atoms
            loop {
                match s.get(pos..pos + 1) {
                    // atoms
                    Some("=") => {
                        pos += scan_for_integer(&s[pos + 1..]).unwrap_or(0) + 1;
                    }
                    Some("^") => {
                        pos += scan_for_float(&s[pos + 1..]).unwrap_or(0) + 1;
                    }
                    Some(">") => {
                        pos += Self::scan_uuid(&s[pos + 1..]) + 1;
                    }
                    Some("\'") => {
                        pos += scan_for_string(&s[pos + 1..]).unwrap_or(0) + 2;
                    }

                    // terminator
                    Some("?") | Some(",") => {
                        pos += 1;
                        break;
                    }

                    Some("!") | Some(";") => {
                        return Some(start..pos + 1);
                    }

                    // next op
                    Some("*") | Some("#") | Some("@") | Some(":") => break,

                    // skip whitespace
                    Some(x) => {
                        if x.chars().next().unwrap().is_whitespace() {
                            pos += 1;
                        } else {
                            return None;
                        }
                    }

                    _ => {
                        return None;
                    }
                }
            }
        }
    }

    fn scan_uuid(s: &str) -> usize {
        let mut ret = 0;

        while let Some(ch) = s.get(ret..ret + 1) {
            let is_uuid_ch =
                ch.chars().next().map(|x| x.is_digit(36)).unwrap_or(false)
                    || ch == "~"
                    || ch == "_"
                    || ch == "-"
                    || ch == "+"
                    || ch == "%"
                    || ch == "("
                    || ch == "{"
                    || ch == "["
                    || ch == ")"
                    || ch == "}"
                    || ch == "]";

            if !is_uuid_ch {
                return ret;
            }

            ret += 1;
        }

        ret
    }
}

impl<'a> Iterator for Batch<'a> {
    type Item = Frame<'a>;

    fn next(&mut self) -> Option<Frame<'a>> {
        if self.body.is_empty() || self.body.starts_with(".") {
            return None;
        }

        let p = self.next.take();
        let end = p.clone().map(|x| x.start).unwrap_or(self.body.len());
        let ret = match &mut self.body {
            &mut Cow::Borrowed(s) => Frame::parse(&s[..end]),
            &mut Cow::Owned(ref mut s) => Frame::parse(s[..end].to_string()),
        };

        match p {
            Some(rgn) => {
                let start = rgn.start;
                let end = rgn.end;

                self.next = Batch::scan(&self.body[end..]).map(|x| {
                    let l = end - start;
                    (x.start + l)..(x.end + l)
                });

                match &mut self.body {
                    b @ &mut Cow::Borrowed(_) => {
                        let s = match b {
                            &mut Cow::Borrowed(s) => &s[start..],
                            _ => unreachable!(),
                        };

                        *b = Cow::Borrowed(s);
                    }
                    &mut Cow::Owned(ref mut s) => s.replace_range(0..start, ""),
                }
            }
            None => {
                self.body = Cow::Owned(String::default());
            }
        }

        Some(ret)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use simple_logger;
    use std::str::FromStr;

    #[test]
    fn batch_parse_none() {
        let mut b1 = Batch::parse("");

        assert!(b1.next().is_none());
        assert!(b1.next().is_none());
    }

    #[test]
    fn batch_parse_no_hdr() {
        let mut b1 = Batch::parse("*a#a@a:0,");

        assert!(b1.next().is_some());
        assert!(b1.next().is_none());
    }

    #[test]
    fn batch_parse_multi() {
        let b1 = Batch::parse("*lww#test@0:0! @1:key'value' @2:number=1 *rga#text@3:0'T'! *rga#text@6:3, @4'e' @5'x' @6't' *lww#more:a=1;.");

        for frm in b1.clone() {
            println!("frm {}", frm.body());
        }
        assert_eq!(b1.count(), 3);
    }

    #[test]
    fn batch_reduce_all() {
        use std::io::Cursor;
        use std::str;

        let _ = simple_logger::init_with_level(log::Level::Trace);

        let b = Batch::parse(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/batch-test"
        )));
        let mut c = Cursor::new(Vec::default());
        b.reduce_all(&mut c).unwrap();

        let s = c.into_inner();
        println!("{}", str::from_utf8(&s).unwrap());
    }

    #[test]
    fn index_one_obj() {
        let b1 = Batch::parse(
            "*lww#test@0:0! @1:key'value' *lww#test@2:0! @3:number=1",
        );
        let idx = b1.index().unwrap();
        let obj = UUID::from_str("test").unwrap();
        let ty = UUID::from_str("lww").unwrap();

        assert_eq!(idx.len(), 1);
        assert_eq!(idx[&obj].0, ty);
        assert_eq!(idx[&obj].1.len(), 2);
    }

    #[test]
    fn index_multiple_obj() {
        let b1 = Batch::parse("*lww#test@0:0! @1:key'value' @2:number=1 *rga#text@3:0'T'! *rga#text@6:3, @4'e' @5'x' @6't' *lww#more:a=1;.");
        let idx = b1.index().unwrap();
        let obj1 = UUID::from_str("test").unwrap();
        let obj2 = UUID::from_str("text").unwrap();
        let ty1 = UUID::from_str("lww").unwrap();
        let ty2 = UUID::from_str("rga").unwrap();

        assert_eq!(idx.len(), 2);
        assert_eq!(idx[&obj1].0, ty1);
        assert_eq!(idx[&obj1].1.len(), 1);
        assert_eq!(idx[&obj2].0, ty2);
        assert_eq!(idx[&obj2].1.len(), 1);
    }

    #[test]
    fn index_diff_type() {
        let b1 = Batch::parse(
            "*lww#test@0:0! @1:key'value' *rga#test@2:0! @3:number=1",
        );

        assert!(b1.index().is_none());
    }

    #[test]
    fn index_empty_batch() {
        let b1 = Batch::parse("");
        let idx = b1.index().unwrap();

        assert!(idx.is_empty());
    }
}
