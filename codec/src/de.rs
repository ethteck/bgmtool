use std::io::{self, prelude::*, SeekFrom};
use std::fmt;
use std::collections::{HashMap, btree_map::{BTreeMap, Entry}};
use std::rc::{Rc, Weak};
use smallvec::smallvec;
use log::{debug, warn};
use crate::*;

#[derive(Debug)]
pub enum Error {
    InvalidMagic,
    InvalidName(tinystr::Error),
    SizeMismatch { true_size: u32, internal_size: u32 },
    InvalidNumSegments(u8),
    NonZeroPadding, // TODO: add { position: u64 }
    Io(io::Error),
}

impl From<io::Error> for Error {
    fn from(io: io::Error) -> Self {
        Self::Io(io)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidMagic => write!(f, "Missing 'BGM' signature at start of file"),
            // TODO(blocked): output source error too; see https://github.com/zbraniecki/tinystr/issues/29
            Error::InvalidName(_source) => write!(f, "Invalid name string, must be 4 ASCII bytes"),
            Error::SizeMismatch {
                true_size, internal_size,
            } => write!(f, "The file says it is {}B, but it is actually {}B", internal_size, true_size),
            Error::InvalidNumSegments(num_segments) =>
                write!(f, "Exactly 4 segment slots are supported, but this file has {}", num_segments),
            Error::NonZeroPadding => write!(f, "Expected padding but found non-zero byte(s)"),
            Error::Io(source) => if let io::ErrorKind::UnexpectedEof = source.kind() {
                write!(f, "Unexpected end-of-file")
            } else {
                write!(f, "{}", source)
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            // TODO(blocked); see https://github.com/zbraniecki/tinystr/issues/29
            //Error::InvalidName(source) => Some(source),
            Error::Io(source) => Some(source),
            _ => None,
        }
    }
}

trait ReadExt: Read {
    fn read_u8(&mut self) -> io::Result<u8>;
    fn read_i8(&mut self) -> io::Result<i8>;
    fn read_u16_be(&mut self) -> io::Result<u16>;
    fn read_u32_be(&mut self) -> io::Result<u32>;
    fn read_padding(&mut self, num_bytes: u32) -> Result<(), Error>;
}

impl<R: Read> ReadExt for R {
    fn read_u8(&mut self) -> io::Result<u8> {
        let mut buffer = [0; 1];
        self.read_exact(&mut buffer)?;
        Ok(buffer[0])
    }

    fn read_i8(&mut self) -> io::Result<i8> {
        self.read_u8().map(|i| i as i8)
    }

    fn read_u16_be(&mut self) -> io::Result<u16> {
        let mut buffer = [0; 2];
        self.read_exact(&mut buffer)?;
        Ok(u16::from_be_bytes(buffer))
    }

    fn read_u32_be(&mut self) -> io::Result<u32> {
        let mut buffer = [0; 4];
        self.read_exact(&mut buffer)?;
        Ok(u32::from_be_bytes(buffer))
    }

    fn read_padding(&mut self, num_bytes: u32) -> Result<(), Error> {
        for _ in 0..num_bytes {
            if self.read_u8()? != 0 {
                return Err(Error::NonZeroPadding);
            }
        }

        Ok(())
    }
}

trait CollectArray<T, E, U: Default + AsMut<[T]>>: Sized + Iterator<Item = Result<T, E>> {
    /// Doesn't panic if the iterator is too large or too small for the output array. If the iterator is too short,
    /// the remaining elements have their default value.
    fn collect_array(mut self) -> Result<U, E> {
        let mut container = U::default();

        for a in container.as_mut().iter_mut() {
            match self.next() {
                None => break,
                Some(v) => *a = v?,
            }
        }

        Ok(container)
    }

    /// Same as `collect_array`, but panics if the iterator is not exactly the same size as the array.
    /// Based on https://stackoverflow.com/a/60572615
    fn collect_array_pedantic(mut self) -> Result<U, E> {
        let mut container = U::default(); // Could use std::mem::zerored and drop Default requirement here

        for a in container.as_mut().iter_mut() {
            match self.next() {
                None => panic!("iterator has too few members"),
                Some(v) => *a = v?,
            }
        }

        assert!(self.next().is_none(), "iterator has too many members");
        Ok(container)
    }
}

impl<T, E, U: Iterator<Item = Result<T, E>>, V: Default + AsMut<[T]>> CollectArray<T, E, V> for U {}

type TracksMap = HashMap<u64, TaggedRc<[Track; 16]>>;

impl Bgm {
    pub fn from_bytes(f: &[u8]) -> Result<Self, Error> {
        Self::decode(&mut std::io::Cursor::new(f))
    }

    pub fn decode<R: Read + Seek>(f: &mut R) -> Result<Self, Error> {
        f.seek(SeekFrom::Start(0))?;
        let mut magic = [0; 4];
        f.read_exact(&mut magic)?;
        if magic != *MAGIC {
            return Err(Error::InvalidMagic);
        }

        debug_assert!(f.pos()? == 0x04);
        let internal_size = f.read_u32_be()?;
        let true_size = f.seek(SeekFrom::End(0))? as u32;
        if internal_size == true_size {
            // Ok
        } else if align(internal_size, 16) == true_size {
            // Make sure the trailing bytes are all zero
            f.seek(SeekFrom::Start(internal_size as u64))?;
            f.read_padding(true_size - internal_size)?;
        } else {
            return Err(Error::SizeMismatch { true_size, internal_size });
        }

        f.seek(SeekFrom::Start(0x08))?;
        let mut index = [0; 4];
        f.read_exact(&mut index)?;
        let index = tinystr::TinyStr4::from_bytes(&index)
            .map_err(|source| Error::InvalidName(source))?;

        debug_assert!(f.pos()? == 0x0C);
        f.read_padding(4)?;

        debug_assert!(f.pos()? == 0x10);
        let num_segments = f.read_u8()?;
        if num_segments != 4 {
            return Err(Error::InvalidNumSegments(num_segments));
        }

        debug_assert!(f.pos()? == 0x11);
        f.read_padding(3)?;

        debug_assert!(f.pos()? == 0x14);
        let segment_offsets: [u16; 4] = (0..4)
            .into_iter()
            .map(|_| -> io::Result<u16> { Ok(f.read_u16_be()? << 2) }) // 4 contiguous u16 offsets
            .collect_array()?; // We need to obtain all offsets before seeking to any

        debug_assert!(f.pos()? == 0x1C);
        let drums_offset = (f.read_u16_be()? as u64) << 2;
        let drums_count = f.read_u16_be()?;
        let voices_offset = (f.read_u16_be()? as u64) << 2;
        let voices_count = f.read_u16_be()?;

        debug_assert!(f.pos()? == 0x24); // End of struct

        let mut tracks_map = TracksMap::new();
        let mut furthest_read_pos = 0;

        let bgm = Self {
            index,
            segments: segment_offsets
                .iter()
                .map(|&pos| -> Result<Option<Segment>, Error> {
                    if pos == 0 {
                        // Null (no segments)
                        Ok(None)
                    } else {
                        // Seek to the offset and decode the segment(s) there
                        let pos = pos as u64;
                        f.seek(SeekFrom::Start(pos))?;

                        debug!("segment {:#X}", pos);

                        let mut segment = vec![];
                        let mut i = 0;
                        while {
                            f.seek(SeekFrom::Start(pos + i * 4))?;

                            // Peek for null terminator
                            let byte = f.read_u32_be()?;
                            f.seek(SeekFrom::Current(-4))?;
                            byte != 0
                        } {
                            segment.push(Subsegment::decode(f, pos, &mut tracks_map, &mut furthest_read_pos)?);

                            i += 1;
                        }

                        debug!("segment end {:#X}", f.pos()?);

                        Ok(Some(segment))
                    }
                })
                .collect_array_pedantic()?,
            drums: if drums_offset != 0 {
                f.seek(SeekFrom::Start(drums_offset))?;
                (0..drums_count)
                    .into_iter()
                    .map(|_| Drum::decode(f))
                    .collect::<Result<_, _>>()?
            } else {
                Vec::new()
            },
            voices: if voices_offset != 0 {
                f.seek(SeekFrom::Start(voices_offset))?;
                (0..voices_count)
                    .into_iter()
                    .map(|_| Voice::decode(f))
                    .collect::<Result<_, _>>()?
            } else {
                Vec::new()
            },
        };

        let eof_pos = f.seek(SeekFrom::End(0))?;
        if align(furthest_read_pos as u32, 16) < eof_pos as u32 {
            warn!("unused data at {:#X}", furthest_read_pos);
        }

        Ok(bgm)
    }
}

impl Subsegment {
    fn decode<R: Read + Seek>(f: &mut R, start: u64, tracks_map: &mut TracksMap, furthest_read_pos: &mut u64) -> Result<Self, Error> {
        debug!("subsegment {:#X}", f.pos()?);
        let flags = f.read_u8()?;

        if flags & 0x70 == 0x10 {
            f.read_padding(1)?;

            let offset = (f.read_u16_be()? as u64) << 2;
            let tracks_start = start + offset;
            debug!("tracks start = {:#X} (offset = {:#X})", tracks_start, offset);

            Ok(Subsegment::Tracks {
                flags,
                tracks: tracks_map.entry(tracks_start)
                    .or_insert({ // TODO(speed): use or_insert_with; need to figure out how to return Result
                        TaggedRc {
                            decoded_pos: Some(tracks_start),
                            rc: Rc::new((0..16)
                                .into_iter()
                                .map(|track_no| {
                                    f.seek(SeekFrom::Start(tracks_start + track_no * 4))?;
                                    let track = Track::decode(f, tracks_start);

                                    let pos = f.pos()?;
                                    if pos > *furthest_read_pos {
                                        *furthest_read_pos = pos;
                                    }

                                    track
                                })
                                .collect_array_pedantic()?),
                        }
                    })
                    .clone(), // New reference
            })
        } else {
            let mut data = [0; 3];
            f.read_exact(&mut data)?;

            Ok(Subsegment::Unknown {
                flags,
                data,
            })
        }
    }
}

impl Track {
    fn decode<R: Read + Seek>(f: &mut R, segment_start: u64) -> Result<Self, Error> {
        let commands_offset = f.read_u16_be()?;
        let flags = f.read_u16_be()?;

        Ok(Self {
            flags,
            commands: if commands_offset == 0 {
                CommandSeq::with_capacity(0)
            } else {
                f.seek(SeekFrom::Start(segment_start + commands_offset as u64))?;
                let seq = CommandSeq::decode(f)?;

                // Assumption; structure will need changing if false for matching.
                // Maybe use command "groups" which can be represented in UI also
                assert_ne!(seq.len(), 0, "CommandSeq assumption wrong");

                seq
            },
        })
    }
}

impl CommandSeq {
    fn decode<R: Read + Seek>(f: &mut R) -> Result<Self, Error> {
        let start = f.pos()? as usize;

        // A binary tree mapping input offset -> Command. This is then trivially converted to a
        // CommandSeq by performing an in-order traversal.
        let mut commands = OffsetCommandMap::new();

        let mut seen_terminator = false;

        loop {
            let cmd_offset = (f.pos()? as usize) - start;

            if seen_terminator {
                // Sometimes there is a terminator followed by some marked commands (i.e. a subroutine section), so
                // keep reading until every marker has been passed.
                if cmd_offset >= commands.last_offset() {
                    break;
                }
            }

            let cmd_byte = f.read_u8()?;

            let command = match cmd_byte {
                // Sentinel (zero-terminator)
                0x00 => {
                    seen_terminator = true;
                    Command::End
                },

                // Delay
                0x01..=0x77 => Command::Delay(cmd_byte),

                // TODO: this doesn't seem like a long delay; needs testing. Leaving as Unknown for now
                // Long delay
                0x78..=0x7F => {
                    /*
                    // This logic taken from N64MidiTool
                    Command::Delay(0x78 + cmd_byte + (f.read_u8()? & 0x7) << 8));
                    */
                    Command::Unknown(smallvec![cmd_byte, f.read_u8()?])
                },

                // Note
                0x80..=0xD3 => {
                    let flag = (cmd_byte & 1) != 0;
                    let pitch = cmd_byte & !1;

                    let velocity = f.read_u8()?;

                    let length = {
                        let first_byte = f.read_u8()? as u16;

                        // This logic taken from N64MidiTool
                        if first_byte < 0xC0 {
                            first_byte
                        } else {
                            let second_byte = f.read_u8()? as u16;

                            debug_assert_eq!(first_byte & 0xC0, 0xC0);

                            0xC0 + (((first_byte & !0xC0) << 8) | second_byte)
                        }
                    };
                    //assert!(length < 0x4000, "{:#X}", length);

                    Command::Note { pitch, flag, velocity, length }
                },

                0xE0 => Command::MasterTempo(f.read_u16_be()?),
                0xE1 => Command::MasterVolume(f.read_u8()?),
                0xE2 => Command::MasterTranspose(f.read_i8()?),
                0xE3 => Command::Unknown(smallvec![0xE3, f.read_u8()?]),
                0xE4 => Command::MasterTempoFade { time: f.read_u16_be()?, bpm: f.read_u16_be()? },
                0xE5 => Command::MasterVolumeFade { time: f.read_u16_be()?, volume: f.read_u8()? },
                0xE6 => Command::MasterEffect(f.read_u8()?),
                0xE7 => Command::Unknown(smallvec![0xE7]),
                0xE8 => Command::TrackOverridePatch { bank: f.read_u8()?, patch: f.read_u8()? },
                0xE9 => Command::SubTrackVolume(f.read_u8()?),
                0xEA => Command::SubTrackPan(f.read_u8()?),
                0xEB => Command::SubTrackReverb(f.read_u8()?),
                0xEC => Command::SegTrackVolume(f.read_u8()?),
                0xED => Command::SubTrackCoarseTune(f.read_u8()?),
                0xEE => Command::SubTrackFineTune(f.read_u8()?),
                0xEF => Command::SegTrackTune { coarse: f.read_u8()?, fine: f.read_u8()? },
                0xF0 => Command::TrackTremolo {
                    amount: f.read_u8()?,
                    speed: f.read_u8()?,
                    unknown: f.read_u8()?,
                },
                0xF1 => Command::Unknown(smallvec![0xF1, f.read_u8()?]),
                0xF2 => Command::Unknown(smallvec![0xF2, f.read_u8()?]),
                0xF3 => Command::TrackTremoloStop,
                0xF4 => Command::Unknown(smallvec![0xF4, f.read_u8()?, f.read_u8()?]),
                0xF5 => Command::TrackVoice(f.read_u8()?),
                0xF6 => Command::TrackVolumeFade { time: f.read_u16_be()?, volume: f.read_u8()? },
                0xF7 => Command::SubTrackReverbType(f.read_u8()?),
                0xF8 => Command::Unknown(smallvec![0xF8]),
                0xF9 => Command::Unknown(smallvec![0xF9]),
                0xFA => Command::Unknown(smallvec![0xFA]),
                0xFB => Command::Unknown(smallvec![0xFB]),
                0xFC => {
                    let _set_pos = f.read_u16_be()?;
                    let _count = f.read_u8()?;

                    // TODO Jump random
                    todo!("Jump random");
                },
                0xFD => Command::Unknown(smallvec![0xFD, f.read_u8()?, f.read_u8()?, f.read_u8()?]),
                0xFE => {
                    let start_offset = f.read_u16_be()? as usize - start;
                    let end_offset = start_offset + (f.read_u8()? as usize);

                    //debug!("subroutine @ {:#X} (start = {:#X}; end = {:#X})", cmd_offset, start_offset, end_offset);

                    Command::Subroutine(CommandRange {
                        name: format!("Subroutine {:#X}", cmd_offset),
                        start: commands.upsert_marker(start_offset, Marker),
                        end: commands.upsert_marker(end_offset, Marker),
                    })
                },
                0xFF => Command::Unknown(smallvec![0xFF, f.read_u8()?, f.read_u8()?, f.read_u8()?]),

                _ => Command::Unknown(smallvec![cmd_byte]),
            };

            commands.insert(cmd_offset, command);
        }

        let size = f.pos()? as usize - start;
        //debug!("end commandseq {:#X}", f.pos()?);

        // Explode if there are no commands (must be markers) past the end of the file
        for (offset, command) in commands.0.split_off(&OffsetCommandMap::atob(size)).into_iter() {
            panic!("command after end of parsed sequence {:?} @ {:#X}", command, offset);
        }

        Ok(commands.into())
    }
}

/// Temporary struct for [CommandSeq::decode].
#[derive(Debug)]
struct OffsetCommandMap(pub(self) BTreeMap<usize, Command>);

impl OffsetCommandMap {
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Tree offset keys are shifted from the input to make space for abstract commands such as Command::Marker to
    /// be inserted between. This is fine, because, in the end, only the order of the keys matters (not their values).
    pub(self) fn atob(offset: usize) -> usize {
        (offset + 1) * 2
    }

    /// Performs the inverse of [`atob`](OffsetCommandMap::atob). Lossy.
    pub(self) fn btoa(key: usize) -> usize {
        key / 2
    }

    pub fn insert(&mut self, offset: usize, command: Command) {
        self.0.insert(Self::atob(offset), command);
    }

    /// Finds the given marker at `offset`, or inserts it if it cannot be found.
    pub fn upsert_marker(&mut self, offset: usize, marker: Marker) -> Weak<Marker> {
        let shifted_offset = Self::atob(offset) - 1;

        match self.0.entry(shifted_offset) {
            Entry::Vacant(entry) => {
                // Insert the new marker here.
                let marker = Rc::new(marker);
                let marker_weak = Rc::downgrade(&marker);
                entry.insert(Command::Marker(marker.into()));
                marker_weak
            },
            Entry::Occupied(entry) => match entry.get() {
                Command::Marker(l) => {
                    // Re-use matching label `l`, discarding `label`.
                    Rc::downgrade(&**l)
                },
                other_command => panic!(
                    "non-label command {:?} found in label range (shifted_offset = {:#X})",
                    other_command,
                    shifted_offset,
                ),
            },
        }
    }

    pub fn last_offset(&self) -> usize {
        self.0.iter().rev().next() //self.0.last_key_value()
            .map_or(0, |(&k, _)| Self::btoa(k))
    }
}

impl From<OffsetCommandMap> for CommandSeq {
    fn from(map: OffsetCommandMap) -> CommandSeq {
        map.0.into_iter()
            .map(|(_, cmd)| cmd)
            .collect()
    }
}

/*
impl Deref for OffsetCommandMap {
    type Target = BTreeMap<usize, Command>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
*/


impl Drum {
    fn decode<R: Read + Seek>(f: &mut R) -> Result<Self, Error> {
        debug!("drum = {:#X}", f.pos()?);
        Ok(Self {
            bank: f.read_u8()?,
            patch: f.read_u8()?,
            coarse_tune: f.read_u8()?,
            fine_tune: f.read_u8()?,
            volume: f.read_u8()?,
            pan: f.read_u8()?,
            reverb: f.read_u8()?,
            unk_07: f.read_u8()?,
            unk_08: f.read_u8()?,
            unk_09: f.read_u8()?,
            unk_0a: f.read_u8()?,
            unk_0b: f.read_u8()?,
        })
    }
}

impl Voice {
    fn decode<R: Read + Seek>(f: &mut R) -> Result<Self, Error> {
        debug!("drum = {:#X}", f.pos()?);
        Ok(Self {
            bank: f.read_u8()?,
            patch: f.read_u8()?,
            volume: f.read_u8()?,
            pan: f.read_u8()?,
            reverb: f.read_u8()?,
            coarse_tune: f.read_u8()?,
            fine_tune: f.read_u8()?,
            unk_07: f.read_u8()?,
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::io::Cursor;

    /// Make sure that parsing garbage data returns an error.
    #[test]
    fn garbage() {
        let data = include_bytes!("de.rs");
        assert!(Bgm::from_bytes(data).is_err());
    }

    #[test]
    fn decode_subroutine() {
        let bytecode: Vec<u8> = vec![
            0x01, // Delay(1) - at offset 0 (subroutine start)
            0x09, // Delay(9)
            0xFE, 0x00, 0x00, 15, // Subroutine { start = 0, length = 15 }
            0xFE, 0x00, 0x00, 15, // Subroutine { start = 0, length = 15 }
            0xFE, 0x00, 0x00, 15, // Subroutine { start = 0, length = 15 }
            0x01, // Delay(1)
            0x00, // End - at offset 15
        ];

        let seq = CommandSeq::decode(&mut Cursor::new(bytecode)).unwrap();
        dbg!(&seq);

        let start_labels: Vec<Weak<Marker>> = seq.at_time(0)
            .into_iter()
            .take_while(|cmd| match cmd {
                Command::Marker(_) => true,
                _ => false,
            })
            .map(|cmd| match cmd {
                Command::Marker(label) => Rc::downgrade(&**label),
                _ => unreachable!(),
            })
            .collect();
        let end_labels: Vec<Weak<Marker>> = seq.at_time(11)
            .into_iter()
            .take_while(|cmd| match cmd {
                Command::Marker(_) => true,
                _ => false,
            })
            .map(|cmd| match cmd {
                Command::Marker(label) => Rc::downgrade(&**label),
                _ => unreachable!(),
            })
            .collect();
        let subroutine_labels: Vec<(&Weak<Marker>, &Weak<Marker>)> = seq.at_time(10)
            .into_iter()
            .take_while(|cmd| match cmd {
                Command::Subroutine(_) => true,
                _ => false,
            })
            .map(|cmd| match cmd {
                Command::Subroutine(CommandRange { start, end, .. }) => (start, end),
                _ => unreachable!(),
            })
            .collect();

        assert_eq!(start_labels.len(), 1);
        assert_eq!(end_labels.len(), 1);
        assert_eq!(subroutine_labels.len(), 3);

        // Check start labels
        assert!(Weak::ptr_eq(subroutine_labels[0].0, &start_labels[0]));
        assert!(Weak::ptr_eq(subroutine_labels[1].0, &start_labels[0]));
        assert!(Weak::ptr_eq(subroutine_labels[2].0, &start_labels[0]));

        // Check end labels
        assert!(Weak::ptr_eq(subroutine_labels[0].1, &end_labels[0]));
        assert!(Weak::ptr_eq(subroutine_labels[1].1, &end_labels[0]));
        assert!(Weak::ptr_eq(subroutine_labels[2].1, &end_labels[0]));
    }
}
