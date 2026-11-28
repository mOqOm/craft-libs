//! Opening a file: the Segment walk, the sample index and `MkvFile` helpers.

use super::block::{BlockRef, FrameRef, TrackIndex, expand, scan_cluster, track_index};
use super::io::{Cache, Hdr, Io};
use super::{FrameBudget, MAX_EBML_HEADER, MAX_META, MAX_TRACKS, MkvFile, OpenOptions, RECOVERY_BUDGET, Resume, WIN, warn};
use crate::matroska::ebml;
use crate::matroska::ebml::{Children, Size};
use crate::matroska::error::{Error, Result};
use crate::matroska::ids::{
    ATTACHED_FILE, ATTACHMENTS, CHAPTERS, CLUSTER, CUES, DOC_TYPE, DOC_TYPE_READ_VERSION, DOC_TYPE_VERSION, EBML, EBML_MAX_ID_LENGTH,
    EBML_MAX_SIZE_LENGTH, FILE_DATA, FILE_DESCRIPTION, FILE_MEDIA_TYPE, FILE_NAME, FILE_UID, INFO, SEEK_HEAD, SEGMENT, TAGS, TRACK_ENTRY, TRACKS,
    VOID,
};
use crate::matroska::index_store;
use crate::matroska::meta::{Attachment, SegmentInfo, parse_chapters, parse_cues, parse_info, parse_seekhead, parse_tags, parse_tracks};
use crate::matroska::track::{Sample, TrackKind, gcd};
use crate::source::ByteSource;
use std::collections::HashMap;

/// Open a Matroska/WebM file with default options (full sample index).
pub fn open<S: ByteSource>(src: S) -> Result<MkvFile> {
    open_with(src, &OpenOptions::default())
}

/// Open with explicit options.
///
/// A full index (`opts.index`, without `verify_crc` or an `index_limit`) is taken from the
/// [`crate::matroska::IndexStore`] registered with [`crate::matroska::set_index_store`] when it
/// holds this file's, and saved there after a scan (the crate's `matroska::index_store` module says
/// how a file is known and its saved tables are checked).
pub fn open_with<S: ByteSource>(src: S, opts: &OpenOptions) -> Result<MkvFile> {
    let mut cache = Cache::default();
    open_stored(&src, opts, &mut cache)
}

/// [`open_impl`], a full index taken from and saved to the registered
/// [`crate::matroska::IndexStore`].
pub(super) fn open_stored<S: ByteSource + ?Sized>(src: &S, opts: &OpenOptions, cache: &mut Cache) -> Result<MkvFile> {
    let stored = opts.index && !opts.verify_crc && opts.index_limit.is_none();
    let Some(store) = stored.then(index_store::index_store).flatten() else {
        return open_impl(src, opts, cache, None).map(|(file, _)| file);
    };
    let key = index_store::key_of(src);
    if let Some(saved) = key.as_deref().and_then(|key| store.load(key)).and_then(|bytes| index_store::decode(&bytes, src.len())) {
        let mut from_store = Cache::default();
        // Tables that do not fit the file: scanned as if there were none.
        if let Ok((file, _)) = open_impl(src, opts, &mut from_store, Some(saved)) {
            *cache = from_store;
            return Ok(file);
        }
    }
    let (file, elements) = open_impl(src, opts, cache, None)?;
    if let Some(key) = key.as_deref()
        && file.indexed
        && file.index_end.is_none()
    {
        let tracks: Vec<(u64, &[Sample], &[u32])> = file.tracks.iter().map(|t| (t.number, t.samples.as_slice(), t.sync.as_slice())).collect();
        store.save(key, &index_store::encode(&file.clusters, &tracks, &elements, &file.warnings, src.len()));
    }
    Ok(file)
}

/// Opens the file, and returns the positions of the level-1 elements its walk read. With `saved`
/// (an index from the store), only the headers are walked and the saved tables are attached after
/// the elements the scan had read are read too; tables that do not fit the file are an error.
fn open_impl<S: ByteSource + ?Sized>(
    src: &S,
    opts: &OpenOptions,
    cache: &mut Cache,
    saved: Option<index_store::Saved>,
) -> Result<(MkvFile, Vec<u64>)> {
    let header_opts;
    let opts = if saved.is_some() {
        header_opts = OpenOptions { index: false, ..opts.clone() };
        &header_opts
    } else {
        opts
    };
    // each pass over the file (open, building the index) gets the whole budget
    cache.recovery_left = RECOVERY_BUDGET;
    let mut io = Io::new(src, cache);
    if let Some(l) = opts.index_limit.filter(|_| opts.index) {
        io.avail = l;
    }
    let len = io.len;
    // EBML header (tolerate leading junk in the first 64 KiB)
    let start = {
        let magic = [0x1A, 0x45, 0xDF, 0xA3];
        // the usual case needs 4 bytes, not the whole window (a source may hold only the front of the file)
        if io.peek(0, 4)? == magic {
            0
        } else {
            let b = io.peek(0, WIN)?;
            b.windows(4).position(|w| w == magic).ok_or_else(|| Error::NotMatroska("no EBML header".into()))? as u64
        }
    };
    let eh = io.header(start, len)?.ok_or_else(|| Error::NotMatroska("bad EBML header".into()))?;
    let hdr_end = eh.end(len);
    let hdr_len = hdr_end - eh.data_start();
    if hdr_len > MAX_EBML_HEADER {
        return Err(Error::NotMatroska(format!("EBML header of {hdr_len} bytes")));
    }
    let hdr = io.read_vec(eh.data_start(), hdr_len)?;
    let (mut doc_type, mut dtv, mut dtrv) = ("matroska".to_string(), 1, 1);
    for e in Children::new(&hdr) {
        match e.id {
            DOC_TYPE => doc_type = ebml::string(e.data),
            DOC_TYPE_VERSION => dtv = ebml::uint(e.data),
            DOC_TYPE_READ_VERSION => dtrv = ebml::uint(e.data),
            EBML_MAX_ID_LENGTH if ebml::uint(e.data) > 4 => return Err(Error::Unsupported("EBMLMaxIDLength > 4".into())),
            EBML_MAX_SIZE_LENGTH if ebml::uint(e.data) > 8 => return Err(Error::Unsupported("EBMLMaxSizeLength > 8".into())),
            _ => {}
        }
    }
    if doc_type != "matroska" && doc_type != "webm" {
        return Err(Error::NotMatroska(format!("DocType {doc_type:?}")));
    }
    // Segment
    let mut pos = hdr_end;
    let seg = loop {
        let h = io.header(pos, len)?.ok_or_else(|| Error::Invalid("no Segment".into()))?;
        if h.id == SEGMENT {
            break h;
        }
        if h.size == Size::Unknown {
            return Err(Error::Invalid("unknown-size element before Segment".into()));
        }
        pos = h.end(len);
    };
    let mut file = MkvFile {
        doc_type,
        doc_type_version: dtv,
        doc_type_read_version: dtrv,
        segment_data_start: seg.data_start(),
        segment_end: seg.end(len),
        live: seg.size == Size::Unknown,
        info: SegmentInfo::default(),
        tracks: Vec::new(),
        seek_head: Vec::new(),
        cues: Vec::new(),
        clusters: Vec::new(),
        chapters: Vec::new(),
        tags: Vec::new(),
        attachments: Vec::new(),
        first_cluster: None,
        indexed: false,
        index_end: None,
        resume: None,
        crc_checked: 0,
        crc_errors: Vec::new(),
        warnings: Vec::new(),
    };
    if seg.overruns(len) {
        warn(&mut file.warnings, "Segment extends past end of file (truncated)".into());
    }
    let mut w = Walker {
        opts,
        parsed: HashMap::new(),
        order: Vec::new(),
        defer_seek_head: saved.is_some(),
        meta_left: len,
        blocks_seen: false,
        resuming: false,
        lookup: None,
        budget: FrameBudget::new(len),
    };
    w.walk(&mut io, &mut file)?;
    if let Some(saved) = saved {
        w.attach(&mut io, &mut file, saved)?;
    }
    finish_tracks(&mut file);
    Ok((file, w.order))
}

struct Walker<'o> {
    opts: &'o OpenOptions,
    /// Start and end of each level-1 element read.
    parsed: HashMap<u64, u64>,
    /// The starts of the level-1 elements read, in the order they were read (what an
    /// [`crate::matroska::IndexStore`] keeps, so that an open from it reads them in the same order).
    order: Vec<u64>,
    /// Headers walked for tables from an [`crate::matroska::IndexStore`]: the SeekHead's targets are
    /// read after the elements the scan read ([`Walker::attach`]), as the scan read them.
    defer_seek_head: bool,
    /// Bytes level-1 elements may still read or scan. Distinct elements do not overlap, so a
    /// valid file stays within its length; SeekHead entries pointing into one another do not.
    meta_left: u64,
    blocks_seen: bool,
    /// Continuing a scan (see [`MkvFile::extend_index`]): keep `file.index_end` and the resume
    /// state current, as a limited open does, so that a failed read can be retried.
    resuming: bool,
    /// Built at the first Cluster after Tracks is parsed.
    lookup: Option<TrackIndex>,
    budget: FrameBudget,
}

impl Walker<'_> {
    fn walk<S: ByteSource + ?Sized>(&mut self, io: &mut Io<'_, S>, file: &mut MkvFile) -> Result<()> {
        let start = file.segment_data_start;
        self.walk_from(io, file, start)
    }

    /// Walk the Segment's children from `start`. With an [`OpenOptions::index_limit`] the walk may
    /// stop early, leaving `file.index_end` and the resume state for [`MkvFile::extend_index`].
    fn walk_from<S: ByteSource + ?Sized>(&mut self, io: &mut Io<'_, S>, file: &mut MkvFile, start: u64) -> Result<()> {
        let result = self.walk_children(io, file, start);
        // Stopped early or failed part-way: hand the elements read so far to the next
        // `extend_index`. `parsed` holds only elements read in full (see `level1`), so it is
        // moved, not copied per element.
        if file.index_end.is_some() {
            file.resume = Some(Resume { parsed: std::mem::take(&mut self.parsed), meta_left: self.meta_left });
        }
        result
    }

    fn walk_children<S: ByteSource + ?Sized>(&mut self, io: &mut Io<'_, S>, file: &mut MkvFile, start: u64) -> Result<()> {
        let seg_end = file.segment_end;
        let limit = self.opts.index_limit.filter(|_| self.opts.index);
        if let Some(l) = limit {
            io.avail = l;
        }
        let mut pos = start;
        let track = limit.is_some() || self.resuming;
        let mut stopped = false;
        while pos < seg_end {
            if track {
                self.save(file, pos);
            }
            // the first Cluster is always scanned; the rest only while they fit below the limit
            // (checked before the header is read: that lies past the limit too)
            if limit.is_some_and(|l| pos >= l) && !file.clusters.is_empty() {
                stopped = true;
                break;
            }
            let Some(h) = io.header(pos, seg_end)? else {
                match io.resync(pos + 1, seg_end)? {
                    Some(p) => {
                        warn(&mut file.warnings, format!("undecodable element at {pos}; resynced at {p}"));
                        pos = p;
                        continue;
                    }
                    None => break,
                }
            };
            if let Some(l) = limit
                && !file.clusters.is_empty()
                && matches!(h.size, Size::Known(_))
                && h.end(seg_end) > l
            {
                stopped = true;
                break;
            }
            match h.id {
                CLUSTER => {
                    if file.first_cluster.is_none() {
                        file.first_cluster = Some(h.pos);
                        // make sure Info/Tracks are known before interpreting blocks
                        self.follow_seek_head(io, file, &[INFO, TRACKS], u64::MAX)?;
                        if !self.opts.index {
                            break;
                        }
                    }
                    let lookup = self.lookup.get_or_insert_with(|| track_index(&file.tracks));
                    let (ci, blocks) = scan_cluster(io, &h, seg_end, lookup, &mut self.budget, &mut file.warnings)?;
                    if self.opts.verify_crc && h.size != Size::Unknown {
                        let data = io.read_vec(h.data_start(), ci.end - h.data_start())?;
                        self.check_crc(file, h.pos, &data);
                    }
                    add_blocks(file, file.clusters.len() as u32, &blocks);
                    self.blocks_seen = true;
                    file.clusters.push(ci);
                    pos = ci.end.max(h.data_start());
                }
                SEEK_HEAD | INFO | TRACKS | CUES | CHAPTERS | TAGS | ATTACHMENTS => {
                    pos = self.level1(io, file, &h, seg_end)?;
                }
                EBML | SEGMENT => break, // chained segment: only the first one is read
                _ => {
                    if h.size == Size::Unknown {
                        pos = io.unknown_end(h.data_start(), seg_end)?;
                    } else if h.overruns(seg_end) && h.id != VOID {
                        match io.resync(pos + 1, seg_end)? {
                            Some(p) => {
                                warn(&mut file.warnings, format!("oversized element 0x{:X} at {pos}; resynced at {p}", h.id));
                                pos = p;
                            }
                            None => break,
                        }
                    } else {
                        pos = h.end(seg_end);
                    }
                }
            }
        }
        if io.cache.recovery_left == 0 {
            warn(&mut file.warnings, "recovery budget spent; the rest of the damaged data was not searched".into());
        }
        if track {
            self.save(file, pos);
        }
        if stopped {
            // what lies after the scanned part (Cues, Tags…) waits for the rest of the index
            let before = file.index_end.unwrap_or(pos);
            self.follow_seek_head(io, file, &[INFO, TRACKS, CUES, CHAPTERS, TAGS, ATTACHMENTS, SEEK_HEAD], before)?;
            file.cues.sort_by_key(|c| c.time);
            file.indexed = false;
            return Ok(());
        }
        if !self.defer_seek_head {
            self.follow_seek_head_to_the_end(io, file)?;
        }
        file.cues.sort_by_key(|c| c.time);
        file.indexed = self.opts.index;
        file.index_end = None;
        file.resume = None;
        Ok(())
    }

    /// Attaches tables saved by an earlier scan (an [`crate::matroska::IndexStore`]) to a file
    /// whose headers were walked: reads the level-1 elements that scan had read (Cues, Chapters,
    /// Tags… after the clusters) in the order it read them and then what the SeekHead points at,
    /// as the scan did, checks the saved tracks and clusters against the file, gives the file the
    /// scan's warnings, and makes it indexed. Tables that do not fit are an error (the caller scans
    /// the file instead).
    fn attach<S: ByteSource + ?Sized>(&mut self, io: &mut Io<'_, S>, file: &mut MkvFile, saved: index_store::Saved) -> Result<()> {
        let bad = || Error::Invalid("saved index does not fit the file".into());
        let seg_end = file.segment_end;
        for position in &saved.elements {
            if self.parsed.contains_key(position) || *position < file.segment_data_start || *position >= seg_end {
                continue;
            }
            if let Some(h) = io.header(*position, seg_end)?
                && matches!(h.id, SEEK_HEAD | INFO | TRACKS | CUES | CHAPTERS | TAGS | ATTACHMENTS)
            {
                self.level1(io, file, &h, seg_end)?;
            }
        }
        self.follow_seek_head_to_the_end(io, file)?;
        if saved.tracks.len() != file.tracks.len() || saved.tracks.iter().zip(&file.tracks).any(|((number, _, _), t)| *number != t.number) {
            return Err(bad());
        }
        let cluster_id = CLUSTER.to_be_bytes();
        for index in index_store::clusters_to_check(saved.clusters.len()) {
            let Some(c) = saved.clusters.get(index) else { return Err(bad()) };
            if c.offset < file.segment_data_start || c.end > seg_end || io.peek(c.offset, 4)? != cluster_id {
                return Err(bad());
            }
        }
        file.clusters = saved.clusters;
        for (track, (_, samples, sync)) in file.tracks.iter_mut().zip(saved.tracks) {
            track.samples = samples;
            track.sync = sync;
        }
        file.first_cluster = file.clusters.first().map(|c| c.offset).or(file.first_cluster);
        // The scan's warnings: damage inside clusters, which this open does not read, included.
        file.warnings = saved.warnings;
        file.cues.sort_by_key(|c| c.time);
        file.indexed = true;
        file.index_end = None;
        file.resume = None;
        Ok(())
    }

    /// Record that everything before `pos` is done: a read that fails from here on (data not
    /// loaded yet) can be retried by [`MkvFile::extend_index`] without repeating any of it.
    fn save(&self, file: &mut MkvFile, pos: u64) {
        file.index_end = Some(pos);
    }

    /// Read every element the SeekHead points at (the last step of a walk through the file).
    fn follow_seek_head_to_the_end<S: ByteSource + ?Sized>(&mut self, io: &mut Io<'_, S>, file: &mut MkvFile) -> Result<()> {
        self.follow_seek_head(io, file, &[INFO, TRACKS, CUES, CHAPTERS, TAGS, ATTACHMENTS, SEEK_HEAD], u64::MAX)?;
        // follow a second-level SeekHead reached through the first one
        self.follow_seek_head(io, file, &[CUES, CHAPTERS, TAGS, ATTACHMENTS], u64::MAX)
    }

    /// Read the elements of the given kinds that the SeekHead points at below `before`.
    fn follow_seek_head<S: ByteSource + ?Sized>(&mut self, io: &mut Io<'_, S>, file: &mut MkvFile, ids: &[u32], before: u64) -> Result<()> {
        let entries = file.seek_head.clone();
        for e in entries {
            if !ids.contains(&e.id) {
                continue;
            }
            let Some(abs) = file.segment_data_start.checked_add(e.position) else { continue };
            if self.parsed.contains_key(&abs) || abs >= file.segment_end || abs >= before {
                continue;
            }
            match io.header(abs, file.segment_end)? {
                Some(h) if h.id == e.id => {
                    self.level1(io, file, &h, file.segment_end)?;
                }
                _ => warn(&mut file.warnings, format!("SeekHead entry 0x{:X} -> {abs} does not point at that element", e.id)),
            }
        }
        Ok(())
    }

    fn check_crc(&self, file: &mut MkvFile, pos: u64, data: &[u8]) {
        if let Some(ok) = ebml::verify_crc(data) {
            file.crc_checked += 1;
            if !ok {
                file.crc_errors.push(pos);
                warn(&mut file.warnings, format!("CRC-32 mismatch in element at {pos}"));
            }
        }
    }

    /// Parse a level-1 metadata element; returns its end. An element whose read fails is left
    /// unrecorded, so a retry reads it again and one that succeeded is never read twice.
    fn level1<S: ByteSource + ?Sized>(&mut self, io: &mut Io<'_, S>, file: &mut MkvFile, h: &Hdr, limit: u64) -> Result<u64> {
        let known = self.parsed.contains_key(&h.pos);
        let (meta_left, attachments) = (self.meta_left, file.attachments.len());
        let result = self.level1_read(io, file, h, limit);
        if result.is_err() && !known {
            self.parsed.remove(&h.pos);
            if self.order.last() == Some(&h.pos) {
                self.order.pop();
            }
            self.meta_left = meta_left;
            file.attachments.truncate(attachments);
        }
        result
    }

    fn level1_read<S: ByteSource + ?Sized>(&mut self, io: &mut Io<'_, S>, file: &mut MkvFile, h: &Hdr, limit: u64) -> Result<u64> {
        if let Some(&end) = self.parsed.get(&h.pos) {
            return Ok(end);
        }
        let end = match h.size {
            Size::Unknown => io.unknown_end(h.data_start(), limit.min(h.data_start().saturating_add(self.meta_left)))?,
            Size::Known(_) => h.end(limit),
        };
        self.parsed.insert(h.pos, end);
        self.order.push(h.pos);
        if h.overruns(limit) {
            warn(&mut file.warnings, format!("element 0x{:X} at {} truncated", h.id, h.pos));
        }
        let n = end - h.data_start();
        if n > self.meta_left || (h.size == Size::Unknown && n == self.meta_left && end < limit) {
            self.meta_left = 0;
            warn(&mut file.warnings, format!("element 0x{:X} at {} overlaps elements already read; skipped", h.id, h.pos));
            return Ok(end);
        }
        self.meta_left -= n;
        if h.id == ATTACHMENTS {
            self.attachments(io, file, h.data_start(), end)?;
            return Ok(end);
        }
        let n = end - h.data_start();
        if n > MAX_META {
            warn(&mut file.warnings, format!("element 0x{:X} at {} too large ({n} bytes); skipped", h.id, h.pos));
            return Ok(end);
        }
        let data = io.read_vec(h.data_start(), n)?;
        if self.opts.verify_crc {
            self.check_crc(file, h.pos, &data);
        }
        match h.id {
            SEEK_HEAD => file.seek_head.extend(parse_seekhead(&data)),
            INFO => file.info = parse_info(&data),
            TRACKS if file.tracks.is_empty() => {
                if Children::new(&data).filter(|e| e.id == TRACK_ENTRY).count() > MAX_TRACKS {
                    return Err(Error::TooLarge("track count"));
                }
                file.tracks = parse_tracks(&data, file.info.timestamp_scale);
                self.lookup = None;
            }
            CUES => parse_cues(&data, &mut file.cues),
            CHAPTERS => parse_chapters(&data, &mut file.chapters),
            TAGS => parse_tags(&data, &mut file.tags),
            _ => {}
        }
        Ok(end)
    }

    fn attachments<S: ByteSource + ?Sized>(&mut self, io: &mut Io<'_, S>, file: &mut MkvFile, start: u64, end: u64) -> Result<()> {
        let mut pos = start;
        while pos < end {
            let Some(h) = io.header(pos, end)? else { break };
            let he = h.end(end);
            if h.id == ATTACHED_FILE {
                let mut a = Attachment::default();
                let mut p = h.data_start();
                while p < he {
                    let Some(c) = io.header(p, he)? else { break };
                    let ce = c.end(he);
                    let n = ce - c.data_start();
                    if c.id == FILE_DATA {
                        a.data_offset = c.data_start();
                        a.data_size = n;
                    } else if n <= 4096 {
                        let d = io.peek(c.data_start(), n as usize)?;
                        match c.id {
                            FILE_NAME => a.name = ebml::string(d),
                            FILE_MEDIA_TYPE => a.media_type = ebml::string(d),
                            FILE_DESCRIPTION => a.description = Some(ebml::string(d)),
                            FILE_UID => a.uid = ebml::uint(d),
                            _ => {}
                        }
                    }
                    p = ce;
                }
                file.attachments.push(a);
            }
            pos = he;
        }
        Ok(())
    }
}

fn add_blocks(file: &mut MkvFile, cluster: u32, blocks: &[BlockRef]) {
    let scale = file.info.timestamp_scale;
    let mut frames: Vec<FrameRef> = Vec::new();
    for b in blocks {
        frames.clear();
        let t = &file.tracks[b.track];
        expand(b, t, scale, 0, &mut frames);
        let t = &mut file.tracks[b.track];
        for f in &frames {
            if f.keyframe {
                t.sync.push(t.samples.len() as u32);
            }
            t.samples.push(Sample {
                offset: f.offset,
                size: f.size,
                pts: f.pts,
                duration: f.duration,
                keyframe: f.keyframe,
                cluster,
                block_offset: f.block_offset,
                lace: f.lace,
                addition: f.addition,
            });
        }
    }
}

fn finish_tracks(file: &mut MkvFile) {
    let scale = file.info.timestamp_scale;
    let g = gcd(scale, 1_000_000_000);
    for t in &mut file.tracks {
        t.timebase = (scale / g, 1_000_000_000 / g);
    }
}

impl MkvFile {
    /// True for WebM (`DocType` = `webm`).
    pub fn is_webm(&self) -> bool {
        self.doc_type == "webm"
    }

    /// Segment duration in ns: `Info/Duration` if present, else the end of the last indexed sample.
    pub fn duration_ns(&self) -> Option<u64> {
        if let Some(d) = self.info.duration {
            return Some((d * self.info.timestamp_scale as f64).round() as u64);
        }
        let end = self.tracks.iter().flat_map(|t| t.samples.iter().map(|s| s.pts.saturating_add(s.duration.min(i64::MAX as u64) as i64))).max()?;
        Some((end.max(0) as u64).saturating_mul(self.info.timestamp_scale))
    }

    /// Continue a scan that an [`OpenOptions::index_limit`] stopped, from [`MkvFile::index_end`],
    /// without rescanning what is indexed. `opts.index_limit` bounds the new scan like at open
    /// (`None`: to the end of the Segment, which completes the index: Cues, Tags… are read and
    /// [`MkvFile::indexed`] becomes `true`); `opts.verify_crc` applies as at open.
    ///
    /// A no-op when there is nothing left to scan, including a limit at or below `index_end`.
    /// Safe to call again after an error such as a byte source that has not loaded the range yet
    /// (`WouldBlock`): the clusters scanned before the failure stay indexed and `index_end` says
    /// where the next call continues.
    pub fn extend_index<S: ByteSource + ?Sized>(&mut self, src: &S, opts: &OpenOptions) -> Result<()> {
        let Some(start) = self.index_end else { return Ok(()) };
        let resume = self.resume.take().unwrap_or_default();
        let opts = OpenOptions { index: true, ..opts.clone() };
        let mut cache = Cache::default();
        let mut io = Io::new(src, &mut cache);
        let budget = FrameBudget::resumed(io.len, &self.tracks);
        let mut w = Walker {
            opts: &opts,
            parsed: resume.parsed,
            order: Vec::new(),
            defer_seek_head: false,
            meta_left: resume.meta_left,
            blocks_seen: true,
            resuming: true,
            lookup: None,
            budget,
        };
        w.walk_from(&mut io, self, start)
    }

    /// Index of the track with this `TrackNumber`.
    pub fn track_by_number(&self, number: u64) -> Option<usize> {
        self.tracks.iter().position(|t| t.number == number)
    }

    /// Index of the first track of the given kind (preferring the default flag).
    pub fn track_of_kind(&self, kind: TrackKind) -> Option<usize> {
        let mut it = self.tracks.iter().enumerate().filter(|(_, t)| t.kind == kind);
        let first = it.clone().next().map(|x| x.0);
        it.find(|(_, t)| t.default).map(|x| x.0).or(first)
    }

    /// Read sample `index` of `track` (header stripping undone).
    pub fn read_sample<S: ByteSource + ?Sized>(&self, src: &S, track: usize, index: usize) -> Result<Vec<u8>> {
        let t = self.tracks.get(track).ok_or(Error::NoSuchTrack(track))?;
        let s = t.samples.get(index).ok_or(Error::NoSuchSample(index))?;
        if !t.frames_readable() {
            return Err(Error::Unsupported(format!("content encoding of track {} (compression/encryption)", t.number)));
        }
        let mut data = t.frame_prefix(s.size as u64);
        let at = data.len();
        data.resize(at + s.size as usize, 0);
        src.read_at(s.offset, &mut data[at..])?;
        Ok(data)
    }

    /// The `BlockAdditional` (ID 1) of sample `index` of `track`, `None` when the block has none
    /// (see [`Sample::addition`]). Stored as is: header stripping applies to frames only.
    pub fn read_sample_addition<S: ByteSource + ?Sized>(&self, src: &S, track: usize, index: usize) -> Result<Option<Vec<u8>>> {
        let t = self.tracks.get(track).ok_or(Error::NoSuchTrack(track))?;
        let s = t.samples.get(index).ok_or(Error::NoSuchSample(index))?;
        let Some((offset, size)) = s.addition else { return Ok(None) };
        if offset.checked_add(size as u64).is_none_or(|end| end > src.len()) {
            return Err(Error::Invalid("block addition past end of file".into()));
        }
        let mut data = vec![0; size as usize];
        src.read_at(offset, &mut data)?;
        Ok(Some(data))
    }

    /// Keyframe sample of `track` with the greatest pts ≤ `time_ns` (indexed files only).
    pub fn keyframe_before(&self, track: usize, time_ns: i64) -> Option<usize> {
        let t = self.tracks.get(track)?;
        t.keyframe_before_pts(t.ns_to_ticks(time_ns))
    }
}
