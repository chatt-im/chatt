//! Strict, serialized range reads for daemon-owned in-memory attachments.

use std::io::{self, Read, Write};

const MAGIC: [u8; 4] = *b"CAR1";
pub const REQUEST_HEADER_LEN: usize = 16;
pub const RESPONSE_HEADER_LEN: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadRequest {
    pub offset: u64,
    pub length: u32,
}

impl ReadRequest {
    pub fn validate(self) -> io::Result<()> {
        let length = self.length as usize;
        if length == 0 || length > super::MAX_ATTACHMENT_READ_BYTES {
            return Err(invalid_data("attachment read length is invalid"));
        }
        self.offset
            .checked_add(self.length as u64)
            .ok_or_else(|| invalid_data("attachment read range overflows"))?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ResponseStatus {
    Data = 0,
    InvalidRequest = 1,
    SourceChanged = 2,
    IoFailure = 3,
}

impl TryFrom<u8> for ResponseStatus {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Data),
            1 => Ok(Self::InvalidRequest),
            2 => Ok(Self::SourceChanged),
            3 => Ok(Self::IoFailure),
            _ => Err(invalid_data("attachment read response status is invalid")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadResponse {
    pub status: ResponseStatus,
    pub payload: Vec<u8>,
}

pub fn encode_request_header(request: ReadRequest) -> io::Result<[u8; REQUEST_HEADER_LEN]> {
    request.validate()?;
    let mut header = [0; REQUEST_HEADER_LEN];
    header[..4].copy_from_slice(&MAGIC);
    header[4..12].copy_from_slice(&request.offset.to_le_bytes());
    header[12..].copy_from_slice(&request.length.to_le_bytes());
    Ok(header)
}

pub fn decode_request_header(header: [u8; REQUEST_HEADER_LEN]) -> io::Result<ReadRequest> {
    if header[..4] != MAGIC {
        return Err(invalid_data("attachment read request magic is invalid"));
    }
    let request = ReadRequest {
        offset: u64::from_le_bytes(header[4..12].try_into().expect("fixed request offset")),
        length: u32::from_le_bytes(header[12..].try_into().expect("fixed request length")),
    };
    request.validate()?;
    Ok(request)
}

pub fn encode_response_header(
    status: ResponseStatus,
    payload_len: usize,
) -> io::Result<[u8; RESPONSE_HEADER_LEN]> {
    validate_response_length(status, payload_len, super::MAX_ATTACHMENT_READ_BYTES)?;
    let payload_len = u32::try_from(payload_len)
        .map_err(|_| invalid_input("attachment read response length does not fit in u32"))?;
    let mut header = [0; RESPONSE_HEADER_LEN];
    header[0] = status as u8;
    header[4..].copy_from_slice(&payload_len.to_le_bytes());
    Ok(header)
}

pub fn decode_response_header(
    header: [u8; RESPONSE_HEADER_LEN],
) -> io::Result<(ResponseStatus, usize)> {
    let status = ResponseStatus::try_from(header[0])?;
    if header[1..4] != [0; 3] {
        return Err(invalid_data(
            "attachment read response reserved bytes are nonzero",
        ));
    }
    let payload_len =
        u32::from_le_bytes(header[4..].try_into().expect("fixed response length")) as usize;
    validate_response_length(status, payload_len, super::MAX_ATTACHMENT_READ_BYTES)?;
    Ok((status, payload_len))
}

pub fn read_request(reader: &mut impl Read) -> io::Result<Option<ReadRequest>> {
    let Some(header) = read_header::<REQUEST_HEADER_LEN>(reader)? else {
        return Ok(None);
    };
    decode_request_header(header).map(Some)
}

pub fn write_request(writer: &mut impl Write, request: ReadRequest) -> io::Result<()> {
    writer.write_all(&encode_request_header(request)?)
}

pub fn read_response(
    reader: &mut impl Read,
    requested_len: u32,
) -> io::Result<Option<ReadResponse>> {
    let requested_len = requested_len as usize;
    if requested_len == 0 || requested_len > super::MAX_ATTACHMENT_READ_BYTES {
        return Err(invalid_input("attachment read request length is invalid"));
    }
    let Some(header) = read_header::<RESPONSE_HEADER_LEN>(reader)? else {
        return Ok(None);
    };
    let (status, payload_len) = decode_response_header(header)?;
    validate_response_length(status, payload_len, requested_len)?;
    let mut payload = vec![0; payload_len];
    read_exact_retry(reader, &mut payload)?;
    Ok(Some(ReadResponse { status, payload }))
}

pub fn write_response(
    writer: &mut impl Write,
    status: ResponseStatus,
    payload: &[u8],
) -> io::Result<()> {
    let header = encode_response_header(status, payload.len())?;
    writer.write_all(&header)?;
    writer.write_all(payload)
}

fn validate_response_length(
    status: ResponseStatus,
    payload_len: usize,
    requested_len: usize,
) -> io::Result<()> {
    let limit = match status {
        ResponseStatus::Data => requested_len.min(super::MAX_ATTACHMENT_READ_BYTES),
        ResponseStatus::InvalidRequest
        | ResponseStatus::SourceChanged
        | ResponseStatus::IoFailure => super::MAX_STRING_BYTES,
    };
    if payload_len > limit {
        return Err(invalid_data(
            "attachment read response payload exceeds its limit",
        ));
    }
    Ok(())
}

fn read_header<const N: usize>(reader: &mut impl Read) -> io::Result<Option<[u8; N]>> {
    let mut header = [0; N];
    loop {
        match reader.read(&mut header[..1]) {
            Ok(0) => return Ok(None),
            Ok(1) => break,
            Ok(_) => unreachable!("one-byte destination"),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    read_exact_retry(reader, &mut header[1..])?;
    Ok(Some(header))
}

fn read_exact_retry(reader: &mut impl Read, mut bytes: &mut [u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        match reader.read(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated attachment read message",
                ));
            }
            Ok(read) => bytes = &mut bytes[read..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct ShortIo {
        bytes: Cursor<Vec<u8>>,
        maximum: usize,
    }

    impl Read for ShortIo {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let length = output.len().min(self.maximum);
            self.bytes.read(&mut output[..length])
        }
    }

    impl Write for ShortIo {
        fn write(&mut self, input: &[u8]) -> io::Result<usize> {
            self.bytes.write(&input[..input.len().min(self.maximum)])
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn request_and_response_survive_one_byte_io() {
        let request = ReadRequest {
            offset: 42,
            length: 7,
        };
        let mut wire = ShortIo {
            bytes: Cursor::new(Vec::new()),
            maximum: 1,
        };
        write_request(&mut wire, request).unwrap();
        write_response(&mut wire, ResponseStatus::Data, b"payload").unwrap();
        wire.bytes.set_position(0);
        assert_eq!(read_request(&mut wire).unwrap(), Some(request));
        assert_eq!(
            read_response(&mut wire, request.length).unwrap(),
            Some(ReadResponse {
                status: ResponseStatus::Data,
                payload: b"payload".to_vec(),
            })
        );
    }

    #[test]
    fn validates_request_bounds_before_writing() {
        for request in [
            ReadRequest {
                offset: 0,
                length: 0,
            },
            ReadRequest {
                offset: 0,
                length: super::super::MAX_ATTACHMENT_READ_BYTES as u32 + 1,
            },
            ReadRequest {
                offset: u64::MAX,
                length: 1,
            },
        ] {
            assert!(encode_request_header(request).is_err());
        }
    }

    #[test]
    fn rejects_bad_request_headers_and_truncation() {
        let mut bad_magic = encode_request_header(ReadRequest {
            offset: 0,
            length: 1,
        })
        .unwrap();
        bad_magic[0] = b'X';
        assert!(decode_request_header(bad_magic).is_err());

        let mut truncated = Cursor::new(b"CAR1".to_vec());
        let error = read_request(&mut truncated).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(read_request(&mut Cursor::new(Vec::new())).unwrap(), None);
    }

    #[test]
    fn rejects_bad_response_status_reserved_and_lengths_before_allocating() {
        let mut header = encode_response_header(ResponseStatus::Data, 1).unwrap();
        header[0] = 99;
        assert!(decode_response_header(header).is_err());

        let mut header = encode_response_header(ResponseStatus::Data, 1).unwrap();
        header[2] = 1;
        assert!(decode_response_header(header).is_err());

        let mut oversized = [0; RESPONSE_HEADER_LEN];
        oversized[4..]
            .copy_from_slice(&(super::super::MAX_ATTACHMENT_READ_BYTES as u32 + 1).to_le_bytes());
        assert!(decode_response_header(oversized).is_err());

        let mut response = Vec::new();
        write_response(&mut response, ResponseStatus::Data, b"1234").unwrap();
        assert!(read_response(&mut Cursor::new(response), 3).is_err());
    }

    #[test]
    fn accepts_eof_and_short_final_data_responses() {
        for payload in [&b""[..], &b"tail"[..]] {
            let mut wire = Vec::new();
            write_response(&mut wire, ResponseStatus::Data, payload).unwrap();
            assert_eq!(
                read_response(&mut Cursor::new(wire), 256).unwrap(),
                Some(ReadResponse {
                    status: ResponseStatus::Data,
                    payload: payload.to_vec(),
                })
            );
        }
    }

    #[test]
    fn rejects_truncated_response_payload() {
        let mut wire = Vec::new();
        write_response(&mut wire, ResponseStatus::Data, b"tail").unwrap();
        wire.pop();
        let error = read_response(&mut Cursor::new(wire), 4).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }
}
