//! Row 10 Item 7 — minimal REAPI `ActionResult` protobuf codec.
//!
//! bazel-remote, with `--disable_http_ac_validation` removed (Item 7
//! re-enables it), parses every `PUT /ac/<key>` body as a serialized
//! `build.bazel.remote.execution.v2.ActionResult` and rejects it unless
//! it (a) decodes and (b) every referenced output digest is present in
//! CAS. Items 5-6's JSON-pointer placeholder bodies would 4xx the
//! moment validation is on. This module produces wire-compatible
//! protobuf instead.
//!
//! ## Why hand-rolled, not prost
//!
//! The full `remote_execution.proto` transitively pulls `google/api`,
//! `google/longrunning`, `google/rpc`, and a `prost-build` codegen step
//! in `build.rs` — a large surface to vendor and keep in sync for the
//! *three* messages bazel-remote's validator actually inspects
//! (`ActionResult`, `OutputFile`, `Digest`). The protobuf wire format
//! is stable and tiny; encoding these three with the upstream field
//! numbers is byte-compatible with any REAPI consumer and is fully
//! round-trip tested below. This keeps the dependency surface flat
//! (memory `feedback_planning_priorities`: clean/robust over least
//! code) and removes a build-time codegen failure mode.
//!
//! Field numbers (frozen by the REAPI v2 spec — do not renumber):
//!
//! ```text
//! Digest      { hash:string = 1, size_bytes:int64 = 2 }
//! OutputFile  { path:string = 1, digest:Digest = 2, is_executable:bool = 4 }
//! ActionResult{ output_files:repeated OutputFile = 2, exit_code:int32 = 9 }
//! ```

/// REAPI `Digest` — a content hash + byte size. `hash` is lowercase
/// hex sha256 for our deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest {
    pub hash: String,
    pub size_bytes: i64,
}

/// REAPI `OutputFile` — one produced artifact, by worktree-relative
/// path, pointing at a CAS blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputFile {
    pub path: String,
    pub digest: Digest,
    pub is_executable: bool,
}

/// REAPI `ActionResult` — the AC value. We populate `output_files`
/// (validated against CAS) and `exit_code` (0 for a green build).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ActionResult {
    pub output_files: Vec<OutputFile>,
    pub exit_code: i32,
}

// ---- wire helpers ------------------------------------------------------

fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if v == 0 {
            break;
        }
    }
}

fn put_tag(buf: &mut Vec<u8>, field: u32, wire: u8) {
    put_varint(buf, ((field << 3) | wire as u32) as u64);
}

fn put_len_delimited(buf: &mut Vec<u8>, field: u32, payload: &[u8]) {
    put_tag(buf, field, 2);
    put_varint(buf, payload.len() as u64);
    buf.extend_from_slice(payload);
}

impl Digest {
    fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        put_len_delimited(&mut b, 1, self.hash.as_bytes());
        if self.size_bytes != 0 {
            put_tag(&mut b, 2, 0);
            put_varint(&mut b, self.size_bytes as u64);
        }
        b
    }
}

impl OutputFile {
    fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        put_len_delimited(&mut b, 1, self.path.as_bytes());
        put_len_delimited(&mut b, 2, &self.digest.encode());
        if self.is_executable {
            put_tag(&mut b, 4, 0);
            put_varint(&mut b, 1);
        }
        b
    }
}

impl ActionResult {
    /// Serialize to the REAPI wire format bazel-remote validates.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        for f in &self.output_files {
            put_len_delimited(&mut b, 2, &f.encode());
        }
        if self.exit_code != 0 {
            put_tag(&mut b, 9, 0);
            put_varint(&mut b, self.exit_code as u64);
        }
        b
    }

    /// Decode (best-effort, skipping unknown fields) — used by the AC
    /// read path (Item 6) and the round-trip test.
    pub fn decode(data: &[u8]) -> Result<ActionResult, String> {
        let mut ar = ActionResult::default();
        let mut d = Decoder::new(data);
        while let Some((field, wire)) = d.next_tag()? {
            match (field, wire) {
                (2, 2) => {
                    let payload = d.read_len_delimited()?;
                    ar.output_files.push(OutputFile::decode(payload)?);
                }
                (9, 0) => ar.exit_code = d.read_varint()? as i32,
                _ => d.skip(wire)?,
            }
        }
        Ok(ar)
    }
}

impl OutputFile {
    fn decode(data: &[u8]) -> Result<OutputFile, String> {
        let mut path = String::new();
        let mut digest = Digest {
            hash: String::new(),
            size_bytes: 0,
        };
        let mut is_executable = false;
        let mut d = Decoder::new(data);
        while let Some((field, wire)) = d.next_tag()? {
            match (field, wire) {
                (1, 2) => path = String::from_utf8_lossy(d.read_len_delimited()?).into_owned(),
                (2, 2) => digest = Digest::decode(d.read_len_delimited()?)?,
                (4, 0) => is_executable = d.read_varint()? != 0,
                _ => d.skip(wire)?,
            }
        }
        Ok(OutputFile {
            path,
            digest,
            is_executable,
        })
    }
}

impl Digest {
    fn decode(data: &[u8]) -> Result<Digest, String> {
        let mut hash = String::new();
        let mut size_bytes = 0i64;
        let mut d = Decoder::new(data);
        while let Some((field, wire)) = d.next_tag()? {
            match (field, wire) {
                (1, 2) => hash = String::from_utf8_lossy(d.read_len_delimited()?).into_owned(),
                (2, 0) => size_bytes = d.read_varint()? as i64,
                _ => d.skip(wire)?,
            }
        }
        Ok(Digest { hash, size_bytes })
    }
}

struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn read_varint(&mut self) -> Result<u64, String> {
        let mut result = 0u64;
        let mut shift = 0;
        loop {
            let byte = *self.data.get(self.pos).ok_or("varint truncated")?;
            self.pos += 1;
            result |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
            if shift >= 64 {
                return Err("varint overflow".into());
            }
        }
    }

    fn next_tag(&mut self) -> Result<Option<(u32, u8)>, String> {
        if self.pos >= self.data.len() {
            return Ok(None);
        }
        let tag = self.read_varint()?;
        let field = (tag >> 3) as u32;
        let wire = (tag & 0x7) as u8;
        Ok(Some((field, wire)))
    }

    fn read_len_delimited(&mut self) -> Result<&'a [u8], String> {
        let len = self.read_varint()? as usize;
        let end = self
            .pos
            .checked_add(len)
            .filter(|e| *e <= self.data.len())
            .ok_or("length-delimited field truncated")?;
        let out = &self.data[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn skip(&mut self, wire: u8) -> Result<(), String> {
        match wire {
            0 => {
                self.read_varint()?;
            }
            2 => {
                self.read_len_delimited()?;
            }
            1 => self.pos += 8,
            5 => self.pos += 4,
            _ => return Err(format!("unsupported wire type {wire}")),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_result_round_trips() {
        let ar = ActionResult {
            output_files: vec![OutputFile {
                path: "target/release/hello-crate.exe".into(),
                digest: Digest {
                    hash: "4c6119b3".repeat(8), // 64 hex chars
                    size_bytes: 253749760,
                },
                is_executable: true,
            }],
            exit_code: 0,
        };
        let bytes = ar.encode();
        let back = ActionResult::decode(&bytes).expect("decode");
        assert_eq!(ar, back);
    }

    #[test]
    fn multi_file_and_nonzero_exit_round_trip() {
        let ar = ActionResult {
            output_files: vec![
                OutputFile {
                    path: "a.bin".into(),
                    digest: Digest {
                        hash: "00".repeat(32),
                        size_bytes: 1,
                    },
                    is_executable: false,
                },
                OutputFile {
                    path: "b/c.so".into(),
                    digest: Digest {
                        hash: "ff".repeat(32),
                        size_bytes: 1 << 40,
                    },
                    is_executable: true,
                },
            ],
            exit_code: 7,
        };
        assert_eq!(ActionResult::decode(&ar.encode()).unwrap(), ar);
    }

    #[test]
    fn empty_action_result_round_trips() {
        let ar = ActionResult::default();
        assert_eq!(ActionResult::decode(&ar.encode()).unwrap(), ar);
    }

    #[test]
    fn unknown_fields_are_skipped() {
        // Encode an ActionResult, then prepend a synthetic unknown
        // varint field (#15) — decode must ignore it.
        let ar = ActionResult {
            output_files: vec![],
            exit_code: 3,
        };
        let mut wire = Vec::new();
        put_tag(&mut wire, 15, 0);
        put_varint(&mut wire, 999);
        wire.extend_from_slice(&ar.encode());
        assert_eq!(ActionResult::decode(&wire).unwrap(), ar);
    }

    #[test]
    fn known_wire_bytes_match_spec_field_numbers() {
        // Digest{hash:"ab", size_bytes:0} → field1 (tag 0x0a) len 2 "ab"
        let d = Digest {
            hash: "ab".into(),
            size_bytes: 0,
        };
        assert_eq!(d.encode(), vec![0x0a, 0x02, b'a', b'b']);
        // ActionResult{exit_code:1} → field9 varint: tag=(9<<3)|0=0x48, val 1
        let ar = ActionResult {
            output_files: vec![],
            exit_code: 1,
        };
        assert_eq!(ar.encode(), vec![0x48, 0x01]);
    }
}
