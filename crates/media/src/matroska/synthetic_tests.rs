//! Hand-built files exercising features FFmpeg does not write: unknown-size Segment/Clusters,
//! header stripping, BlockGroups, Colour + mastering metadata, Chapters, Tags, Attachments, CRC-32.

use crate::matroska::codec::Codec;
use crate::matroska::ebml::{self, write_id, write_unknown_size};
use crate::matroska::ids::*;
use crate::matroska::mux::{el, el_float, el_str, el_uint};
use crate::matroska::track::ContentEncoding;
use crate::matroska::{ByteSource, Demuxer, MkvFile, OpenOptions, Packet, open_with};

mod hostile;

fn block(track: u8, rel: i16, flags: u8, data: &[u8]) -> Vec<u8> {
    let mut b = vec![0x80 | track];
    b.extend_from_slice(&rel.to_be_bytes());
    b.push(flags);
    b.extend_from_slice(data);
    b
}

fn synthetic() -> Vec<u8> {
    let mut f = Vec::new();
    let mut h = Vec::new();
    el_str(&mut h, DOC_TYPE, "matroska");
    el_uint(&mut h, DOC_TYPE_VERSION, 4);
    el(&mut f, EBML, &h);
    write_id(&mut f, SEGMENT);
    write_unknown_size(&mut f, 8);
    // Info with a correct CRC-32 and a Void
    let mut info = Vec::new();
    el_uint(&mut info, TIMESTAMP_SCALE, 1_000_000);
    el_str(&mut info, TITLE, "Synthetic");
    el(&mut info, VOID, &[0; 3]);
    let mut crc_info = Vec::new();
    el(&mut crc_info, CRC32, &ebml::crc32(&info).to_le_bytes());
    crc_info.extend_from_slice(&info);
    el(&mut f, INFO, &crc_info);
    // Tracks
    let mut v = Vec::new();
    el_uint(&mut v, TRACK_NUMBER, 1);
    el_uint(&mut v, TRACK_UID, 11);
    el_uint(&mut v, TRACK_TYPE, 1);
    el_str(&mut v, CODEC_ID, "V_MPEG4/ISO/AVC");
    el(&mut v, CODEC_PRIVATE, &[1, 0x64, 0, 0x1F]);
    el_uint(&mut v, DEFAULT_DURATION, 40_000_000);
    let mut mm = Vec::new();
    for (id, val) in
        [(PRIMARY_R_X, 0.708), (PRIMARY_R_Y, 0.292), (PRIMARY_G_X, 0.17), (PRIMARY_G_Y, 0.797), (PRIMARY_B_X, 0.131), (PRIMARY_B_Y, 0.046)]
    {
        el_float(&mut mm, id, val);
    }
    el_float(&mut mm, WHITE_POINT_X, 0.3127);
    el_float(&mut mm, WHITE_POINT_Y, 0.329);
    el_float(&mut mm, LUMINANCE_MAX, 1000.0);
    el_float(&mut mm, LUMINANCE_MIN, 0.0001);
    let mut col = Vec::new();
    el_uint(&mut col, MATRIX_COEFFICIENTS, 9);
    el_uint(&mut col, RANGE, 2);
    el_uint(&mut col, TRANSFER_CHARACTERISTICS, 16);
    el_uint(&mut col, PRIMARIES, 9);
    el_uint(&mut col, MAX_CLL, 1000);
    el_uint(&mut col, MAX_FALL, 400);
    el(&mut col, MASTERING_METADATA, &mm);
    let mut vid = Vec::new();
    el_uint(&mut vid, PIXEL_WIDTH, 1440);
    el_uint(&mut vid, PIXEL_HEIGHT, 1080);
    el_uint(&mut vid, DISPLAY_WIDTH, 1920);
    el_uint(&mut vid, DISPLAY_HEIGHT, 1080);
    el(&mut vid, COLOUR, &col);
    // rectangular, a quarter turn clockwise (portrait phone video)
    let mut proj = Vec::new();
    el_uint(&mut proj, PROJECTION_TYPE, 0);
    el_float(&mut proj, PROJECTION_POSE_ROLL, -90.0);
    el(&mut vid, PROJECTION, &proj);
    el(&mut v, VIDEO, &vid);
    // header stripping: every frame starts with 00 00 00 01
    let mut comp = Vec::new();
    el_uint(&mut comp, CONTENT_COMP_ALGO, 3);
    el(&mut comp, CONTENT_COMP_SETTINGS, &[0, 0, 0, 1]);
    let mut enc = Vec::new();
    el(&mut enc, CONTENT_COMPRESSION, &comp);
    let mut encs = Vec::new();
    el(&mut encs, CONTENT_ENCODING, &enc);
    el(&mut v, CONTENT_ENCODINGS, &encs);
    let mut a = Vec::new();
    el_uint(&mut a, TRACK_NUMBER, 2);
    el_uint(&mut a, TRACK_TYPE, 2);
    el_str(&mut a, CODEC_ID, "A_AAC/MPEG4/LC/SBR");
    el_str(&mut a, LANGUAGE, "ger");
    let mut au = Vec::new();
    el_float(&mut au, SAMPLING_FREQUENCY, 24000.0);
    el_float(&mut au, OUTPUT_SAMPLING_FREQUENCY, 48000.0);
    el_uint(&mut au, CHANNELS, 2);
    el(&mut a, AUDIO, &au);
    let mut tr = Vec::new();
    el(&mut tr, TRACK_ENTRY, &v);
    el(&mut tr, TRACK_ENTRY, &a);
    el(&mut f, TRACKS, &tr);
    // Chapters
    let mut disp = Vec::new();
    el_str(&mut disp, CHAP_STRING, "Intro");
    let mut atom = Vec::new();
    el_uint(&mut atom, CHAPTER_UID, 5);
    el_uint(&mut atom, CHAPTER_TIME_START, 0);
    el_uint(&mut atom, CHAPTER_TIME_END, 40_000_000);
    el(&mut atom, CHAPTER_DISPLAY, &disp);
    let mut ed = Vec::new();
    el(&mut ed, CHAPTER_ATOM, &atom);
    let mut ch = Vec::new();
    el(&mut ch, EDITION_ENTRY, &ed);
    el(&mut f, CHAPTERS, &ch);
    // Attachments
    let mut af = Vec::new();
    el_str(&mut af, FILE_NAME, "font.ttf");
    el_str(&mut af, FILE_MEDIA_TYPE, "font/ttf");
    el_uint(&mut af, FILE_UID, 77);
    el(&mut af, FILE_DATA, &[9; 100]);
    let mut att = Vec::new();
    el(&mut att, ATTACHED_FILE, &af);
    el(&mut f, ATTACHMENTS, &att);
    // Cluster 1 (unknown size): keyframe SimpleBlock, BlockGroup with ReferenceBlock + duration
    write_id(&mut f, CLUSTER);
    write_unknown_size(&mut f, 1);
    el_uint(&mut f, TIMESTAMP, 1000);
    el(&mut f, SIMPLE_BLOCK, &block(1, 0, 0x80, &[0x65, 1, 2]));
    el(&mut f, SIMPLE_BLOCK, &block(2, 5, 0x80, &[0xAA; 10]));
    let mut g = Vec::new();
    el(&mut g, BLOCK, &block(1, 40, 0, &[0x41, 3]));
    el(&mut g, REFERENCE_BLOCK, &[0xD8]);
    el_uint(&mut g, BLOCK_DURATION, 80);
    el(&mut f, BLOCK_GROUP, &g);
    el(&mut f, SIMPLE_BLOCK, &block(9, 0, 0x80, &[1])); // unknown track: ignored
    // Cluster 2 (unknown size), ended by Tags
    write_id(&mut f, CLUSTER);
    write_unknown_size(&mut f, 8);
    el_uint(&mut f, TIMESTAMP, 1120);
    el(&mut f, SIMPLE_BLOCK, &block(1, 0, 0x80, &[0x65, 4]));
    let mut st = Vec::new();
    el_str(&mut st, TAG_NAME, "ARTIST");
    el_str(&mut st, TAG_STRING, "Nobody");
    let mut tg = Vec::new();
    let mut tgt = Vec::new();
    el_uint(&mut tgt, TAG_TRACK_UID, 11);
    el(&mut tg, TARGETS, &tgt);
    el(&mut tg, SIMPLE_TAG, &st);
    let mut tags = Vec::new();
    el(&mut tags, TAG, &tg);
    el(&mut f, TAGS, &tags);
    f
}

fn assert_synthetic_metadata(f: &MkvFile, bytes: &[u8]) {
    assert!(f.live);
    assert!(f.warnings.is_empty(), "{:?}", f.warnings);
    assert_eq!((f.crc_checked, f.crc_errors.len()), (1, 0));
    assert_eq!(f.info.title.as_deref(), Some("Synthetic"));
    let video = &f.tracks[0];
    assert!(matches!(&video.codec, Codec::Avc { avcc } if avcc[1] == 0x64));
    let info = video.video.as_ref().unwrap();
    assert_eq!(info.pixel_aspect(), (4, 3));
    assert_eq!(info.projection.as_ref().map(|projection| projection.roll), Some(-90.0));
    assert_eq!(info.display_rotation(), Some(1));
    let color = info.color.as_ref().unwrap();
    assert!(color.full_range());
    assert_eq!(
        (color.matrix_coefficients, color.transfer_characteristics, color.primaries, color.max_cll, color.max_fall),
        (Some(9), Some(16), Some(9), Some(1000), Some(400))
    );
    let mastering = color.mastering.as_ref().unwrap();
    assert_eq!(mastering.primaries[1], (0.17, 0.797));
    assert_eq!(mastering.white_point, (0.3127, 0.329));
    assert_eq!((mastering.luminance_max, mastering.luminance_min), (1000.0, 0.0001));
    assert_eq!(video.encodings, vec![ContentEncoding::HeaderStripping { bytes: vec![0, 0, 0, 1], scope: 1 }]);
    // Legacy AAC ID synthesizes explicit SBR signaling (LC, 24 kHz, stereo to 48 kHz).
    assert_eq!(f.tracks[1].codec, Codec::Aac { asc: vec![0x13, 0x10, 0x56, 0xE5, 0x98] });
    assert_eq!(f.tracks[1].language, "ger");
    assert_eq!(f.chapters[0].chapters[0].title(), Some("Intro"));
    assert_eq!(f.chapters[0].chapters[0].end_ns, Some(40_000_000));
    assert_eq!(f.attachments.len(), 1);
    let attachment = &f.attachments[0];
    assert_eq!((attachment.name.as_str(), attachment.media_type.as_str(), attachment.uid), ("font.ttf", "font/ttf", 77));
    assert_eq!(&bytes[attachment.data_offset as usize..][..100], &[9; 100]);
    assert_eq!(f.tags[0].targets.track_uids, vec![11]);
    assert_eq!(f.tags[0].simple[0].string.as_deref(), Some("Nobody"));
    assert_eq!(f.clusters.len(), 2);
    assert_eq!(video.samples.len(), 3);
    assert_eq!(video.keyframes(), &[0, 2]);
}

fn assert_synthetic_packets(f: &MkvFile, bytes: &[u8]) {
    let packets: Vec<Packet> = Demuxer::from_slice(bytes).unwrap().map(|packet| packet.unwrap()).collect();
    assert_eq!(packets.len(), 4);
    assert_eq!((packets[0].track, packets[0].pts, packets[0].keyframe, packets[0].duration), (0, 1000, true, 40));
    assert_eq!(packets[0].data, [0, 0, 0, 1, 0x65, 1, 2]);
    assert_eq!((packets[1].track, packets[1].pts, packets[1].data.len()), (1, 1005, 10));
    assert_eq!((packets[2].pts, packets[2].keyframe, packets[2].duration), (1040, false, 80));
    assert_eq!(packets[2].data, [0, 0, 0, 1, 0x41, 3]);
    assert_eq!((packets[3].pts, packets[3].pts_ns), (1120, 1_120_000_000));
    assert_eq!(f.read_sample(bytes, 0, 1).unwrap(), packets[2].data);
}

fn assert_crc_mismatch_is_reported(bytes: &[u8]) {
    let mut corrupted = bytes.to_vec();
    let title = corrupted.windows(9).position(|window| window == b"Synthetic").unwrap();
    corrupted[title] = b's';
    let file = open_with(&corrupted[..], &OpenOptions { index: true, verify_crc: true, index_limit: None }).unwrap();
    assert_eq!(file.crc_errors.len(), 1);
}

fn assert_seek_builds_an_index(bytes: &[u8]) {
    let mut demuxer = Demuxer::with_options(bytes, OpenOptions { index: false, verify_crc: false, index_limit: None }).unwrap();
    assert_eq!(demuxer.file().tracks.len(), 2);
    assert_eq!(demuxer.seek(0, 1_100_000_000).unwrap().pts, 1000);
    assert_eq!(demuxer.seek(0, 1_130_000_000).unwrap().pts, 1120);
    assert_eq!(demuxer.next_packet().unwrap().unwrap().pts, 1120);
    assert!(demuxer.file().indexed);
}

#[test]
fn synthetic_file() {
    let bytes = synthetic();
    let file = open_with(&bytes[..], &OpenOptions { index: true, verify_crc: true, index_limit: None }).unwrap();
    assert_synthetic_metadata(&file, &bytes);
    assert_synthetic_packets(&file, &bytes);
    assert_crc_mismatch_is_reported(&bytes);
    assert_seek_builds_an_index(&bytes);
}

/// An in-memory [`crate::matroska::IndexStore`] that counts what it is asked for one key, and
/// serves only the thread that made it: other tests open files, the same one too, while it is
/// registered, and they neither find nor save anything in it.
struct CountingStore {
    key: String,
    thread: std::thread::ThreadId,
    kept: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
    loads: std::sync::atomic::AtomicUsize,
    saves: std::sync::atomic::AtomicUsize,
}

impl CountingStore {
    fn new(key: String) -> Self {
        CountingStore {
            key,
            thread: std::thread::current().id(),
            kept: std::sync::Mutex::default(),
            loads: std::sync::atomic::AtomicUsize::new(0),
            saves: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn counts(&self) -> (usize, usize) {
        (self.loads.load(std::sync::atomic::Ordering::SeqCst), self.saves.load(std::sync::atomic::Ordering::SeqCst))
    }
}

impl crate::matroska::IndexStore for CountingStore {
    fn load(&self, key: &str) -> Option<Vec<u8>> {
        if std::thread::current().id() != self.thread {
            return None;
        }
        if key == self.key {
            self.loads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        self.kept.lock().unwrap().get(key).cloned()
    }
    fn save(&self, key: &str, index: &[u8]) {
        if std::thread::current().id() != self.thread {
            return;
        }
        if key == self.key {
            self.saves.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        self.kept.lock().unwrap().insert(key.to_owned(), index.to_vec());
    }
}

/// A store registered for as long as this lives: dropping it (at the end of a scope, or when an
/// assertion fails) registers again the store that was registered before.
struct Registered(Option<std::sync::Arc<dyn crate::matroska::IndexStore>>);

impl Registered {
    fn new(store: std::sync::Arc<dyn crate::matroska::IndexStore>) -> Self {
        let before = crate::matroska::index_store::index_store();
        crate::matroska::set_index_store(Some(store));
        Registered(before)
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        crate::matroska::set_index_store(self.0.take());
    }
}

/// The parts of an open file the scan builds or reads after the clusters.
fn scanned(f: &MkvFile) -> String {
    let tracks: Vec<_> = f.tracks.iter().map(|t| (t.number, t.samples.clone(), t.keyframes().to_vec())).collect();
    format!("{:?} {:?} {:?} {:?} {:?} {:?} {:?} {}", f.clusters, tracks, f.cues, f.chapters, f.tags, f.attachments, f.first_cluster, f.indexed)
}

/// A file's sample tables are saved to the registered store after a scan and taken from it at the
/// next open, which then gives the same file (its Chapters and Attachments before the clusters,
/// its Tags after them, in an unknown-size Segment). Tables that do not fit the file, or that were
/// damaged in the store, are not used: it is scanned again (and saved again). Elements after the
/// clusters are read in the scan's order, not first those the SeekHead points at, and the scan's
/// warnings come back with the tables.
///
/// (One test, as the store is registered for the whole process.)
#[test]
fn an_index_store_keeps_the_scan() {
    let bytes = synthetic();
    let plain = OpenOptions { index: true, verify_crc: false, index_limit: None };
    let reference = open_with(&bytes[..], &plain).unwrap();
    let key = crate::matroska::index_store::key_of(&bytes[..]).unwrap();
    let store = std::sync::Arc::new(CountingStore::new(key.clone()));
    let registered = Registered::new(store.clone());
    let counts = || store.counts();
    // Scanned and saved.
    let first = open_with(&bytes[..], &plain).unwrap();
    assert_eq!(counts(), (1, 1));
    // Taken from the store: the same file.
    let second = open_with(&bytes[..], &plain).unwrap();
    assert_eq!(counts(), (2, 1));
    assert_eq!(scanned(&first), scanned(&reference));
    assert_eq!(scanned(&second), scanned(&reference));
    assert_eq!(second.info.title.as_deref(), Some("Synthetic"));
    // A Demuxer opens through the store too (the packets are read through one).
    assert_synthetic_packets(&second, &bytes);
    assert_eq!(counts(), (3, 1));
    let demuxer = Demuxer::new(&bytes[..]).unwrap();
    assert_eq!(scanned(demuxer.file()), scanned(&reference));
    assert_eq!(counts(), (4, 1));
    // Saved clusters that are not Clusters in the file (an index saved whole, its checksum
    // matching): scanned again, and saved again.
    {
        let mut kept = store.kept.lock().unwrap();
        let saved = kept.get_mut(&key).unwrap();
        // The first saved cluster's offset (after the magic, the length and the cluster count).
        let at = 8 + 8 + 8;
        let offset = u64::from_le_bytes(saved[at..at + 8].try_into().unwrap());
        saved[at..at + 8].copy_from_slice(&(offset - 1).to_le_bytes());
        saved.truncate(saved.len() - 8);
        crate::matroska::index_store::seal(saved);
    }
    let third = open_with(&bytes[..], &plain).unwrap();
    assert_eq!(scanned(&third), scanned(&reference));
    assert_eq!(counts(), (5, 2));
    // An index damaged in the store (its checksum does not match): scanned again, and saved again.
    {
        let mut kept = store.kept.lock().unwrap();
        let saved = kept.get_mut(&key).unwrap();
        let middle = saved.len() / 2;
        saved[middle] ^= 0x10;
    }
    let fourth = open_with(&bytes[..], &plain).unwrap();
    assert_eq!(scanned(&fourth), scanned(&reference));
    assert_eq!(counts(), (6, 3));
    // With CRC checks or an index limit the store is not used.
    let _ = open_with(&bytes[..], &OpenOptions { index: true, verify_crc: true, index_limit: None }).unwrap();
    assert_eq!(counts(), (6, 3));
    drop(registered);
    the_store_keeps_the_order_of_elements_after_the_clusters();
    the_store_keeps_the_scan_warnings();
}

/// Two Tags after the clusters, the SeekHead pointing at the second only: a scan reads them in the
/// file's order, and so does an open from the store (not the SeekHead's target first).
fn the_store_keeps_the_order_of_elements_after_the_clusters() {
    let tags = |value: &str| {
        let mut st = Vec::new();
        el_str(&mut st, TAG_NAME, "TITLE");
        el_str(&mut st, TAG_STRING, value);
        let mut tg = Vec::new();
        el(&mut tg, SIMPLE_TAG, &st);
        let mut tags = Vec::new();
        el(&mut tags, TAG, &tg);
        let mut o = Vec::new();
        el(&mut o, TAGS, &tags);
        o
    };
    let seek_head = |position: u64| {
        let mut s = Vec::new();
        el(&mut s, SEEK_ID, &TAGS.to_be_bytes());
        el(&mut s, SEEK_POSITION, &position.to_be_bytes());
        let mut entry = Vec::new();
        el(&mut entry, SEEK, &s);
        let mut o = Vec::new();
        el(&mut o, SEEK_HEAD, &entry);
        o
    };
    let mut info = Vec::new();
    el_uint(&mut info, TIMESTAMP_SCALE, 1_000_000);
    let mut v = Vec::new();
    el_uint(&mut v, TRACK_NUMBER, 1);
    el_uint(&mut v, TRACK_TYPE, 1);
    el_str(&mut v, CODEC_ID, "V_MPEG4/ISO/AVC");
    let mut tracks = Vec::new();
    el(&mut tracks, TRACK_ENTRY, &v);
    let mut cluster = Vec::new();
    el_uint(&mut cluster, TIMESTAMP, 0);
    el(&mut cluster, SIMPLE_BLOCK, &block(1, 0, 0x80, &[0x65, 1]));
    let mut middle = Vec::new();
    el(&mut middle, INFO, &info);
    el(&mut middle, TRACKS, &tracks);
    el(&mut middle, CLUSTER, &cluster);
    middle.extend_from_slice(&tags("A"));
    // The SeekHead's position of the second Tags (from the Segment's data).
    let second = (seek_head(0).len() + middle.len()) as u64;
    let mut f = Vec::new();
    let mut h = Vec::new();
    el_str(&mut h, DOC_TYPE, "matroska");
    el(&mut f, EBML, &h);
    write_id(&mut f, SEGMENT);
    write_unknown_size(&mut f, 8);
    f.extend_from_slice(&seek_head(second));
    f.extend_from_slice(&middle);
    f.extend_from_slice(&tags("B"));

    let plain = OpenOptions { index: true, verify_crc: false, index_limit: None };
    let reference = open_with(&f[..], &plain).unwrap();
    let values: Vec<String> = reference.tags.iter().map(|t| format!("{t:?}")).collect();
    assert_eq!(values.len(), 2, "{values:?}");
    assert!(values[0].contains("\"A\"") && values[1].contains("\"B\""), "{values:?}");
    let store = std::sync::Arc::new(CountingStore::new(crate::matroska::index_store::key_of(&f[..]).unwrap()));
    let registered = Registered::new(store.clone());
    let scan = open_with(&f[..], &plain).unwrap();
    let from_store = open_with(&f[..], &plain).unwrap();
    drop(registered);
    assert_eq!(store.counts(), (2, 1));
    assert_eq!(scanned(&scan), scanned(&reference));
    assert_eq!(scanned(&from_store), scanned(&reference));
}

/// A file whose first Cluster holds an element running past its end: the scan warns about it, and
/// an open from the store (which does not read the clusters) gives the same warnings.
fn the_store_keeps_the_scan_warnings() {
    let mut info = Vec::new();
    el_uint(&mut info, TIMESTAMP_SCALE, 1_000_000);
    let mut v = Vec::new();
    el_uint(&mut v, TRACK_NUMBER, 1);
    el_uint(&mut v, TRACK_TYPE, 1);
    el_str(&mut v, CODEC_ID, "V_MPEG4/ISO/AVC");
    let mut tracks = Vec::new();
    el(&mut tracks, TRACK_ENTRY, &v);
    let mut damaged = Vec::new();
    el_uint(&mut damaged, TIMESTAMP, 0);
    el(&mut damaged, SIMPLE_BLOCK, &block(1, 0, 0x80, &[0x65, 1]));
    // A Void that says it is 100 bytes long, with 2 bytes left in the Cluster.
    write_id(&mut damaged, VOID);
    ebml::write_size(&mut damaged, 100, 1);
    damaged.extend_from_slice(&[0, 0]);
    let mut f = Vec::new();
    let mut h = Vec::new();
    el_str(&mut h, DOC_TYPE, "matroska");
    el(&mut f, EBML, &h);
    write_id(&mut f, SEGMENT);
    write_unknown_size(&mut f, 8);
    el(&mut f, INFO, &info);
    el(&mut f, TRACKS, &tracks);
    el(&mut f, CLUSTER, &damaged);
    f.extend_from_slice(&one_cluster_body(40));

    let plain = OpenOptions { index: true, verify_crc: false, index_limit: None };
    let reference = open_with(&f[..], &plain).unwrap();
    assert!(reference.warnings.iter().any(|w| w.starts_with("damaged cluster")), "{:?}", reference.warnings);
    assert_eq!(reference.clusters.len(), 2);
    let store = std::sync::Arc::new(CountingStore::new(crate::matroska::index_store::key_of(&f[..]).unwrap()));
    let registered = Registered::new(store.clone());
    let _ = open_with(&f[..], &plain).unwrap();
    let from_store = open_with(&f[..], &plain).unwrap();
    drop(registered);
    assert_eq!(store.counts(), (2, 1));
    assert_eq!(from_store.warnings, reference.warnings);
    assert_eq!(scanned(&from_store), scanned(&reference));
}

/// A Cluster at timestamp `ts` with one keyframe of track 1.
fn one_cluster_body(ts: u64) -> Vec<u8> {
    let mut c = Vec::new();
    el_uint(&mut c, TIMESTAMP, ts);
    el(&mut c, SIMPLE_BLOCK, &block(1, 0, 0x80, &[0x65, 2]));
    let mut o = Vec::new();
    el(&mut o, CLUSTER, &c);
    o
}

/// One video track and one cluster at timestamp `ts` holding `content`.
fn one_cluster(ts: u64, content: &[u8]) -> Vec<u8> {
    let mut f = Vec::new();
    let mut h = Vec::new();
    el_str(&mut h, DOC_TYPE, "matroska");
    el(&mut f, EBML, &h);
    write_id(&mut f, SEGMENT);
    write_unknown_size(&mut f, 8);
    let mut info = Vec::new();
    el_uint(&mut info, TIMESTAMP_SCALE, 1_000_000);
    el(&mut f, INFO, &info);
    let mut v = Vec::new();
    el_uint(&mut v, TRACK_NUMBER, 1);
    el_uint(&mut v, TRACK_UID, 1);
    el_uint(&mut v, TRACK_TYPE, 1);
    el_str(&mut v, CODEC_ID, "V_VP9");
    let mut tr = Vec::new();
    el(&mut tr, TRACK_ENTRY, &v);
    el(&mut f, TRACKS, &tr);
    write_id(&mut f, CLUSTER);
    write_unknown_size(&mut f, 8);
    el_uint(&mut f, TIMESTAMP, ts);
    f.extend_from_slice(content);
    f
}

/// Counts the bytes read from `data`.
struct Counting<'a>(&'a [u8], std::cell::Cell<u64>);

impl ByteSource for Counting<'_> {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        self.1.set(self.1.get() + buf.len() as u64);
        self.0.read_at(offset, buf)
    }
}

/// Fake Cluster candidates every 16 bytes: a Cluster of 512 KiB whose first child is a 64 KiB Void, so
/// `plausible` has to look 64 KiB away from each candidate and then finds a Cluster ID, not a Timestamp.
fn dense_fake_clusters(total: usize) -> Vec<u8> {
    let unit = fake_cluster_unit();
    assert_eq!(unit.len(), 16);
    let mut f = Vec::new();
    let mut h = Vec::new();
    el_str(&mut h, DOC_TYPE, "matroska");
    el(&mut f, EBML, &h);
    write_id(&mut f, SEGMENT);
    write_unknown_size(&mut f, 8);
    // an undecodable first Segment child forces a resync into the fake candidates
    f.push(0x00);
    while f.len() < total {
        f.extend_from_slice(&unit);
    }
    f
}

#[test]
fn resync_over_dense_fake_clusters_is_bounded_by_the_recovery_budget() {
    let f = dense_fake_clusters(1 << 20);
    let src = Counting(&f, std::cell::Cell::new(0));
    let _ = open_with(&src, &OpenOptions::default());
    let read = src.1.get();
    // 64 MiB of recovery work plus the one candidate that overdraws it; unbounded this is gigabytes
    assert!(read <= (64 << 20) + (1 << 20), "read {read} bytes of a {}-byte file", f.len());
}

/// `prefix` followed by zeros up to `len` bytes, without holding them.
struct Sparse {
    prefix: Vec<u8>,
    len: u64,
}

impl ByteSource for Sparse {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        if offset.checked_add(buf.len() as u64).is_none_or(|e| e > self.len) {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        if buf.len() > 64 << 20 {
            return Err(std::io::Error::other("a read this large is the bug under test"));
        }
        for (i, b) in buf.iter_mut().enumerate() {
            *b = self.prefix.get((offset as usize).saturating_add(i)).copied().unwrap_or(0);
        }
        Ok(())
    }
}

#[test]
fn a_block_larger_than_u32_is_skipped_without_reading_its_body() {
    // flags 0x00 / 0x02 / 0x06 / 0x04: no lacing, Xiph, EBML and fixed-size lacing
    for flags in [0x80u8, 0x82, 0x86, 0x84] {
        let frame = (1u64 << 32) + 100;
        let mut hdr = Vec::new();
        write_id(&mut hdr, SIMPLE_BLOCK);
        ebml::write_size(&mut hdr, 4 + frame, 8);
        hdr.extend_from_slice(&[0x81, 0, 0, flags]);
        let prefix = one_cluster(0, &hdr);
        let len = prefix.len() as u64 + frame;
        let f = open_with(Sparse { prefix, len }, &OpenOptions::default());
        let f = f.unwrap_or_else(|e| panic!("flags {flags:#x}: {e:?}"));
        assert!(f.tracks[0].samples.is_empty(), "flags {flags:#x}: {:?}", f.tracks[0].samples);
    }
}

/// One fake Cluster candidate: it costs the resync two window refills, about 128 KiB.
fn fake_cluster_unit() -> Vec<u8> {
    let mut unit = Vec::new();
    write_id(&mut unit, CLUSTER);
    ebml::write_size(&mut unit, 512 << 10, 8);
    write_id(&mut unit, VOID);
    ebml::write_size(&mut unit, 64 << 10, 3);
    unit
}

#[test]
fn packet_reading_keeps_its_recovery_budget_after_the_index_is_built() {
    // a Cluster, about 40 MiB of recovery reads of damage, then a second Cluster
    let mut f = one_cluster(0, &{
        let mut b = Vec::new();
        el(&mut b, SIMPLE_BLOCK, &block(1, 0, 0x80, &[1]));
        b
    });
    f.push(0x00);
    for _ in 0..320 {
        f.extend_from_slice(&fake_cluster_unit());
    }
    f.resize(f.len() + (700 << 10), 0);
    write_id(&mut f, CLUSTER);
    write_unknown_size(&mut f, 8);
    el_uint(&mut f, TIMESTAMP, 10);
    el(&mut f, SIMPLE_BLOCK, &block(1, 0, 0x80, &[2]));
    let mut d = Demuxer::with_options(&f[..], OpenOptions { index: false, verify_crc: false, index_limit: None }).unwrap();
    d.build_index().unwrap();
    assert_eq!(d.file().tracks[0].samples.len(), 2, "the index found both clusters: {:?}", d.file().warnings);
    let mut n = 0;
    while d.next_packet().unwrap().is_some() {
        n += 1;
    }
    assert_eq!(n, 2, "reading ended at the damage: {:?}", d.file().warnings);
}
