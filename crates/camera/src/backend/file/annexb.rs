// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Access units read incrementally from an Annex B file.
//!
//! The splitting is `edgefirst_codec::video::access_units`, which works on a
//! complete buffer. Here the file is read in chunks: an access unit is
//! complete once the next one has started or the file has ended, so the last
//! one in the buffer waits for more bytes. Bytes that are not part of an
//! access unit (before the first start code, or a stretch with none) are
//! skipped with a warning.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;

use edgefirst_codec::video::access_units;

const CHUNK: usize = 256 * 1024;
/// Bytes kept when a buffer holds no start code: a start code may straddle
/// the next read.
const START_CODE_TAIL: usize = 3;

#[derive(Debug)]
pub(crate) struct AccessUnitReader<R> {
    src: R,
    buf: Vec<u8>,
    /// Start of the unread bytes in `buf`.
    start: usize,
    eof: bool,
}

impl<R: Read + Seek> AccessUnitReader<R> {
    pub(crate) fn new(src: R) -> Self {
        Self {
            src,
            buf: Vec::with_capacity(2 * CHUNK),
            start: 0,
            eof: false,
        }
    }

    /// The next access unit, or `None` at the end of the stream.
    pub(crate) fn next_unit(&mut self) -> io::Result<Option<Arc<[u8]>>> {
        loop {
            let pending = &self.buf[self.start..];
            let mut units = access_units(pending);
            let Some(first) = units.next() else {
                // No start code: none of this can belong to an access unit,
                // except a start code split by the read boundary.
                let keep = if self.eof { 0 } else { START_CODE_TAIL };
                self.skip(pending.len().saturating_sub(keep));
                if self.eof {
                    return Ok(None);
                }
                self.fill()?;
                continue;
            };
            let offset = first.as_ptr() as usize - pending.as_ptr() as usize;
            if offset > 0 {
                self.skip(offset);
                continue;
            }
            if units.next().is_none() && !self.eof {
                self.fill()?;
                continue;
            }
            let unit: Arc<[u8]> = first.into();
            self.start += unit.len();
            return Ok(Some(unit));
        }
    }

    /// Goes back to the start of the stream.
    pub(crate) fn rewind(&mut self) -> io::Result<()> {
        self.src.seek(SeekFrom::Start(0))?;
        self.buf.clear();
        self.start = 0;
        self.eof = false;
        Ok(())
    }

    fn skip(&mut self, n: usize) {
        if n > 0 {
            tracing::warn!("file: skipping {n} bytes that are not part of an H.264 access unit");
            self.start += n;
        }
    }

    /// Reads one chunk after the unread bytes; sets `eof` on a zero read.
    fn fill(&mut self) -> io::Result<()> {
        if self.start > 0 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        let len = self.buf.len();
        self.buf.resize(len + CHUNK, 0);
        let n = loop {
            match self.src.read(&mut self.buf[len..]) {
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.buf.truncate(len);
                    return Err(e);
                }
            }
        };
        self.buf.truncate(len + n);
        self.eof = n == 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const SPS: &[u8] = &[0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28];
    const PPS: &[u8] = &[0, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80];
    const IDR: &[u8] = &[0, 0, 1, 0x65, 0x88, 0x84, 0x00];
    const P_SLICE: &[u8] = &[0, 0, 0, 1, 0x41, 0x9a, 0x02];

    fn units(stream: Vec<u8>) -> Vec<Vec<u8>> {
        let mut r = AccessUnitReader::new(Cursor::new(stream));
        std::iter::from_fn(|| r.next_unit().unwrap().map(|u| u.to_vec())).collect()
    }

    /// A stream of `n` pictures, the first an IDR with its parameter sets.
    /// Each P slice carries `payload` bytes so units can span reads.
    fn stream(n: usize, payload: usize) -> (Vec<u8>, Vec<Vec<u8>>) {
        let mut aus = vec![[SPS, PPS, IDR].concat()];
        for i in 1..n {
            let mut p = P_SLICE.to_vec();
            // No zero bytes, so the payload never contains a start code.
            p.extend((0..payload).map(|j| (((i + j) % 250) + 1) as u8));
            aus.push(p);
        }
        (aus.concat(), aus)
    }

    #[test]
    fn yields_every_access_unit_in_order() {
        let (bytes, expected) = stream(5, 16);
        assert_eq!(units(bytes), expected);
    }

    #[test]
    fn units_spanning_reads_come_back_whole() {
        // 600 KiB units cross two read boundaries each.
        let (bytes, expected) = stream(4, 600 * 1024);
        let got = units(bytes);
        assert_eq!(got.len(), expected.len());
        assert!(got == expected, "a unit was split or joined across reads");
    }

    #[test]
    fn leading_garbage_is_skipped() {
        let (bytes, expected) = stream(3, 8);
        let garbage = [0xde, 0xad, 0xbe, 0xef, 0x00, 0x42];
        assert_eq!(units([&garbage[..], &bytes].concat()), expected);
    }

    #[test]
    fn a_stretch_without_start_codes_is_dropped_and_reading_resyncs() {
        let (bytes, expected) = stream(3, 8);
        // Junk longer than a read chunk, with no start code in it, before
        // the stream: the reader drops it chunk by chunk.
        let junk = vec![0xffu8; CHUNK + 1000];
        assert_eq!(units([junk, bytes].concat()), expected);
    }

    #[test]
    fn a_start_code_split_by_a_read_boundary_is_found() {
        let (bytes, expected) = stream(2, 8);
        // The first read ends two bytes into the stream's `00 00 00 01`.
        let junk = vec![0xffu8; CHUNK - 2];
        assert_eq!(units([junk, bytes].concat()), expected);
    }

    #[test]
    fn empty_and_garbage_only_files_have_no_units() {
        assert!(units(Vec::new()).is_empty());
        assert!(units(vec![1, 2, 3, 4, 5]).is_empty());
    }

    #[test]
    fn rewind_replays_the_stream() {
        let (bytes, expected) = stream(3, 8);
        let mut r = AccessUnitReader::new(Cursor::new(bytes));
        let first: Vec<_> = std::iter::from_fn(|| r.next_unit().unwrap()).collect();
        r.rewind().unwrap();
        let second: Vec<_> = std::iter::from_fn(|| r.next_unit().unwrap()).collect();
        assert_eq!(first.len(), expected.len());
        assert_eq!(first, second);
    }
}
