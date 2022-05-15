use std::collections::{HashMap, VecDeque};
use std::io::{self, Cursor, Read, Seek, SeekFrom};

use byteorder::{BigEndian, ByteOrder, LittleEndian, ReadBytesExt};
use linux_perf_event_reader::consts::PERF_RECORD_MISC_BUILD_ID_SIZE;
use linux_perf_event_reader::records::{ParsedRecord, RawRecord, RecordParseInfo};
use linux_perf_event_reader::{CpuMode, PerfEventAttr, PerfEventHeader, RawData, RecordType};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endianness {
    LittleEndian,
    BigEndian,
}

pub struct PerfFileReader<'a, R: Read> {
    reader: &'a mut R,
    endian: Endianness,
    feature_sections: Vec<(FlagFeature, Vec<u8>)>,
    read_offset: u64,
    record_data_len: u64,
    current_event_body: Vec<u8>,
    parse_info: RecordParseInfo,
    /// Sorted by time
    remaining_pending_records: VecDeque<PendingRecord>,
    buffers_for_recycling: VecDeque<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DsoBuildId {
    pub path: Vec<u8>,
    pub build_id: Vec<u8>,
}

impl<'a, C: Read + Seek> PerfFileReader<'a, C> {
    pub fn parse_file(cursor: &'a mut C) -> Result<Self, Error> {
        let header = PerfHeader::parse(cursor)?;
        match header.endian {
            Endianness::LittleEndian => Self::parse_file_impl::<LittleEndian>(cursor, header),
            Endianness::BigEndian => Self::parse_file_impl::<BigEndian>(cursor, header),
        }
    }

    fn parse_file_impl<T>(cursor: &'a mut C, header: PerfHeader) -> Result<Self, Error>
    where
        T: ByteOrder,
    {
        // Read the section information for each flag, starting just after the data section.
        let mut flag = 0u32;
        let feature_pos = header.data_section.offset + header.data_section.size;
        cursor.seek(SeekFrom::Start(feature_pos))?;
        let mut feature_sections_info = Vec::new();
        for flags_chunk in header.flags {
            for bit_index in 0..8 {
                let flag_is_set = (flags_chunk & (1 << bit_index)) != 0;
                if flag_is_set {
                    let section = PerfFileSection::parse::<C, T>(cursor)?;
                    if let Some(feature) = FlagFeature::from_int(flag) {
                        feature_sections_info.push((feature, section));
                    } else {
                        eprintln!("Unrecognized flag feature {}", flag);
                    }
                }
                flag += 1;
            }
        }

        let mut feature_sections = Vec::new();
        for (feature, section) in feature_sections_info {
            let offset = section.offset;
            let size = usize::try_from(section.size).map_err(|_| Error::SectionSizeTooBig)?;
            let mut data = vec![0; size];
            cursor.seek(SeekFrom::Start(offset))?;
            cursor.read_exact(&mut data)?;
            feature_sections.push((feature, data));
        }

        let attrs_offset = header.attr_section.offset;
        let attrs_size = header.attr_section.size;
        cursor.seek(SeekFrom::Start(attrs_offset))?;
        let mut perf_event_attrs = Vec::new();
        let attr_size = header.attr_size;
        let mut offset = 0;
        while offset + attr_size <= attrs_size {
            let attr = PerfEventAttr::parse::<C, T>(cursor, Some(attr_size as u32))
                .map_err(|_| ReadError::PerfEventAttr)?;
            perf_event_attrs.push(attr);
            offset += attr_size;
        }

        // Grab the first of the perf event attrs.
        // TODO: What happens if there's more than one attr? How do we know which
        // records belong to which event?
        let attr = &perf_event_attrs[0];
        let parse_info = RecordParseInfo::from_attr(attr);

        // Move the cursor to the start of the data section so that we can start
        // reading records from it.
        cursor.seek(SeekFrom::Start(header.data_section.offset))?;

        Ok(Self {
            reader: cursor,
            endian: header.endian,
            feature_sections,
            read_offset: 0,
            record_data_len: header.data_section.size,
            parse_info,
            remaining_pending_records: VecDeque::new(),
            buffers_for_recycling: VecDeque::new(),
            current_event_body: Vec::new(),
        })
    }
}

impl<'a, R: Read> PerfFileReader<'a, R> {
    pub fn endian(&self) -> Endianness {
        self.endian
    }

    pub fn has_feature(&self, feature: FlagFeature) -> bool {
        self.feature_sections.iter().any(|(f, _)| *f == feature)
    }

    pub fn feature_section(&self, feature: FlagFeature) -> Option<&[u8]> {
        self.feature_sections
            .iter()
            .find_map(|(f, d)| if *f == feature { Some(&d[..]) } else { None })
    }

    /// Returns a map of build ID entries. `perf record` creates these records for any DSOs
    /// which it thinks have been "hit" in the profile. They supplement Mmap events
    /// the perf event stream; those usually don't come with build IDs.
    ///
    /// This method returns a HashMap so that you can easily look up the right build ID from
    /// the DsoKey in an Mmap event. For some DSOs, the path in the raw Mmap event can be
    /// different from the path in the build ID record; for example, the Mmap event for the
    /// kernel ("vmlinux") image could have the path "[kernel.kallsyms]_text", whereas the
    /// corresponding build ID record might have the path "[kernel.kallsyms]" (without the
    /// trailing "_text"), or it could even have the full absolute path to a vmlinux file.
    /// The DsoKey canonicalizes those differences away.
    ///
    /// Having the build ID for a DSO allows you to do the following:
    ///
    ///  - If the DSO file has changed in the time since the perf.data file was captured,
    ///    you can detect this change because the new file will have a different build ID.
    ///  - If debug symbols are installed for the DSO, you can sometimes find the debug symbol
    ///    file using the build ID. For example, you might find it at
    ///    /usr/lib/debug/.build-id/b8/037b6260865346802321dd2256b8ad1d857e63.debug
    ///  - If the original DSO file is gone, or you're trying to read the perf.data file on
    ///    an entirely different machine, you can sometimes retrieve the original DSO file just
    ///    from its build ID, for example from a debuginfod server.
    ///  - This also works for DSOs which are not present on the file system at all;
    ///    specifically, the vDSO file is a bit of a pain to obtain. With the build ID you can
    ///    instead obtain it from, say,
    ///    <https://debuginfod.elfutils.org/buildid/0d82ee4bd7f9609c367095ba0bedf155b71cb058/executable>
    ///
    /// This method is a bit lossy. We discard the pid, because it seems to be always -1 in
    /// the files I've tested. We also discard any entries for which we fail to create a `DsoKey`.
    pub fn build_ids(&self) -> Result<HashMap<DsoKey, DsoBuildId>, Error> {
        let section_data = match self.feature_section(FlagFeature::BuildId) {
            Some(section) => section,
            None => return Ok(HashMap::new()),
        };
        let mut cursor = Cursor::new(section_data);
        let mut build_ids = HashMap::new();
        loop {
            let event = match self.endian {
                Endianness::LittleEndian => BuildIdEvent::parse::<_, LittleEndian>(&mut cursor),
                Endianness::BigEndian => BuildIdEvent::parse::<_, BigEndian>(&mut cursor),
            };
            let event = match event {
                Ok(e) => e,
                Err(_) => break,
            };
            let misc = event.header.misc;
            let path = event.file_path;
            let build_id = event.build_id;
            let dso_key = match DsoKey::detect(&path, CpuMode::from_misc(misc)) {
                Some(dso_key) => dso_key,
                None => continue,
            };
            build_ids.insert(dso_key, DsoBuildId { path, build_id });
        }
        Ok(build_ids)
    }

    /// Only call this for features whose section is just a perf_header_string.
    fn feature_string(&self, feature: FlagFeature) -> Result<Option<&str>, Error> {
        self.feature_section(feature)
            .map(|section| self.read_string(section))
            .transpose()
    }

    pub fn hostname(&self) -> Result<Option<&str>, Error> {
        self.feature_string(FlagFeature::Hostname)
    }

    pub fn os_release(&self) -> Result<Option<&str>, Error> {
        self.feature_string(FlagFeature::OsRelease)
    }

    pub fn perf_version(&self) -> Result<Option<&str>, Error> {
        self.feature_string(FlagFeature::Version)
    }

    pub fn arch(&self) -> Result<Option<&str>, Error> {
        self.feature_string(FlagFeature::Arch)
    }

    pub fn nr_cpus(&self) -> Result<Option<NrCpus>, Error> {
        self.feature_section(FlagFeature::NrCpus)
            .map(|section| {
                let mut cursor = Cursor::new(section);
                match self.endian {
                    Endianness::LittleEndian => NrCpus::parse::<_, LittleEndian>(&mut cursor),
                    Endianness::BigEndian => NrCpus::parse::<_, BigEndian>(&mut cursor),
                }
            })
            .transpose()
    }

    pub fn is_stats(&self) -> bool {
        self.has_feature(FlagFeature::Stat)
    }

    fn read_string<'s>(&self, s: &'s [u8]) -> Result<&'s str, Error> {
        if s.len() < 4 {
            return Err(Error::NotEnoughSpaceForStringLen);
        }
        let (len_bytes, rest) = s.split_at(4);
        let len_bytes = [len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]];
        let len = match self.endian {
            Endianness::LittleEndian => u32::from_le_bytes(len_bytes),
            Endianness::BigEndian => u32::from_be_bytes(len_bytes),
        };
        let len = usize::try_from(len).map_err(|_| Error::StringLengthBiggerThanUsize)?;
        let s = &rest.get(..len as usize).ok_or(Error::StringLengthTooLong)?;
        let actual_len = memchr::memchr(0, s).unwrap_or(s.len());
        let s = std::str::from_utf8(&s[..actual_len]).map_err(|_| Error::StringUtf8)?;
        Ok(s)
    }

    /// Emits records in the correct order (sorted by time).
    ///
    /// It buffers records until it sees a FINISHED_ROUND record; then it sorts the
    /// buffered records and emits them one by one.
    pub fn next_record(&mut self) -> Result<Option<ParsedRecord>, Error> {
        if self.remaining_pending_records.is_empty() {
            self.read_current_round()?;
        }
        if let Some(pending_record) = self.remaining_pending_records.pop_front() {
            return Ok(Some(self.convert_pending_record(pending_record)?));
        }
        Ok(None)
    }

    /// Reads events into self.remaining_pending_records until a FINISHED_ROUND
    /// record is found and self.remaining_pending_records is non-empty, or until
    /// we've run out of records to read.
    ///
    /// When this function returns, self.remaining_pending_records is sorted by
    /// timestamp.
    fn read_current_round(&mut self) -> Result<(), Error> {
        if self.endian == Endianness::LittleEndian {
            self.read_current_round_impl::<byteorder::LittleEndian>()
        } else {
            self.read_current_round_impl::<byteorder::BigEndian>()
        }
    }

    /// Reads events into self.remaining_pending_records until a FINISHED_ROUND
    /// record is found and self.remaining_pending_records is non-empty, or until
    /// we've run out of records to read.
    ///
    /// When this function returns, self.remaining_pending_records is sorted by
    /// timestamp.
    fn read_current_round_impl<T: ByteOrder>(&mut self) -> Result<(), Error> {
        assert!(self.remaining_pending_records.is_empty());

        while self.read_offset < self.record_data_len {
            let offset = self.read_offset;
            let header = PerfEventHeader::parse::<_, T>(&mut self.reader)?;
            let size = header.size as usize;
            if size < PerfEventHeader::STRUCT_SIZE {
                return Err(Error::InvalidPerfEventSize);
            }
            self.read_offset += u64::from(header.size);

            let record_type = RecordType(header.type_);
            if record_type == RecordType::FINISHED_ROUND {
                if self.remaining_pending_records.is_empty() {
                    // Keep going so that we never return with remaining_pending_records
                    // being empty, unless we've truly run out of data to read.
                    continue;
                } else {
                    // We've finished a non-empty round. Exit the loop.
                    break;
                }
            }

            let event_body_len = size - PerfEventHeader::STRUCT_SIZE;
            let mut buffer = self.buffers_for_recycling.pop_front().unwrap_or_default();
            buffer.resize(event_body_len, 0);
            self.reader
                .read_exact(&mut buffer)
                .map_err(|_| ReadError::PerfEventData)?;

            let misc = header.misc;
            let raw_event = RawRecord {
                record_type,
                misc,
                data: RawData::from(&buffer[..]),
            };
            let timestamp = raw_event.timestamp::<T>(&self.parse_info);
            let sort_key = RecordSortKey { timestamp, offset };
            let pending_record = PendingRecord {
                sort_key,
                record_type,
                misc,
                buffer,
            };
            self.remaining_pending_records.push_back(pending_record);
        }

        self.remaining_pending_records
            .make_contiguous()
            .sort_unstable_by_key(|r| r.sort_key);
        Ok(())
    }

    /// Converts pending_record into an ParsedRecord which references the data in self.current_event_body.
    fn convert_pending_record(
        &mut self,
        pending_record: PendingRecord,
    ) -> Result<ParsedRecord, Error> {
        let PendingRecord {
            record_type,
            misc,
            buffer,
            ..
        } = pending_record;
        let prev_buffer = std::mem::replace(&mut self.current_event_body, buffer);
        self.buffers_for_recycling.push_back(prev_buffer);
        let raw_data = RawData::from(&self.current_event_body[..]);
        let raw_event = RawRecord {
            record_type,
            misc,
            data: raw_data,
        };
        Ok(if self.endian == Endianness::LittleEndian {
            raw_event.parse::<byteorder::LittleEndian>(&self.parse_info)
        } else {
            raw_event.parse::<byteorder::BigEndian>(&self.parse_info)
        }?)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingRecord {
    sort_key: RecordSortKey,
    record_type: RecordType,
    misc: u16,
    buffer: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RecordSortKey {
    timestamp: Option<u64>,
    offset: u64,
}

/// Old versions of perf did not write down the length of the build ID.
/// Detect the true length by removing 4-byte chunks of zeros from the end.
fn detect_build_id_len(build_id_bytes: &[u8]) -> u8 {
    let mut len = build_id_bytes.len();
    const CHUNK_SIZE: usize = 4;
    for chunk in build_id_bytes.chunks(CHUNK_SIZE).rev() {
        if chunk.iter().any(|b| *b != 0) {
            break;
        }
        len -= chunk.len();
    }
    len as u8
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagFeature {
    TracingData,
    BuildId,
    Hostname,
    OsRelease,
    Version,
    Arch,
    NrCpus,
    CpuDesc,
    CpuId,
    TotalMem,
    Cmdline,
    EventDesc,
    CpuTopology,
    NumaTopology,
    BranchStack,
    PmuMappings,
    GroupDesc,
    Auxtrace,
    Stat,
    Cache,
    SampleTime,
    SampleTopology,
    ClockId,
    DirFormat,
    CpuPmuCaps,
    ClockData,
    HybridTopology,
    HybridCpuPmuCaps,
}

impl FlagFeature {
    pub fn from_int(i: u32) -> Option<Self> {
        let feature = match i {
            HEADER_TRACING_DATA => Self::TracingData,
            HEADER_BUILD_ID => Self::BuildId,
            HEADER_HOSTNAME => Self::Hostname,
            HEADER_OSRELEASE => Self::OsRelease,
            HEADER_VERSION => Self::Version,
            HEADER_ARCH => Self::Arch,
            HEADER_NRCPUS => Self::NrCpus,
            HEADER_CPUDESC => Self::CpuDesc,
            HEADER_CPUID => Self::CpuId,
            HEADER_TOTAL_MEM => Self::TotalMem,
            HEADER_CMDLINE => Self::Cmdline,
            HEADER_EVENT_DESC => Self::EventDesc,
            HEADER_CPU_TOPOLOGY => Self::CpuTopology,
            HEADER_NUMA_TOPOLOGY => Self::NumaTopology,
            HEADER_BRANCH_STACK => Self::BranchStack,
            HEADER_PMU_MAPPINGS => Self::PmuMappings,
            HEADER_GROUP_DESC => Self::GroupDesc,
            HEADER_AUXTRACE => Self::Auxtrace,
            HEADER_STAT => Self::Stat,
            HEADER_CACHE => Self::Cache,
            HEADER_SAMPLE_TIME => Self::SampleTime,
            HEADER_SAMPLE_TOPOLOGY => Self::SampleTopology,
            HEADER_CLOCKID => Self::ClockId,
            HEADER_DIR_FORMAT => Self::DirFormat,
            HEADER_CPU_PMU_CAPS => Self::CpuPmuCaps,
            HEADER_CLOCK_DATA => Self::ClockData,
            HEADER_HYBRID_TOPOLOGY => Self::HybridTopology,
            HEADER_HYBRID_CPU_PMU_CAPS => Self::HybridCpuPmuCaps,
            _ => return None,
        };
        Some(feature)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DsoKey {
    Kernel,
    GuestKernel,
    Vdso32,
    VdsoX32,
    Vdso64,
    Vsyscall,
    KernelModule(String),
    User(String, Vec<u8>),
}

impl DsoKey {
    pub fn detect(path: &[u8], cpu_mode: CpuMode) -> Option<Self> {
        if path == b"//anon" || path == b"[stack]" || path == b"[heap]" || path == b"[vvar]" {
            return None;
        }

        if path.starts_with(b"[kernel.kallsyms]") {
            let dso_key = if cpu_mode == CpuMode::GuestKernel {
                DsoKey::GuestKernel
            } else {
                DsoKey::Kernel
            };
            return Some(dso_key);
        }
        if path.starts_with(b"[guest.kernel.kallsyms") {
            return Some(DsoKey::GuestKernel);
        }
        if path == b"[vdso32]" {
            return Some(DsoKey::Vdso32);
        }
        if path == b"[vdsox32]" {
            return Some(DsoKey::VdsoX32);
        }
        if path == b"[vdso]" {
            // TODO: I think this could also be Vdso32 when recording on a 32 bit machine.
            return Some(DsoKey::Vdso64);
        }
        if path == b"[vsyscall]" {
            return Some(DsoKey::Vsyscall);
        }
        if (cpu_mode == CpuMode::Kernel || cpu_mode == CpuMode::GuestKernel)
            && path.starts_with(b"[")
        {
            return Some(DsoKey::KernelModule(String::from_utf8_lossy(path).into()));
        }

        let filename = if let Some(final_slash_pos) = path.iter().rposition(|b| *b == b'/') {
            &path[final_slash_pos + 1..]
        } else {
            path
        };

        let dso_key = match (cpu_mode, filename.strip_suffix(b".ko")) {
            (CpuMode::Kernel | CpuMode::GuestKernel, Some(kmod_name)) => {
                // "/lib/modules/5.13.0-35-generic/kernel/sound/core/snd-seq-device.ko" -> "[snd-seq-device]"
                let kmod_name = String::from_utf8_lossy(kmod_name);
                DsoKey::KernelModule(format!("[{}]", kmod_name))
            }
            (CpuMode::Kernel, _) => DsoKey::Kernel,
            (CpuMode::GuestKernel, _) => DsoKey::GuestKernel,
            (CpuMode::User | CpuMode::GuestUser, _) => {
                DsoKey::User(String::from_utf8_lossy(filename).into(), path.to_owned())
            }
            _ => return None,
        };
        Some(dso_key)
    }

    pub fn name(&self) -> &str {
        match self {
            DsoKey::Kernel => "[kernel.kallsyms]",
            DsoKey::GuestKernel => "[guest.kernel.kallsyms]",
            DsoKey::Vdso32 => "[vdso32]",
            DsoKey::VdsoX32 => "[vdsox32]",
            DsoKey::Vdso64 => "[vdso]",
            DsoKey::Vsyscall => "[vsyscall]",
            DsoKey::KernelModule(name) => name,
            DsoKey::User(name, _) => name,
        }
    }
}

/// `perf_header`
///
/// The magic number identifies the perf file and the version. Current perf versions
/// use PERFILE2. Old perf versions generated a version 1 format (PERFFILE). Version 1
/// is not described here. The magic number also identifies the endian. When the
/// magic value is 64bit byte swapped compared the file is in non-native
/// endian.

#[derive(Debug, Clone, Copy)]
pub struct PerfHeader {
    pub endian: Endianness,
    /// size of the header
    pub header_size: u64,
    /// size of an attribute in attrs
    pub attr_size: u64,
    pub attr_section: PerfFileSection,
    pub data_section: PerfFileSection,
    /// Room for 4 * 64 = 256 header flag bits
    pub flags: [u64; 4],
}

impl PerfHeader {
    pub fn parse<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let mut magic = [0; 8];
        reader.read_exact(&mut magic)?;
        if &magic != b"PERFILE2" && &magic != b"2ELIFREP" {
            return Err(Error::UnrecognizedMagicValue(magic));
        }

        if magic[0] == b'P' {
            Self::parse_impl::<R, byteorder::LittleEndian>(reader, Endianness::LittleEndian)
        } else {
            Self::parse_impl::<R, byteorder::BigEndian>(reader, Endianness::BigEndian)
        }
    }

    fn parse_impl<R: Read, T: ByteOrder>(
        reader: &mut R,
        endian: Endianness,
    ) -> Result<Self, Error> {
        let header_size = reader.read_u64::<T>()?;
        let attr_size = reader.read_u64::<T>()?;
        let attr_section = PerfFileSection::parse::<R, T>(reader)?;
        let data_section = PerfFileSection::parse::<R, T>(reader)?;
        let _event_types_section = PerfFileSection::parse::<R, T>(reader)?;
        let flags = [
            reader.read_u64::<T>()?,
            reader.read_u64::<T>()?,
            reader.read_u64::<T>()?,
            reader.read_u64::<T>()?,
        ];
        Ok(Self {
            endian,
            header_size,
            attr_size,
            attr_section,
            data_section,
            flags,
        })
    }
}

pub const HEADER_TRACING_DATA: u32 = 1;
pub const HEADER_BUILD_ID: u32 = 2;
pub const HEADER_HOSTNAME: u32 = 3;
pub const HEADER_OSRELEASE: u32 = 4;
pub const HEADER_VERSION: u32 = 5;
pub const HEADER_ARCH: u32 = 6;
pub const HEADER_NRCPUS: u32 = 7;
pub const HEADER_CPUDESC: u32 = 8;
pub const HEADER_CPUID: u32 = 9;
pub const HEADER_TOTAL_MEM: u32 = 10;
pub const HEADER_CMDLINE: u32 = 11;
pub const HEADER_EVENT_DESC: u32 = 12;
pub const HEADER_CPU_TOPOLOGY: u32 = 13;
pub const HEADER_NUMA_TOPOLOGY: u32 = 14;
pub const HEADER_BRANCH_STACK: u32 = 15;
pub const HEADER_PMU_MAPPINGS: u32 = 16;
pub const HEADER_GROUP_DESC: u32 = 17;
pub const HEADER_AUXTRACE: u32 = 18;
pub const HEADER_STAT: u32 = 19;
pub const HEADER_CACHE: u32 = 20;
pub const HEADER_SAMPLE_TIME: u32 = 21;
pub const HEADER_SAMPLE_TOPOLOGY: u32 = 22;
pub const HEADER_CLOCKID: u32 = 23;
pub const HEADER_DIR_FORMAT: u32 = 24;
pub const HEADER_CPU_PMU_CAPS: u32 = 28;
pub const HEADER_CLOCK_DATA: u32 = 29;
pub const HEADER_HYBRID_TOPOLOGY: u32 = 30;
pub const HEADER_HYBRID_CPU_PMU_CAPS: u32 = 31;

/// `perf_file_section`
///
/// A PerfFileSection contains a pointer to another section of the perf file.
/// The header contains three such pointers: for attributes, data and event types.
#[derive(Debug, Clone, Copy)]
pub struct PerfFileSection {
    /// offset from start of file
    pub offset: u64,
    /// size of the section
    pub size: u64,
}

impl PerfFileSection {
    pub fn parse<R: Read, T: ByteOrder>(reader: &mut R) -> Result<Self, Error> {
        let offset = reader.read_u64::<T>()?;
        let size = reader.read_u64::<T>()?;
        Ok(Self { offset, size })
    }
}

/// `build_id_event`
///
/// If PERF_RECORD_MISC_KERNEL is set in header.misc, then this
/// is the build id for the vmlinux image or a kmod.
#[derive(Debug, Clone)]
pub struct BuildIdEvent {
    pub header: PerfEventHeader,
    pub pid: i32,
    pub build_id: Vec<u8>,
    pub file_path: Vec<u8>,
}

impl BuildIdEvent {
    pub fn parse<R: Read, T: ByteOrder>(reader: &mut R) -> Result<Self, Error> {
        let header = PerfEventHeader::parse::<R, T>(reader)?;
        let pid = reader.read_i32::<T>()?;
        let mut build_id_bytes = [0; 24];
        reader.read_exact(&mut build_id_bytes)?;

        // Followed by file path for the remaining bytes. The total size of the record
        // is given by header.size.
        const BYTES_BEFORE_PATH: usize = PerfEventHeader::STRUCT_SIZE + 4 + 24;
        let path_len = usize::from(header.size).saturating_sub(BYTES_BEFORE_PATH);
        let mut path_bytes = vec![0; path_len];
        reader.read_exact(&mut path_bytes)?;

        let path_len = memchr::memchr(0, &path_bytes).unwrap_or(path_len);
        path_bytes.truncate(path_len);
        let file_path = path_bytes;

        // If PERF_RECORD_MISC_BUILD_ID_SIZE is set in header.misc, then build_id_bytes[20]
        // is the length of the build id (<= 20), and build_id_bytes[21..24] are unused.
        // Otherwise, the length of the build ID is unknown and has to be detected by
        // removing trailing 4-byte groups of zero bytes. (Usually there will be
        // exactly one such group, because build IDs are usually 20 bytes long.)
        let build_id_len = if header.misc & PERF_RECORD_MISC_BUILD_ID_SIZE != 0 {
            build_id_bytes[20].min(20)
        } else {
            detect_build_id_len(&build_id_bytes)
        };
        let build_id = build_id_bytes[..build_id_len as usize].to_owned();

        Ok(Self {
            header,
            pid,
            build_id,
            file_path,
        })
    }
}

/// `nr_cpus`
#[derive(Debug, Clone, Copy)]
pub struct NrCpus {
    /// CPUs not yet onlined
    pub nr_cpus_available: u32,
    pub nr_cpus_online: u32,
}

impl NrCpus {
    pub const STRUCT_SIZE: usize = 4 + 4;

    pub fn parse<R: Read, T: ByteOrder>(reader: &mut R) -> Result<Self, Error> {
        let nr_cpus_available = reader.read_u32::<T>()?;
        let nr_cpus_online = reader.read_u32::<T>()?;
        Ok(Self {
            nr_cpus_available,
            nr_cpus_online,
        })
    }
}

/// The error type used in this crate.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// The data slice was not big enough to read the struct, or we
    /// were trying to follow an invalid offset to somewhere outside
    /// of the data bounds.
    #[error("Read error: {0}")]
    Read(#[from] ReadError),

    #[error("I/O error: {0}")]
    IoError(#[from] io::Error),

    #[error("Did not recognize magic value {0:?}")]
    UnrecognizedMagicValue([u8; 8]),

    #[error("Section size did not fit into usize")]
    SectionSizeTooBig,

    #[error("The section wasn't big enough to contain the u32 string length")]
    NotEnoughSpaceForStringLen,

    #[error("The section wasn't big enough to contain the NrCpus struct")]
    NotEnoughSpaceForNrCpus,

    #[error("The indicated string length wouldn't fit in the indicated section size")]
    StringLengthTooLong,

    #[error("The indicated string length wouldn't fit into usize")]
    StringLengthBiggerThanUsize,

    #[error("The string was not valid utf-8")]
    StringUtf8,

    #[error("The specified size in the perf event header was smaller than the header itself")]
    InvalidPerfEventSize,
}

/// This error indicates that the data slice was not large enough to
/// read the respective item.
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    #[error("Could not read PerfHeader")]
    PerfHeader,

    #[error("Could not read PerFlagSection")]
    PerFlagSection,

    #[error("Could not read BuildIdSection")]
    BuildIdSection,

    #[error("Could not read StringLen")]
    StringLen,

    #[error("Could not read String")]
    String,

    #[error("Could not read NrCpus")]
    NrCpus,

    #[error("Could not read AttrsSection")]
    AttrsSection,

    #[error("Could not read PerfEventAttr")]
    PerfEventAttr,

    #[error("Could not read PerfEventHeader")]
    PerfEventHeader,

    #[error("Could not read PerfEvent data")]
    PerfEventData,
}
