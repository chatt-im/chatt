use std::{
    fs::{self, File},
    io::{self, BufReader, BufWriter, Read, Write},
    path::Path,
};

pub const MAGIC: &[u8; 8] = b"TCHOPUS1";
pub const FLAG_DENOISE: u8 = 0b0000_0001;
pub const HEADER_LEN: usize = MAGIC.len() + 4 + 2 + 1 + 1 + 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketLogHeader {
    pub sample_rate: u32,
    pub frame_samples: u16,
    pub channels: u8,
    pub flags: u8,
    pub bitrate_bps: u32,
}

pub struct PacketLogWriter<W> {
    inner: W,
}

pub struct PacketLogReader<R> {
    inner: R,
    header: PacketLogHeader,
}

impl<W: Write> PacketLogWriter<W> {
    pub fn new(mut inner: W, header: PacketLogHeader) -> io::Result<Self> {
        inner.write_all(MAGIC)?;
        inner.write_all(&header.sample_rate.to_le_bytes())?;
        inner.write_all(&header.frame_samples.to_le_bytes())?;
        inner.write_all(&[header.channels, header.flags])?;
        inner.write_all(&header.bitrate_bps.to_le_bytes())?;
        Ok(Self { inner })
    }

    pub fn write_packet(&mut self, packet: &[u8]) -> io::Result<()> {
        let len = u16::try_from(packet.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "opus packet exceeds packet-log u16 length field",
            )
        })?;
        self.inner.write_all(&len.to_le_bytes())?;
        self.inner.write_all(packet)
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }

    #[cfg(test)]
    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl PacketLogWriter<BufWriter<File>> {
    pub fn create(path: &Path, header: PacketLogHeader) -> io::Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let file = File::create(path)?;
        PacketLogWriter::new(BufWriter::new(file), header)
    }
}

impl<R: Read> PacketLogReader<R> {
    pub fn new(mut inner: R) -> io::Result<Self> {
        let mut header_bytes = [0u8; HEADER_LEN];
        inner.read_exact(&mut header_bytes)?;

        if &header_bytes[..MAGIC.len()] != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid tomchat opus packet-log magic",
            ));
        }

        let header = PacketLogHeader {
            sample_rate: u32::from_le_bytes(header_bytes[8..12].try_into().unwrap()),
            frame_samples: u16::from_le_bytes(header_bytes[12..14].try_into().unwrap()),
            channels: header_bytes[14],
            flags: header_bytes[15],
            bitrate_bps: u32::from_le_bytes(header_bytes[16..20].try_into().unwrap()),
        };

        Ok(Self { inner, header })
    }

    pub fn header(&self) -> PacketLogHeader {
        self.header
    }

    pub fn read_packet(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut len_bytes = [0u8; 2];
        match self.inner.read_exact(&mut len_bytes) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error),
        }

        let len = u16::from_le_bytes(len_bytes) as usize;
        let mut packet = vec![0; len];
        self.inner.read_exact(&mut packet)?;
        Ok(Some(packet))
    }
}

impl PacketLogReader<BufReader<File>> {
    pub fn open(path: &Path) -> io::Result<Self> {
        PacketLogReader::new(BufReader::new(File::open(path)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_header_and_length_prefixed_packets() {
        let header = PacketLogHeader {
            sample_rate: 48_000,
            frame_samples: 480,
            channels: 1,
            flags: FLAG_DENOISE,
            bitrate_bps: 24_000,
        };
        let mut writer = PacketLogWriter::new(Vec::new(), header).unwrap();
        writer.write_packet(&[1, 2, 3]).unwrap();
        writer.flush().unwrap();

        let bytes = writer.into_inner();
        assert_eq!(&bytes[..MAGIC.len()], MAGIC);
        assert_eq!(bytes.len(), HEADER_LEN + 2 + 3);
        assert_eq!(
            u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            header.sample_rate
        );
        assert_eq!(
            u16::from_le_bytes(bytes[12..14].try_into().unwrap()),
            header.frame_samples
        );
        assert_eq!(bytes[14], header.channels);
        assert_eq!(bytes[15], header.flags);
        assert_eq!(
            u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            header.bitrate_bps
        );
        assert_eq!(u16::from_le_bytes(bytes[20..22].try_into().unwrap()), 3);
        assert_eq!(&bytes[22..], &[1, 2, 3]);
    }

    #[test]
    fn rejects_packets_larger_than_length_field() {
        let header = PacketLogHeader {
            sample_rate: 48_000,
            frame_samples: 480,
            channels: 1,
            flags: 0,
            bitrate_bps: 24_000,
        };
        let mut writer = PacketLogWriter::new(Vec::new(), header).unwrap();
        let packet = vec![0; usize::from(u16::MAX) + 1];
        let err = writer.write_packet(&packet).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn reads_header_and_length_prefixed_packets() {
        let header = PacketLogHeader {
            sample_rate: 48_000,
            frame_samples: 480,
            channels: 1,
            flags: FLAG_DENOISE,
            bitrate_bps: 24_000,
        };
        let mut writer = PacketLogWriter::new(Vec::new(), header).unwrap();
        writer.write_packet(&[1, 2, 3]).unwrap();
        writer.write_packet(&[4, 5]).unwrap();

        let bytes = writer.into_inner();
        let mut reader = PacketLogReader::new(bytes.as_slice()).unwrap();

        assert_eq!(reader.header(), header);
        assert_eq!(reader.read_packet().unwrap(), Some(vec![1, 2, 3]));
        assert_eq!(reader.read_packet().unwrap(), Some(vec![4, 5]));
        assert_eq!(reader.read_packet().unwrap(), None);
    }
}
